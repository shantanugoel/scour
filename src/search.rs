use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until};
use url::Url;

use crate::api::{
    EngineHealth, EngineState, EngineStatus, ResultItem, SearchResponse, StatusResponse,
};
use crate::config::Config;
use crate::engines::{Engine, Hit, Page};
use crate::limits::{Admit, Limits};

const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Searcher {
    client: wreq::Client,
    engines: Vec<EngineSlot>,
    daily_limit: u32,
}

struct EngineSlot {
    engine: &'static Engine,
    base_url: Url,
    limits: Mutex<Limits>,
}

struct Answer {
    hits: Option<Vec<Hit>>,
    state: EngineState,
    error: Option<String>,
}

impl Answer {
    fn without_hits(state: EngineState) -> Self {
        Self {
            hits: None,
            state,
            error: None,
        }
    }

    fn error(message: String) -> Self {
        Self {
            hits: None,
            state: EngineState::Error,
            error: Some(message),
        }
    }
}

impl Searcher {
    pub fn new(config: &Config) -> wreq::Result<Self> {
        let builder = http_client_builder();
        let builder = match &config.proxy {
            Some(url) => {
                let proxy = wreq::Proxy::all(url.as_str()).map_err(wreq::Error::without_uri)?;
                builder.proxy(proxy)
            }
            None => builder.no_proxy(),
        };
        let client = builder.build().map_err(wreq::Error::without_uri)?;
        Ok(Self::with_client(config, client))
    }

    fn with_client(config: &Config, client: wreq::Client) -> Self {
        let engines = config
            .engines
            .iter()
            .map(|engine| EngineSlot {
                engine,
                base_url: config
                    .base_urls
                    .get(engine.name)
                    .cloned()
                    .unwrap_or_else(|| Url::parse(engine.base_url).expect("engine base URL")),
                limits: Mutex::new(Limits::new(config.rest, config.limits)),
            })
            .collect();
        Self {
            client,
            engines,
            daily_limit: config.limits.daily,
        }
    }

    pub async fn search(self: &Arc<Self>, query: &str) -> SearchResponse {
        let query = Arc::<str>::from(query);
        let mut tasks = JoinSet::new();
        for index in 0..self.engines.len() {
            let searcher = self.clone();
            let query = query.clone();
            tasks.spawn(async move {
                let slot = &searcher.engines[index];
                let (answer, ready_at) = searcher.search_engine(slot, &query).await;
                let status = EngineStatus {
                    name: slot.engine.name.to_string(),
                    state: answer.state,
                    daily_count: slot
                        .limits
                        .lock()
                        .expect("engine limits")
                        .daily_count(Instant::now()),
                    daily_limit: searcher.daily_limit,
                    error: answer.error,
                    ready_at: ready_at.map(format_deadline),
                };
                (index, status, answer.hits.unwrap_or_default().into_iter())
            });
        }
        let mut answers = tasks.join_all().await;
        answers.sort_unstable_by_key(|(index, _, _)| *index);
        let (engines, mut hits): (Vec<_>, Vec<_>) = answers
            .into_iter()
            .map(|(_, status, hits)| (status, hits))
            .unzip();

        let mut results = Vec::new();
        let mut seen = HashSet::new();
        let ranks = hits.iter().map(ExactSizeIterator::len).max().unwrap_or(0);
        for _ in 0..ranks {
            for (engine, hits) in engines.iter().zip(&mut hits) {
                let Some(hit) = hits.next() else { continue };
                if seen.insert(hit.url.clone()) {
                    results.push(ResultItem {
                        url: hit.url,
                        title: hit.title,
                        content: hit.content,
                        engine: engine.name.clone(),
                        position: (results.len() + 1) as u32,
                    });
                }
            }
        }
        SearchResponse {
            query: query.to_string(),
            results,
            engines,
        }
    }

    pub fn status(&self) -> StatusResponse {
        let engines = self
            .engines
            .iter()
            .map(|slot| {
                let limits = slot.limits.lock().expect("engine limits");
                let now = Instant::now();
                EngineHealth {
                    name: slot.engine.name.to_string(),
                    last_state: limits.last_state(),
                    daily_count: limits.daily_count(now),
                    daily_limit: self.daily_limit,
                    ready_at: limits.peek(now).map(format_deadline),
                }
            })
            .collect();
        StatusResponse { engines }
    }

