use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::search::Searcher;

/// The daemon listens here. No flag, variable or file can change it.
pub const ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7268);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: String,
    pub results: Vec<ResultItem>,
    pub engines: Vec<EngineStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultItem {
    pub url: String,
    pub title: String,
    pub content: String,
    pub engine: String,
    pub position: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineState {
    Ok,
    Empty,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineStatus {
    pub name: String,
    pub state: EngineState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchParams {
    q: Option<String>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
}

pub fn router(searcher: Arc<Searcher>) -> axum::Router {
    axum::Router::new()
        .route("/search", axum::routing::get(get_search))
        .route_service("/mcp", crate::mcp::service(searcher.clone()))
        .with_state(searcher)
        .layer(axum::middleware::from_fn(guard_local))
}

/// A web page can point its own name at this address (DNS rebinding).
/// Only a local `Host` and a local or absent `Origin` reach a route.
async fn guard_local(request: Request, next: Next) -> Response {
    if is_local(&request) {
        return next.run(request).await;
    }
    (
        StatusCode::FORBIDDEN,
        Json(ErrorBody {
            error: "the Host or Origin is not local",
        }),
    )
        .into_response()
}

fn is_local(request: &Request) -> bool {
    let headers = request.headers();
    let mut hosts = headers.get_all(header::HOST).iter();
    let mut origins = headers.get_all(header::ORIGIN).iter();
    hosts
        .next()
        .is_some_and(|value| value.to_str().is_ok_and(is_local_address))
        && hosts.next().is_none()
        && origins
            .next()
            .is_none_or(|value| value.to_str().is_ok_and(is_local_origin))
        && origins.next().is_none()
}

fn is_local_address(address: &str) -> bool {
    let (host, port) = address
        .split_once(':')
        .map_or((address, None), |(host, port)| (host, Some(port)));
    (host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost"))
        && port.is_none_or(|port| {
            port.bytes().all(|byte| byte.is_ascii_digit()) && port.parse::<u16>().is_ok()
        })
}

fn is_local_origin(origin: &str) -> bool {
    origin.split_once("://").is_some_and(|(scheme, address)| {
        scheme.eq_ignore_ascii_case("http") && is_local_address(address)
    })
}

async fn get_search(
    State(searcher): State<Arc<Searcher>>,
    Query(params): Query<SearchParams>,
) -> Response {
    let Some(query) = params.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: "the query parameter q is missing",
            }),
        )
            .into_response();
    };
    let body = searcher.search(query).await;
    (StatusCode::OK, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::config::Config;
    use crate::engines::{Engine, Hit, Page};

    const ACCEPT: &str = "application/json, text/event-stream";

    async fn serve(app: axum::Router) -> (&'static str, SocketAddr) {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (
            Box::leak(format!("http://{address}").into_boxed_str()),
            address,
        )
    }

    /// An engine page on this machine. The counter proves that a request went out.
    async fn engine(status: StatusCode, body: &'static str) -> (&'static str, Arc<AtomicUsize>) {
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        let app = axum::Router::new().route(
            "/engine",
            axum::routing::get(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                async move { (status, body) }
            }),
        );
        let (base, _) = serve(app).await;
        (base, requests)
    }

    fn engine_url(base: &url::Url, query: &str) -> url::Url {
        let mut url = base.join("/engine").unwrap();
        url.query_pairs_mut().append_pair("q", query);
        url
    }

    fn read_page(_status: u16, body: &str) -> Page {
        let body = body.trim();
        match body {
            "" => return Page::Empty,
            "block page" => return Page::Blocked,
            "changed markup" => return Page::ParseFailed,
            _ => {}
        }
        Page::Results(vec![Hit {
            url: "https://tokio.rs/".to_string(),
            title: "Tokio".to_string(),
            content: body.to_string(),
        }])
    }

    fn searcher(base: &'static str) -> Arc<Searcher> {
        let engine = Box::leak(Box::new(Engine {
            name: "test",
            base_url: base,
            profile: wreq_util::Profile::Chrome149,
            request_url: engine_url,
            read: read_page,
        }));
        Arc::new(
            Searcher::new(&Config {
                engines: vec![engine],
            })
            .expect("client"),
        )
    }

    fn client() -> wreq::Client {
        wreq::Client::builder()
            .timeout(Duration::from_secs(10))
            .no_proxy()
            .build()
            .expect("client")
    }

    /// One plain JSON-RPC call. No MCP client library is involved.
    async fn rpc(base: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let response = client()
            .post(format!("{base}/mcp"))
            .header("content-type", "application/json")
            .header("accept", ACCEPT)
            .json(&body)
            .send()
            .await
            .expect("mcp request");
        let status = response.status().as_u16();
        let text = response.text().await.expect("body");
        let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    async fn search_json(base: &str, query: &str) -> SearchResponse {
        let mut url = url::Url::parse(&format!("{base}/search")).unwrap();
        url.query_pairs_mut().append_pair("q", query);
        let text = client()
            .get(url.as_str())
            .send()
            .await
            .expect("search request")
            .text()
            .await
            .expect("body");
        serde_json::from_str(&text).expect("search answer")
    }

    #[tokio::test]
    async fn a_plain_json_rpc_client_lists_and_calls_the_search_tool() {
        let (engine_base, engine_requests) = engine(StatusCode::OK, "Tokio").await;
        let (base, _) = serve(router(searcher(engine_base))).await;

        let (status, initialized) = rpc(
            base,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "plain-json-rpc", "version": "1.0"}
                }
            }),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(initialized["result"]["serverInfo"]["name"], "scour");

        let (status, _) = rpc(
            base,
            serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;
        assert_eq!(status, 202);

        let (status, listed) = rpc(
            base,
            serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
        )
        .await;
        assert_eq!(status, 200);
        let tools = listed["result"]["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "search");
        assert_eq!(
            tools[0]["inputSchema"]["properties"]["query"]["type"],
            "string"
        );
        let description = tools[0]["description"].as_str().expect("description");
        assert!(description.contains("untrusted"));

        let reference = search_json(base, "rust async runtime").await;

        let (status, called) = rpc(
            base,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {"name": "search", "arguments": {"query": "rust async runtime"}}
            }),
        )
        .await;
        assert_eq!(status, 200);
        let result = &called["result"];
        assert_eq!(result["isError"], false);
        assert_eq!(
            result["structuredContent"],
            serde_json::to_value(&reference).unwrap()
        );
        assert_eq!(
            result["content"][0]["text"].as_str().unwrap(),
            crate::cli::render(&reference)
        );
        assert_eq!(engine_requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_blank_query_is_a_tool_error_without_an_engine_request() {
        let (engine_base, engine_requests) = engine(StatusCode::OK, "Tokio").await;
        let (base, _) = serve(router(searcher(engine_base))).await;

        let (status, called) = rpc(
            base,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "search", "arguments": {"query": "   "}}
            }),
        )
        .await;
        assert_eq!(status, 200);
        let result = &called["result"];
        assert_eq!(result["isError"], true);
        assert!(!result["content"][0]["text"].as_str().unwrap().is_empty());
        assert!(result.get("structuredContent").is_none());
        assert_eq!(engine_requests.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_empty_engine_answer_succeeds_and_a_failed_answer_is_a_tool_error() {
        let cases = [
            (
                StatusCode::OK,
                "",
                "empty",
                false,
                "test: empty (0 results)",
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "",
                "error",
                true,
                "test: error: HTTP 503",
            ),
            (
                StatusCode::TOO_MANY_REQUESTS,
                "",
                "blocked",
                true,
                "test: blocked",
            ),
            (
                StatusCode::ACCEPTED,
                "block page",
                "blocked",
                true,
                "test: blocked",
            ),
            (
                StatusCode::OK,
                "changed markup",
                "parse_failed",
                true,
                "test: parse_failed",
            ),
        ];
        for (engine_status, body, state, is_error, expected_text) in cases {
            let (engine_base, engine_requests) = engine(engine_status, body).await;
            let (base, _) = serve(router(searcher(engine_base))).await;

            let (status, called) = rpc(
                base,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {"name": "search", "arguments": {"query": "rust async runtime"}}
                }),
            )
            .await;
            assert_eq!(status, 200);
            let result = &called["result"];
            assert_eq!(result["structuredContent"]["engines"][0]["state"], state);
            assert_eq!(
                result["structuredContent"]["results"],
                serde_json::json!([])
            );
            assert_eq!(result["isError"], is_error, "engine state {state}");
            let text = result["content"][0]["text"].as_str().unwrap();
            assert!(text.contains(expected_text), "{text}");
            assert_eq!(engine_requests.load(Ordering::SeqCst), 1);
        }
    }

    async fn raw(address: SocketAddr, request: &str) -> (u16, String) {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").expect("a response head");
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .and_then(|code| code.parse().ok())
            .expect("a status code");
        (status, body.to_string())
    }

    fn get(path: &str, hosts: &[&str], origins: &[&str]) -> String {
        let mut request = format!("GET {path} HTTP/1.1\r\nConnection: close\r\n");
        for host in hosts {
            request.push_str(&format!("Host: {host}\r\n"));
        }
        for origin in origins {
            request.push_str(&format!("Origin: {origin}\r\n"));
        }
        request.push_str("\r\n");
        request
    }

    #[tokio::test]
    async fn a_foreign_host_or_origin_is_forbidden_on_every_route() {
        let (engine_base, engine_requests) = engine(StatusCode::OK, "Tokio").await;
        let (_, address) = serve(router(searcher(engine_base))).await;
        let local = "127.0.0.1";
        let origin = "http://127.0.0.1";
        let cases: &[(&[&str], &[&str])] = &[
            (&[], &[]),
            (&["evil.example"], &[]),
            (&["evil.example@127.0.0.1"], &[]),
            (&["127.0.0.1:"], &[]),
            (&["localhost:evil.example"], &[]),
            (&["localhost:+80"], &[]),
            (&["localhost:65536"], &[]),
            (&[local, local], &[]),
            (&[local], &["http://evil.example"]),
            (&[local], &["http://evil.example@127.0.0.1"]),
            (&[local], &["http://127.0.0.1:"]),
            (&[local], &["http://localhost:evil.example"]),
            (&[local], &["http://localhost:80/path"]),
            (&[local], &["https://127.0.0.1"]),
            (&[local], &["null"]),
            (&[local], &[origin, "http://evil.example"]),
            (&[local], &[origin, origin]),
        ];

        for path in ["/search?q=x", "/mcp", "/no-route"] {
            for &(hosts, origins) in cases {
                let (status, body) = raw(address, &get(path, hosts, origins)).await;
                assert_eq!(
                    status, 403,
                    "Host {hosts:?} and Origin {origins:?} on {path}"
                );
                assert_eq!(body, r#"{"error":"the Host or Origin is not local"}"#);
            }
        }
        assert_eq!(engine_requests.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_local_host_on_any_port_and_no_origin_or_a_local_origin_pass() {
        let (engine_base, engine_requests) = engine(StatusCode::OK, "Tokio").await;
        let (_, address) = serve(router(searcher(engine_base))).await;
        let local = format!("127.0.0.1:{}", address.port());
        let local_name = format!("localhost:{}", address.port());
        let local_origin = format!("http://{local}");
        let cases = [
            (local.as_str(), None),
            (local_name.as_str(), None),
            (local.as_str(), Some(local_origin.as_str())),
            ("localhost:1234", Some("http://localhost:5173")),
            ("127.0.0.1", Some("http://127.0.0.1")),
        ];
        for (host, origin) in &cases {
            let (status, body) =
                raw(address, &get("/search?q=x", &[host], origin.as_slice())).await;
            assert_eq!(status, 200, "Host {host} and Origin {origin:?}: {body}");
        }
        assert_eq!(engine_requests.load(Ordering::SeqCst), cases.len());

        let (status, _) = raw(address, &get("/no-route", &[&local], &[])).await;
        assert_eq!(status, 404);

        let (status, _) = raw(address, &get("/mcp", &[&local], &[])).await;
        assert_eq!(status, 405);
    }
}
