use std::net::SocketAddr;
use std::time::Duration;

use crate::api::{EngineState, EngineStatus, ResultItem, SearchResponse};
use crate::config::Config;
use crate::engines::{Engine, Hit, Page};

const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Searcher {
    client: wreq::Client,
    engines: Vec<&'static Engine>,
}

struct Answer {
    hits: Option<Vec<Hit>>,
    state: EngineState,
    error: Option<String>,
}

impl Answer {
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
        Ok(Self {
            client: http_client()?,
            engines: config.engines.clone(),
        })
    }

    pub async fn search(&self, query: &str) -> SearchResponse {
        let mut results = Vec::new();
        let mut engines = Vec::with_capacity(self.engines.len());
        for engine in &self.engines {
            let answer = self.fetch(engine, query).await;
            if let Some(hits) = answer.hits {
                for hit in hits {
                    let position = (results.len() + 1) as u32;
                    results.push(ResultItem {
                        url: hit.url,
                        title: hit.title,
                        content: hit.content,
                        engine: engine.name.to_string(),
                        position,
                    });
                }
            }
            engines.push(EngineStatus {
                name: engine.name.to_string(),
                state: answer.state,
                error: answer.error,
            });
        }
        SearchResponse {
            query: query.to_string(),
            results,
            engines,
        }
    }

    async fn fetch(&self, engine: &Engine, query: &str) -> Answer {
        let base = url::Url::parse(engine.base_url).expect("engine base url");
        let host = base.host_str().unwrap_or(engine.base_url).to_string();
        let url = (engine.request_url)(&base, query);
        match self
            .client
            .get(url.as_str())
            .emulation(engine.profile)
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status().as_u16();
                match response.text().await {
                    Ok(body) => answer_for_page(engine, status, &body),
                    Err(err) => {
                        if status != 200 {
                            answer_for_page(engine, status, "")
                        } else {
                            Answer::error(fetch_error(&err, &host))
                        }
                    }
                }
            }
            Err(err) => Answer::error(fetch_error(&err, &host)),
        }
    }
}

fn answer_for_page(engine: &Engine, status: u16, body: &str) -> Answer {
    if status != 200 {
        return Answer::error(format!("HTTP {status}"));
    }
    match (engine.read)(status, body) {
        Page::Results(hits) => Answer {
            hits: Some(hits),
            state: EngineState::Ok,
            error: None,
        },
        Page::Empty => Answer {
            hits: None,
            state: EngineState::Empty,
            error: None,
        },
    }
}

pub fn http_client() -> wreq::Result<wreq::Client> {
    // wreq follows no redirects by default, and it reads the system proxy.
    // scour has no proxy setting yet, and a Bing answer can be a redirect.
    wreq::Client::builder()
        .timeout(TIMEOUT)
        .redirect(wreq::redirect::Policy::limited(10))
        .no_proxy()
        .dns_resolver(Ipv4Only)
        .build()
}

fn fetch_error(err: &wreq::Error, host: &str) -> String {
    // The wreq error text includes the request URL, and the URL includes the query.
    if err.is_timeout() {
        format!("no answer from {host} in {} s", TIMEOUT.as_secs())
    } else {
        format!("cannot connect to {host}")
    }
}

/// `local_address` binds only for an IPv4 target, so drop every IPv6 answer.
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
    use crate::engines::bing;

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
    async fn both_enabled_engines_report_a_state_in_config_order() {
        let app = axum::Router::new()
            .route("/first", axum::routing::get(|| async { "first" }))
            .route("/second", axum::routing::get(|| async { "second" }));
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
        let searcher = Searcher {
            client: test_client(),
            engines,
        };
        let response = searcher.search("rust async runtime").await;
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
        let searcher = Searcher {
            client: test_client(),
            engines,
        };
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
        let searcher = Searcher {
            client: test_client(),
            engines,
        };
        let response = searcher.search("x").await;
        assert!(response.results.is_empty());
        assert_eq!(response.engines[0].state, EngineState::Empty);
        assert!(response.engines[0].error.is_none());
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
        let searcher = Searcher {
            client,
            engines: vec![&bing::ENGINE],
        };
        let response = searcher.search("secret query").await;
        assert!(response.results.is_empty());
        assert_eq!(response.engines.len(), 1);
        assert_eq!(response.engines[0].name, "bing");
        assert_eq!(response.engines[0].state, EngineState::Error);
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