    async fn search_engine(
        &self,
        slot: &EngineSlot,
        query: &str,
    ) -> (Answer, Option<DateTime<Utc>>) {
        let (now, wall, admit) = {
            let mut limits = slot.limits.lock().expect("engine limits");
            let now = Instant::now();
            let wall = Utc::now();
            (now, wall, limits.admit(now, wall))
        };
        let send_at = match admit {
            Admit::Resting(ready_at) => {
                return (Answer::without_hits(EngineState::Blocked), Some(ready_at));
            }
            Admit::Limited(ready_at) => {
                return (Answer::without_hits(EngineState::Limited), Some(ready_at));
            }
            Admit::SendAt(send_at) => send_at,
        };
        if send_at > now {
            sleep_until(send_at).await;
            let ready_at = slot
                .limits
                .lock()
                .expect("engine limits")
                .confirm_send(Instant::now(), (now, wall));
            if ready_at.is_some() {
                return (Answer::without_hits(EngineState::Blocked), ready_at);
            }
        }
        let answer = self.fetch(slot, query).await;
        let ready_at = slot.limits.lock().expect("engine limits").record(
            answer.state,
            Instant::now(),
            Utc::now(),
        );
        (answer, ready_at)
    }

    async fn fetch(&self, slot: &EngineSlot, query: &str) -> Answer {
        let engine = slot.engine;
        let host = slot.base_url.host_str().unwrap_or(engine.base_url);
        let url = (engine.request_url)(&slot.base_url, query);
        match self
            .client
            .get(url.as_str())
            .emulation(engine.profile)
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status().as_u16();
                if matches!(status, 403 | 429) {
                    return Answer::without_hits(EngineState::Blocked);
                }
                match response.text().await {
                    Ok(body) => answer_for_page(engine, status, &body),
                    Err(_) if status != 200 => Answer::error(format!("HTTP {status}")),
                    Err(err) => Answer::error(fetch_error(&err, host)),
                }
            }
            Err(err) => Answer::error(fetch_error(&err, host)),
        }
    }
}

fn format_deadline(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn answer_for_page(engine: &Engine, status: u16, body: &str) -> Answer {
    match (engine.read)(status, body) {
        Page::Blocked => Answer::without_hits(EngineState::Blocked),
        _ if status != 200 => Answer::error(format!("HTTP {status}")),
        Page::Results(hits) => Answer {
            hits: Some(hits),
            state: EngineState::Ok,
            error: None,
        },
        Page::Empty => Answer::without_hits(EngineState::Empty),
        Page::ParseFailed => Answer::without_hits(EngineState::ParseFailed),
    }
}

pub fn http_client_builder() -> wreq::ClientBuilder {
    // A Bing answer can be a redirect.
    wreq::Client::builder()
        .timeout(TIMEOUT)
        .redirect(wreq::redirect::Policy::limited(10))
        .dns_resolver(Ipv4Only)
}

fn fetch_error(err: &wreq::Error, host: &str) -> String {
    // The wreq error can include the query or proxy details.
    if err.is_timeout() {
        format!("no answer from {host} in {} s", TIMEOUT.as_secs())
    } else {
        format!("cannot connect to {host}")
    }
}

struct Ipv4Only;

impl wreq::dns::Resolve for Ipv4Only {
    fn resolve(&self, name: wreq::dns::Name) -> wreq::dns::Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let looked_up = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(box_error)?;
            let addrs: Vec<SocketAddr> = looked_up.filter(|addr| addr.is_ipv4()).collect();
            if addrs.is_empty() {
                return Err(box_error(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no IPv4 address",
                )));
            }
            let iter: wreq::dns::Addrs = Box::new(addrs.into_iter());
            Ok(iter)
        })
    }
}

