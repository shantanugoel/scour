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

impl SearchResponse {
    pub(crate) fn has_answer(&self) -> bool {
        self.engines
            .iter()
            .any(|engine| matches!(engine.state, EngineState::Ok | EngineState::Empty))
    }
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
    Blocked,
    Limited,
    ParseFailed,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineStatus {
    pub name: String,
    pub state: EngineState,
    pub daily_count: u32,
    pub daily_limit: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub engines: Vec<EngineHealth>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineHealth {
    pub name: String,
    pub last_state: Option<EngineState>,
    pub daily_count: u32,
    pub daily_limit: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<String>,
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
        .route("/status", axum::routing::get(get_status))
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

async fn get_status(State(searcher): State<Arc<Searcher>>) -> Json<StatusResponse> {
    Json(searcher.status())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::{advance, pause, resume, sleep, timeout};

    use crate::config::Config;
    use crate::engines::{Engine, Hit, Page, bing, duckduckgo};
    use crate::limits::LimitSettings;

    const ACCEPT: &str = "application/json, text/event-stream";
    const SHORT_REFILL: LimitSettings = LimitSettings {
        burst: 5,
        refill: Duration::from_secs(30),
        daily: 200,
    };

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
        searcher_with_limits(base, LimitSettings::default())
    }

    fn searcher_with_limits(base: &'static str, limits: LimitSettings) -> Arc<Searcher> {
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
                limits,
                ..Config::default()
            })
            .expect("client"),
        )
    }

    async fn blocked_duckduckgo_with_bing(
        limits: LimitSettings,
    ) -> (Arc<Searcher>, Arc<AtomicUsize>) {
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        let app = axum::Router::new()
            .route(
                "/html/",
                axum::routing::get(|| async { StatusCode::FORBIDDEN }),
            )
            .route(
                "/search",
                axum::routing::get(move || {
                    counted.fetch_add(1, Ordering::SeqCst);
                    async {
                        r#"<ol id="b_results"><li class="b_algo"><h2>
                        <a href="https://tokio.rs/">Tokio</a></h2></li></ol>"#
                    }
                }),
            );
        let (base, _) = serve(app).await;
        let base = url::Url::parse(base).unwrap();
        let config = Config {
            engines: vec![&duckduckgo::ENGINE, &bing::ENGINE],
            base_urls: [("duckduckgo", base.clone()), ("bing", base)].into(),
            limits,
            ..Config::default()
        };
        (Arc::new(Searcher::new(&config).unwrap()), requests)
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
        let response = client()
            .get(url.as_str())
            .send()
            .await
            .expect("search request");
        assert_eq!(response.status().as_u16(), 200);
        response.json().await.expect("search answer")
    }

    async fn mcp_search(base: &str, query: &str) -> serde_json::Value {
        let (status, mut called) = rpc(
            base,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "search", "arguments": {"query": query}}
            }),
        )
        .await;
        assert_eq!(status, 200);
        called["result"].take()
    }

    async fn status_json(address: SocketAddr) -> serde_json::Value {
        let (status, body) = raw(address, &get("/status", &["localhost"], &[])).await;
        assert_eq!(status, 200);
        serde_json::from_str(&body).unwrap()
    }

    async fn reserve_next_pace_slot(
        searcher: &Arc<Searcher>,
        engine_index: usize,
    ) -> tokio::task::JoinHandle<SearchResponse> {
        let waiting = tokio::spawn({
            let searcher = searcher.clone();
            async move { searcher.search("wait").await }
        });
        timeout(Duration::from_secs(2), async {
            while searcher.status().engines[engine_index].daily_count != 6 {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the search should reserve the next pace slot");
        waiting
    }

    #[tokio::test]
    async fn default_pace_rejects_the_fourth_search_without_sending() {
        let (engine_base, requests) = engine(StatusCode::OK, "Tokio").await;
        let searcher = searcher(engine_base);
        let before = Utc::now();
        for count in 1..=3 {
            let response = searcher.search("burst").await;
            assert_eq!(response.engines[0].state, EngineState::Ok);
            assert_eq!(requests.load(Ordering::SeqCst), count);
        }
        let after = Utc::now();

        pause();
        let health = searcher.status();
        let response = timeout(Duration::from_secs(1), searcher.search("too soon"))
            .await
            .expect("a limited query should return without waiting for a token");
        let engine = &response.engines[0];
        assert_eq!(engine.state, EngineState::Limited);
        assert_eq!(engine.daily_count, 3);
        assert_eq!(engine.ready_at, health.engines[0].ready_at);
        let deadline = DateTime::parse_from_rfc3339(engine.ready_at.as_deref().unwrap()).unwrap();
        assert!(deadline > before + TimeDelta::seconds(119));
        assert!(deadline <= after + TimeDelta::seconds(120));
        assert_eq!(requests.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn configured_limits_apply_to_each_enabled_engine() {
        let app = axum::Router::new()
            .route(
                "/html/",
                axum::routing::get(|| async {
                    include_str!("../fixtures/duckduckgo/results.html")
                }),
            )
            .route(
                "/search",
                axum::routing::get(|| async { include_str!("../fixtures/bing/results.html") }),
            );
        let (engine_base, _) = serve(app).await;
        let path = std::env::temp_dir().join(format!("scour-limits-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            format!(
                r#"engines = ["duckduckgo", "bing"]
[limits]
burst = 1
refill_seconds = 60
daily = 2
[testing.base_urls]
duckduckgo = "{engine_base}"
bing = "{engine_base}"
"#
            ),
        )
        .unwrap();
        let config = crate::config::load(Some(&path));
        std::fs::remove_file(path).unwrap();
        let searcher = Arc::new(Searcher::new(&config.unwrap()).unwrap());
        let (base, address) = serve(router(searcher)).await;

        assert_eq!(
            status_json(address).await,
            serde_json::json!({"engines": [
                {"name": "duckduckgo", "last_state": null, "daily_count": 0, "daily_limit": 2},
                {"name": "bing", "last_state": null, "daily_count": 0, "daily_limit": 2}
            ]})
        );
        for (state, count) in [(EngineState::Ok, 1), (EngineState::Limited, 1)] {
            let response = search_json(base, "burst").await;
            for engine in response.engines {
                assert_eq!(engine.state, state, "{}", engine.name);
                assert_eq!(engine.daily_count, count);
                assert_eq!(engine.daily_limit, 2);
            }
        }

        pause();
        advance(Duration::from_secs(60)).await;
        resume();
        for engine in search_json(base, "refilled").await.engines {
            assert_eq!(engine.state, EngineState::Ok, "{}", engine.name);
            assert_eq!(engine.daily_count, 2);
        }

        let now = Utc::now();
        for engine in search_json(base, "daily limit").await.engines {
            assert_eq!(engine.state, EngineState::Limited, "{}", engine.name);
            assert_eq!(engine.daily_count, 2);
            let deadline =
                DateTime::parse_from_rfc3339(engine.ready_at.as_deref().unwrap()).unwrap();
            assert!(deadline > now + TimeDelta::hours(23));
        }
    }

    #[tokio::test]
    async fn configured_engine_skips_requests_until_its_block_rest_expires() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        let app = axum::Router::new().route(
            "/html/",
            axum::routing::get(move || {
                let index = counted.fetch_add(1, Ordering::SeqCst);
                async move {
                    if index == 0 {
                        include_str!("../fixtures/duckduckgo/blocked.html")
                    } else {
                        include_str!("../fixtures/duckduckgo/results.html")
                    }
                }
            }),
        );
        let (engine_base, _) = serve(app).await;
        let path = std::env::temp_dir().join(format!("scour-rest-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            format!("[testing]\nrest_seconds = 20\nbase_urls.duckduckgo = \"{engine_base}\"\n"),
        )
        .unwrap();
        let config = crate::config::load(Some(&path));
        std::fs::remove_file(path).unwrap();
        let searcher = Searcher::new(&config.unwrap()).unwrap();
        let (base, address) = serve(router(Arc::new(searcher))).await;

        assert_eq!(
            status_json(address).await,
            serde_json::json!({"engines": [{
                "name": "duckduckgo", "last_state": null, "daily_count": 0, "daily_limit": 100
            }]})
        );
        assert_eq!(requests.load(Ordering::SeqCst), 0);

        let before = Utc::now();
        let blocked = search_json(base, "rust async runtime").await;
        let after = Utc::now();
        assert_eq!(blocked.engines[0].state, EngineState::Blocked);
        assert_eq!(blocked.engines[0].daily_count, 1);
        assert_eq!(blocked.engines[0].daily_limit, 100);
        let ready_at = blocked.engines[0].ready_at.as_deref().unwrap();
        let deadline = DateTime::parse_from_rfc3339(ready_at).unwrap().to_utc();
        assert_eq!(
            ready_at,
            deadline.to_rfc3339_opts(SecondsFormat::Secs, true)
        );
        assert!(deadline > before + TimeDelta::seconds(19));
        assert!(deadline <= after + TimeDelta::seconds(20));
        let text = crate::cli::render(&blocked);
        assert!(text.contains(&format!("until {ready_at} (in ")), "{text}");
        assert!(text.ends_with(" s)\n"), "{text}");
        assert_eq!(requests.load(Ordering::SeqCst), 1);

        // Keep Tokio from auto-advancing the clock while the HTTP sockets are idle.
        pause();
        advance(Duration::from_secs(5)).await;
        resume();
        let resting = search_json(base, "second query").await;
        assert_eq!(resting.engines, blocked.engines);
        assert_eq!(
            status_json(address).await["engines"][0],
            serde_json::json!({
                "name": "duckduckgo", "last_state": "blocked", "ready_at": ready_at,
                "daily_count": 1, "daily_limit": 100
            })
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);

        pause();
        advance(Duration::from_secs(15)).await;
        resume();
        let health = &status_json(address).await["engines"][0];
        assert_eq!(health["last_state"], "blocked");
        assert!(health.get("ready_at").is_none());
        assert_eq!(requests.load(Ordering::SeqCst), 1);

        let resumed = search_json(base, "rust async runtime").await;
        assert_eq!(resumed.engines[0].state, EngineState::Ok);
        assert!(resumed.engines[0].ready_at.is_none());
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_blocked_engine_leaves_the_other_engines_results_available() {
        let (searcher, requests) = blocked_duckduckgo_with_bing(LimitSettings::default()).await;
        let (base, _) = serve(router(searcher)).await;
        let mut response = search_json(base, "rust").await;
        let [duckduckgo, bing] = response.engines.as_mut_slice() else {
            panic!("expected two engine states");
        };
        assert_eq!(duckduckgo.name, "duckduckgo");
        assert_eq!(duckduckgo.state, EngineState::Blocked);
        assert!(duckduckgo.ready_at.is_some());
        assert_eq!(bing.name, "bing");
        assert_eq!(bing.state, EngineState::Ok);
        assert_eq!(bing.daily_count, 1);
        assert_eq!(
            response.results,
            [ResultItem {
                url: "https://tokio.rs/".to_string(),
                title: "Tokio".to_string(),
                content: String::new(),
                engine: "bing".to_string(),
                position: 1,
            }]
        );

        let mcp = mcp_search(base, "rust").await;
        assert_eq!(mcp["isError"], false);
        bing.daily_count = 2;
        assert_eq!(
            mcp["structuredContent"],
            serde_json::to_value(response).unwrap()
        );
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn all_resting_engines_report_their_deadlines_and_no_answer() {
        let (searcher, requests) = blocked_duckduckgo_with_bing(SHORT_REFILL).await;
        let (base, address) = serve(router(searcher.clone())).await;
        for count in 1..=5 {
            let response = search_json(base, "burst").await;
            assert_eq!(response.engines[1].state, EngineState::Ok);
            assert_eq!(response.engines[1].daily_count, count);
            assert_eq!(requests.load(Ordering::SeqCst), count as usize);
        }
        assert!(
            status_json(address).await["engines"][1]
                .get("ready_at")
                .is_none()
        );
        assert_eq!(requests.load(Ordering::SeqCst), 5);

        let waiting = reserve_next_pace_slot(&searcher, 1).await;

        let health = &status_json(address).await["engines"];
        assert_eq!(health[1]["last_state"], "ok");
        assert_eq!(health[1]["daily_count"], 6);
        assert_eq!(requests.load(Ordering::SeqCst), 5);

        let response = search_json(base, "too soon").await;
        assert!(response.results.is_empty());
        let [duckduckgo, bing] = response.engines.as_slice() else {
            panic!("expected two engine states");
        };
        let mcp = mcp_search(base, "too soon").await;
        assert_eq!(mcp["isError"], true);
        assert_eq!(
            mcp["structuredContent"],
            serde_json::to_value(&response).unwrap()
        );
        let text = mcp["content"][0]["text"].as_str().unwrap();
        for (index, (engine, name, state, state_text, count)) in [
            (duckduckgo, "duckduckgo", EngineState::Blocked, "blocked", 1),
            (bing, "bing", EngineState::Limited, "limited", 6),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(engine.name, name);
            assert_eq!(engine.state, state);
            assert_eq!(engine.daily_count, count);
            assert_eq!(engine.daily_limit, 200);
            let ready_at = engine.ready_at.as_deref().unwrap();
            assert_eq!(health[index]["ready_at"], ready_at);
            assert!(
                text.contains(&format!(
                    "{name}: {state_text} ({count}/200 daily) until {ready_at} (in "
                )),
                "{text}"
            );
        }
        assert_eq!(
            status_json(address).await["engines"][1]["last_state"],
            "limited"
        );
        assert_eq!(requests.load(Ordering::SeqCst), 5);

        pause();
        advance(Duration::from_secs(30)).await;
        resume();
        let health = &status_json(address).await["engines"][1];
        assert!(health.get("ready_at").is_none());
        let sent = waiting.await.unwrap();
        assert_eq!(sent.engines[1].state, EngineState::Ok);
        assert_eq!(sent.engines[1].daily_count, 6);
        assert_eq!(requests.load(Ordering::SeqCst), 6);
    }

    #[tokio::test]
    async fn a_block_during_pace_sleep_refunds_the_waiting_search() {
        let requests = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let app = axum::Router::new().route(
            "/engine",
            axum::routing::get({
                let (requests, entered, release) =
                    (requests.clone(), entered.clone(), release.clone());
                move || {
                    let index = requests.fetch_add(1, Ordering::SeqCst);
                    let (entered, release) = (entered.clone(), release.clone());
                    async move {
                        if index == 0 {
                            entered.notify_one();
                            release.notified().await;
                            StatusCode::FORBIDDEN
                        } else {
                            StatusCode::NO_CONTENT
                        }
                    }
                }
            }),
        );
        let (engine_base, _) = serve(app).await;
        let searcher = searcher_with_limits(engine_base, SHORT_REFILL);
        let blocking = tokio::spawn({
            let searcher = searcher.clone();
            async move { searcher.search("block").await }
        });
        entered.notified().await;
        for _ in 0..4 {
            searcher.search("burst").await;
        }
        let waiting = reserve_next_pace_slot(&searcher, 0).await;
        assert_eq!(searcher.status().engines[0].daily_count, 6);
        release.notify_one();
        let blocking = blocking.await.unwrap();
        assert_eq!(blocking.engines[0].state, EngineState::Blocked);

        pause();
        advance(Duration::from_secs(30)).await;
        resume();
        let refunded = waiting.await.unwrap();
        assert_eq!(refunded.engines[0].state, EngineState::Blocked);
        assert_eq!(refunded.engines[0].ready_at, blocking.engines[0].ready_at);
        assert_eq!(refunded.engines[0].daily_count, 5);
        assert_eq!(requests.load(Ordering::SeqCst), 5);
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

        let mut reference = search_json(base, "rust async runtime").await;
        assert_eq!(reference.engines[0].daily_count, 1);

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
        reference.engines[0].daily_count = 2;
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
            let engine = &result["structuredContent"]["engines"][0];
            assert_eq!(engine["daily_count"], 1);
            assert_eq!(engine["daily_limit"], 100);
            if state == "blocked" {
                let ready_at = engine["ready_at"].as_str().expect("a rest deadline");
                assert!(text.contains(&format!("{ready_at} (in 10 min)")), "{text}");
            } else {
                assert!(engine.get("ready_at").is_none());
            }
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

        for path in ["/search?q=x", "/status", "/mcp", "/no-route"] {
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
        let (_, address) = serve(router(searcher_with_limits(engine_base, SHORT_REFILL))).await;
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