fn box_error(
    err: impl std::error::Error + Send + Sync + 'static,
) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::{bing, duckduckgo};
    use std::net::Ipv4Addr;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpSocket};
    use tokio::time::timeout;

    fn proxy_config(scheme: &str, address: SocketAddr, userinfo: &str) -> Config {
        let path = std::env::temp_dir().join(format!(
            "scour-proxy-{}-{}.toml",
            std::process::id(),
            address.port()
        ));
        std::fs::write(
            &path,
            format!("proxy = \"{scheme}://{userinfo}{address}\"\n"),
        )
        .unwrap();
        let config = crate::config::load(Some(&path));
        std::fs::remove_file(path).unwrap();
        config.unwrap()
    }

    #[tokio::test]
    async fn http_proxy_receives_connect_for_duckduckgo() {
        let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = proxy.local_addr().unwrap();
        let config = proxy_config("http", address, "fixture-user:fixture-password@");
        let searcher = Arc::new(Searcher::new(&config).unwrap());

        timeout(Duration::from_secs(3), async {
            tokio::join!(searcher.search("offline"), async {
                let (stream, _) = proxy.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut request = String::new();
                while !request.ends_with("\r\n\r\n") {
                    assert_ne!(stream.read_line(&mut request).await.unwrap(), 0);
                }
                assert_eq!(
                    request.lines().next(),
                    Some("CONNECT html.duckduckgo.com:443 HTTP/1.1")
                );
                use base64::Engine as _;
                let credentials = base64::engine::general_purpose::STANDARD
                    .encode("fixture-user:fixture-password");
                let auth = format!("Basic {credentials}");
                assert!(request.lines().any(|line| {
                    line.split_once(": ").is_some_and(|(name, value)| {
                        name.eq_ignore_ascii_case("proxy-authorization") && value == auth
                    })
                }));
                stream
                    .get_mut()
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                    .await
                    .unwrap();
            })
        })
        .await
        .expect("HTTP proxy did not receive CONNECT");
    }

    #[tokio::test]
    async fn socks5_resolves_ipv4_locally() {
        check_socks_address("socks5", &[0x01, 127, 0, 0, 1]).await;
    }

    #[tokio::test]
    async fn socks5h_sends_the_hostname_to_the_proxy() {
        check_socks_address("socks5h", b"\x03\x09localhost").await;
    }

    async fn check_socks_address(scheme: &str, address: &[u8]) {
        let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let port = proxy_addr.port();
        let mut config = proxy_config(scheme, proxy_addr, "");
        config.base_urls.insert(
            "duckduckgo",
            Url::parse(&format!("http://localhost:{port}")).unwrap(),
        );
        let searcher = Arc::new(Searcher::new(&config).unwrap());
        timeout(Duration::from_secs(3), async {
            tokio::join!(searcher.search("offline"), async {
                let (mut stream, _) = proxy.accept().await.unwrap();
                let mut greeting = [0; 3];
                stream.read_exact(&mut greeting).await.unwrap();
                assert_eq!(greeting, [5, 1, 0]);
                stream.write_all(&[5, 0]).await.unwrap();

                let mut request = vec![0; 3 + address.len() + 2];
                stream.read_exact(&mut request[..4]).await.unwrap();
                assert_eq!(&request[..4], &[5, 1, 0, address[0]]);
                stream.read_exact(&mut request[4..]).await.unwrap();
                assert_eq!(&request[3..3 + address.len()], address);
                assert_eq!(&request[3 + address.len()..], &port.to_be_bytes());
                stream
                    .write_all(&[5, 1, 0, 1, 0, 0, 0, 0, 0, 0])
                    .await
                    .unwrap();
            })
        })
        .await
        .expect("SOCKS proxy did not receive the request");
    }

    #[tokio::test]
    async fn unreachable_proxy_reports_error_without_direct_fallback() {
        let proxy = TcpSocket::new_v4().unwrap();
        proxy.bind((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let mut config = proxy_config("http", proxy_addr, "fixture-user:fixture-password@");
        config.base_urls.insert(
            "duckduckgo",
            Url::parse(&format!("http://{target_addr}")).unwrap(),
        );
        let searcher = Arc::new(Searcher::new(&config).unwrap());

        let response = tokio::select! {
            biased;
            result = target.accept() => panic!("direct connection to the engine: {result:?}"),
            response = searcher.search("offline") => response,
        };
        assert_eq!(response.engines[0].state, EngineState::Error);
        assert_eq!(
            response.engines[0].error.as_deref(),
            Some("cannot connect to 127.0.0.1")
        );
    }

    /// DNS that fails before any connection, so the test makes no request.
    struct FailResolve;

    impl wreq::dns::Resolve for FailResolve {
        fn resolve(&self, _name: wreq::dns::Name) -> wreq::dns::Resolving {
            Box::pin(async {
                Err(box_error(std::io::Error::new(
                    std::io::ErrorKind::HostUnreachable,
                    "no address",
                )))
            })
        }
    }

    fn test_client() -> wreq::Client {
        wreq::Client::builder()
            .timeout(Duration::from_secs(2))
            .redirect(wreq::redirect::Policy::none())
            .no_proxy()
            .build()
            .unwrap()
    }

    fn test_searcher(client: wreq::Client, engines: Vec<&'static Engine>) -> Arc<Searcher> {
        Arc::new(Searcher::with_client(
            &Config {
                engines,
                ..Config::default()
            },
            client,
        ))
    }

    async fn serve(app: axum::Router) -> &'static str {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Box::leak(format!("http://{address}").into_boxed_str())
    }

    fn leaked(engine: Engine) -> &'static Engine {
        Box::leak(Box::new(engine))
    }

    fn first_url(base: &url::Url, query: &str) -> url::Url {
        let mut url = base.join("/first").unwrap();
        url.query_pairs_mut().append_pair("q", query);
        url
    }

    fn second_url(base: &url::Url, query: &str) -> url::Url {
        let mut url = base.join("/second").unwrap();
        url.query_pairs_mut().append_pair("q", query);
        url
    }

    fn fail_url(base: &url::Url, query: &str) -> url::Url {
        let mut url = base.join("/fail").unwrap();
        url.query_pairs_mut().append_pair("q", query);
        url
    }

    fn empty_url(base: &url::Url, query: &str) -> url::Url {
        let mut url = base.join("/empty").unwrap();
        url.query_pairs_mut().append_pair("q", query);
        url
    }

    fn read_body(_status: u16, body: &str) -> Page {
        Page::Results(vec![Hit {
            url: format!("https://{}.example/", body.trim()),
            title: body.trim().to_string(),
            content: String::new(),
        }])
    }

    fn read_empty(_status: u16, _body: &str) -> Page {
        Page::Empty
    }

    #[tokio::test]
    async fn engines_run_concurrently_and_report_in_config_order() {
        let arrived = Arc::new(tokio::sync::Barrier::new(2));
        let release_first = Arc::new(tokio::sync::Notify::new());
        let app = axum::Router::new()
            .route(
                "/first",
                axum::routing::get({
                    let arrived = arrived.clone();
                    let release_first = release_first.clone();
                    move || async move {
                        arrived.wait().await;
                        release_first.notified().await;
                        "first"
                    }
                }),
            )
            .route(
                "/second",
                axum::routing::get(move || async move {
                    arrived.wait().await;
                    "second"
                }),
            );
        let base = serve(app).await;
        let engines = vec![
            leaked(Engine {
                name: "first",
                base_url: base,
                profile: wreq_util::Profile::Chrome149,
                request_url: first_url,
                read: read_body,
            }),
            leaked(Engine {
                name: "second",
                base_url: base,
                profile: wreq_util::Profile::Chrome149,
                request_url: second_url,
                read: read_body,
            }),
        ];
        let searcher = test_searcher(test_client(), engines);
        let (response, ()) = tokio::join!(searcher.search("rust async runtime"), async {
            tokio::time::timeout(Duration::from_secs(2), async {
                while searcher.status().engines[1].last_state != Some(EngineState::Ok) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("the second engine must finish while the first is waiting");
            release_first.notify_one();
        });
        assert_eq!(response.query, "rust async runtime");
        assert_eq!(response.engines.len(), 2);
        assert_eq!(response.engines[0].name, "first");
        assert_eq!(response.engines[0].state, EngineState::Ok);
        assert_eq!(response.engines[1].name, "second");
        assert_eq!(response.engines[1].state, EngineState::Ok);
        assert_eq!(response.results.len(), 2);
        assert_eq!(response.results[0].position, 1);
        assert_eq!(response.results[0].engine, "first");
        assert_eq!(response.results[0].title, "first");
        assert_eq!(response.results[1].position, 2);
        assert_eq!(response.results[1].engine, "second");
        assert_eq!(response.results[1].title, "second");
    }

    #[tokio::test]
    async fn results_merge_by_rank_with_exact_url_deduplication() {
        let app = axum::Router::new()
            .route(
                "/search",
                axum::routing::get(|| async {
                    r#"<ol id="b_results">
                    <li class="b_algo"><h2><a href="https://one.example/">Bing original</a></h2></li>
                    <li class="b_algo"><h2><a href="https://two.example/">Bing second</a></h2></li>
                    </ol>"#
                }),
            )
            .route(
                "/html/",
                axum::routing::get(|| async {
                    r#"<div class="result"><a class="result__a" href="https://three.example/">DuckDuckGo first</a></div>
                    <div class="result"><a class="result__a" href="https://one.example/">Duplicate</a></div>
                    <div class="result"><a class="result__a" href="https://one.example/?lang=en">Distinct URL</a></div>"#
                }),
            );
        let base = Url::parse(serve(app).await).unwrap();
        let searcher = Arc::new(
            Searcher::new(&Config {
                engines: vec![&bing::ENGINE, &duckduckgo::ENGINE],
                base_urls: [("bing", base.clone()), ("duckduckgo", base)].into(),
                ..Config::default()
            })
            .unwrap(),
        );
        let response = searcher.search("x").await;

        let results: Vec<_> = response
            .results
            .iter()
            .map(|hit| {
                (
                    hit.position,
                    hit.engine.as_str(),
                    hit.url.as_str(),
                    hit.title.as_str(),
                )
            })
            .collect();
        assert_eq!(
            results,
            [
                (1, "bing", "https://one.example/", "Bing original"),
                (
                    2,
                    "duckduckgo",
                    "https://three.example/",
                    "DuckDuckGo first"
                ),
                (3, "bing", "https://two.example/", "Bing second"),
                (
                    4,
                    "duckduckgo",
                    "https://one.example/?lang=en",
                    "Distinct URL"
                ),
            ]
        );
    }

    #[tokio::test]
    async fn one_failed_engine_does_not_stop_the_other() {
        let app = axum::Router::new()
            .route(
                "/fail",
                axum::routing::get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
            )
            .route("/second", axum::routing::get(|| async { "second" }));
        let base = serve(app).await;
        let engines = vec![
            leaked(Engine {
                name: "failing",
                base_url: base,
                profile: wreq_util::Profile::Chrome149,
                request_url: fail_url,
                read: read_body,
            }),
            leaked(Engine {
                name: "working",
                base_url: base,
                profile: wreq_util::Profile::Chrome149,
                request_url: second_url,
                read: read_body,
            }),
        ];
        let searcher = test_searcher(test_client(), engines);
        let response = searcher.search("x").await;
        assert_eq!(response.engines[0].state, EngineState::Error);
        assert_eq!(response.engines[0].error.as_deref(), Some("HTTP 503"));
        assert_ne!(response.engines[0].state, EngineState::Empty);
        assert_eq!(response.engines[1].state, EngineState::Ok);
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].engine, "working");
        assert_eq!(response.results[0].position, 1);
    }

    #[tokio::test]
    async fn page_without_results_reports_empty() {
        let app = axum::Router::new().route("/empty", axum::routing::get(|| async { "" }));
        let base = serve(app).await;
        let engines = vec![leaked(Engine {
            name: "empty",
            base_url: base,
            profile: wreq_util::Profile::Chrome149,
            request_url: empty_url,
            read: read_empty,
        })];
        let searcher = test_searcher(test_client(), engines);
        let response = searcher.search("x").await;
        assert!(response.results.is_empty());
        assert_eq!(response.engines[0].state, EngineState::Empty);
        assert!(response.engines[0].error.is_none());
    }

    #[tokio::test]
    async fn http_403_and_429_from_any_engine_are_blocked() {
        for status in [
            axum::http::StatusCode::FORBIDDEN,
            axum::http::StatusCode::TOO_MANY_REQUESTS,
        ] {
            // The reader would find a result in this body, so only the status can give `blocked`.
            let app = axum::Router::new().route(
                "/first",
                axum::routing::get(move || async move { (status, "first") }),
            );
            let base = serve(app).await;
            let searcher = test_searcher(
                test_client(),
                vec![leaked(Engine {
                    name: "any",
                    base_url: base,
                    profile: wreq_util::Profile::Chrome149,
                    request_url: first_url,
                    read: read_body,
                })],
            );
            let response = searcher.search("x").await;
            assert_eq!(response.engines[0].state, EngineState::Blocked, "{status}");
            assert!(response.results.is_empty(), "{status}");
        }
    }

    #[tokio::test]
    async fn saved_duckduckgo_pages_report_their_state() {
        // See the tests in `engines::duckduckgo` for how each saved page was made.
        let results = include_str!("../fixtures/duckduckgo/results.html");
        let empty = include_str!("../fixtures/duckduckgo/empty.html");
        let blocked = include_str!("../fixtures/duckduckgo/blocked.html");
        let changed: &'static str = Box::leak(
            results
                .replace(r#"class="result__a""#, r#"class="result__link""#)
                .into(),
        );
        let cases = [
            (200, results, EngineState::Ok),
            (200, empty, EngineState::Empty),
            (200, changed, EngineState::ParseFailed),
            (202, blocked, EngineState::Blocked),
            (200, blocked, EngineState::Blocked),
        ];
        for (status, page, state) in cases {
            let status = axum::http::StatusCode::from_u16(status).unwrap();
            let app = axum::Router::new().route(
                "/html/",
                axum::routing::get(move || async move { (status, axum::response::Html(page)) }),
            );
            let base = serve(app).await;
            let searcher = test_searcher(
                test_client(),
                vec![leaked(Engine {
                    base_url: base,
                    ..duckduckgo::ENGINE
                })],
            );
            let response = searcher.search("rust async runtime").await;
            assert_eq!(response.engines[0].name, "duckduckgo");
            assert_eq!(response.engines[0].state, state, "HTTP {status}");
            assert_eq!(
                response.results.is_empty(),
                state != EngineState::Ok,
                "HTTP {status}"
            );
        }
    }

    #[tokio::test]
    async fn engine_that_does_not_answer_in_time_reports_error_not_blocked() {
        let app = axum::Router::new().route(
            "/first",
            axum::routing::get(std::future::pending::<&'static str>),
        );
        let base = serve(app).await;
        let client = wreq::Client::builder()
            .timeout(Duration::from_millis(300))
            .no_proxy()
            .build()
            .unwrap();
        let searcher = test_searcher(
            client,
            vec![leaked(Engine {
                name: "slow",
                base_url: base,
                profile: wreq_util::Profile::Chrome149,
                request_url: first_url,
                read: read_body,
            })],
        );
        let response = searcher.search("x").await;
        assert_eq!(response.engines[0].state, EngineState::Error);
        assert!(response.results.is_empty());
    }

    #[tokio::test]
    async fn search_that_cannot_reach_an_engine_reports_error() {
        // A resolver error stays on this machine. Port 443 is already in use here,
        // and `resolve` would send the HTTPS request to that local port.
        let client = wreq::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(wreq::redirect::Policy::none())
            .no_proxy()
            .dns_resolver(FailResolve)
            .build()
            .unwrap();
        let searcher = test_searcher(client, vec![&bing::ENGINE]);
        let response = searcher.search("secret query").await;
        assert!(response.results.is_empty());
        assert_eq!(response.engines.len(), 1);
        assert_eq!(response.engines[0].name, "bing");
        assert_eq!(response.engines[0].state, EngineState::Error);
        assert_eq!(response.engines[0].daily_count, 1);
        let health = &searcher.status().engines[0];
        assert_eq!(health.last_state, Some(EngineState::Error));
        assert_eq!(health.daily_count, 1);
        assert_eq!(
            response.engines[0].error.as_deref(),
            Some("cannot connect to www.bing.com")
        );
        assert!(
            !response.engines[0]
                .error
                .as_deref()
                .unwrap()
                .contains("secret")
        );
    }
}
