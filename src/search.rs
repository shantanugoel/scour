use std::net::SocketAddr;
use std::time::Duration;

use crate::api::{EngineState, EngineStatus, ResultItem, SearchResponse};
use crate::engines::bing;

pub fn http_client() -> wreq::Result<wreq::Client> {
    // wreq follows no redirects, and it reads the system proxy.
    // This issue has no proxy. A Bing answer can be a redirect.
    wreq::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(wreq::redirect::Policy::limited(10))
        .no_proxy()
        .dns_resolver(Ipv4Only)
        .build()
}

pub async fn search(client: &wreq::Client, query: &str) -> SearchResponse {
    let url = bing::request_url(query);
    match client
        .get(url.as_str())
        .emulation(bing::PROFILE)
        .send()
        .await
    {
        Ok(response) => {
            let status = response.status().as_u16();
            match response.text().await {
                Ok(body) => response_for_page(query, status, &body),
                Err(err) => {
                    if status != 200 {
                        response_for_page(query, status, "")
                    } else {
                        error_response(query, fetch_error(&err))
                    }
                }
            }
        }
        Err(err) => error_response(query, fetch_error(&err)),
    }
}

fn response_for_page(query: &str, status: u16, body: &str) -> SearchResponse {
    if status != 200 {
        return error_response(query, format!("HTTP {status}"));
    }
    let hits = bing::parse(body);
    if hits.is_empty() {
        return SearchResponse {
            query: query.to_string(),
            results: Vec::new(),
            engines: vec![EngineStatus {
                name: bing::NAME.to_string(),
                state: EngineState::Empty,
                error: None,
            }],
        };
    }
    let results = hits
        .into_iter()
        .enumerate()
        .map(|(index, hit)| ResultItem {
            url: hit.url,
            title: hit.title,
            content: hit.content,
            engine: bing::NAME.to_string(),
            position: (index + 1) as u32,
        })
        .collect();
    SearchResponse {
        query: query.to_string(),
        results,
        engines: vec![EngineStatus {
            name: bing::NAME.to_string(),
            state: EngineState::Ok,
            error: None,
        }],
    }
}

fn error_response(query: &str, message: String) -> SearchResponse {
    SearchResponse {
        query: query.to_string(),
        results: Vec::new(),
        engines: vec![EngineStatus {
            name: bing::NAME.to_string(),
            state: EngineState::Error,
            error: Some(message),
        }],
    }
}

fn fetch_error(err: &wreq::Error) -> String {
    // The wreq error text includes the request URL, and the URL includes the query.
    if err.is_timeout() {
        "no answer from www.bing.com in 10 s".to_string()
    } else {
        "cannot connect to www.bing.com".to_string()
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

    const RESULTS_PAGE: &str = include_str!("../fixtures/bing/results.html");
    const EMPTY_PAGE: &str = include_str!("../fixtures/bing/empty.html");

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

    #[test]
    fn ads_and_undecoded_bing_links_are_not_results() {
        let html = r#"
            <ol id="b_results">
                <li class="b_ad">
                    <h2><a href="https://ads.example/buy">Buy now</a></h2>
                    <div class="b_caption"><p>An advertisement</p></div>
                </li>
                <li class="b_algo b_ad">
                    <h2><a href="https://shop.example/sale">Sale</a></h2>
                    <div class="b_caption"><p>Also an advertisement</p></div>
                </li>
                <li class="b_algo">
                    <h2><a href="https://www.bing.com/ck/a?u=not-base64">Hidden</a></h2>
                    <div class="b_caption"><p>This link does not decode</p></div>
                </li>
                <li class="b_algo">
                    <h2><a href="https://www.bing.com/maps">Maps</a></h2>
                    <div class="b_caption"><p>A bing.com page</p></div>
                </li>
                <li class="b_algo">
                    <h2><a href="https://tokio.rs/">Tokio</a></h2>
                    <div class="b_caption"><p>An asynchronous runtime</p></div>
                </li>
                <li class="b_rs"><a href="https://related.example/">related search</a></li>
            </ol>
        "#;
        let response = response_for_page("rust", 200, html);
        assert_eq!(response.engines[0].state, EngineState::Ok);
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].title, "Tokio");
        assert_eq!(response.results[0].url, "https://tokio.rs/");
        assert_eq!(response.results[0].content, "An asynchronous runtime");
        assert_eq!(response.results[0].engine, "bing");
        assert_eq!(response.results[0].position, 1);
        assert!(!response.results[0].url.contains("bing.com"));
    }

    #[test]
    fn http_error_is_not_an_empty_answer() {
        let response = response_for_page("x", 503, "<html></html>");
        assert!(response.results.is_empty());
        assert_eq!(response.engines[0].name, "bing");
        assert_eq!(response.engines[0].state, EngineState::Error);
        assert_eq!(response.engines[0].error.as_deref(), Some("HTTP 503"));
        assert_ne!(response.engines[0].state, EngineState::Ok);
        assert_ne!(response.engines[0].state, EngineState::Empty);
    }

    #[tokio::test]
    async fn search_that_cannot_reach_bing_reports_error() {
        // A resolver error stays on this machine. Port 443 is already in use here,
        // and `resolve` would send the HTTPS request to that local port.
        let client = wreq::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(wreq::redirect::Policy::none())
            .no_proxy()
            .dns_resolver(FailResolve)
            .build()
            .unwrap();
        let response = search(&client, "x").await;
        assert!(response.results.is_empty());
        assert_eq!(response.engines.len(), 1);
        assert_eq!(response.engines[0].name, "bing");
        assert_eq!(response.engines[0].state, EngineState::Error);
        assert_eq!(
            response.engines[0].error.as_deref(),
            Some("cannot connect to www.bing.com")
        );
    }

    #[test]
    fn saved_result_page_reports_the_organic_results_as_ok() {
        let response = response_for_page("rust async runtime", 200, RESULTS_PAGE);
        assert_eq!(response.query, "rust async runtime");
        assert_eq!(response.engines.len(), 1);
        assert_eq!(response.engines[0].name, "bing");
        assert_eq!(response.engines[0].state, EngineState::Ok);
        assert!(response.engines[0].error.is_none());
        assert_eq!(response.results.len(), 10);
        assert_eq!(response.results[0].position, 1);
        assert_eq!(response.results[0].title, "Rust Programming Language");
        assert_eq!(response.results[0].url, "https://rust-lang.org/");
        assert_eq!(response.results[0].engine, "bing");
        assert!(!response.results[0].content.is_empty());
        let last = response.results.last().unwrap();
        assert_eq!(last.position, 10);
        assert_eq!(last.title, "The Rust Programming Language");
        assert_eq!(last.url, "https://doc.rust-lang.org/book/");
        assert!(
            response
                .results
                .iter()
                .all(|result| !result.url.contains("bing.com"))
        );
    }

    #[test]
    fn saved_page_with_no_results_reports_empty() {
        // This page is made from the saved result page. Bing did not send it.
        let response = response_for_page("nothing", 200, EMPTY_PAGE);
        assert_eq!(response.results.len(), 0);
        assert_eq!(response.engines.len(), 1);
        assert_eq!(response.engines[0].name, "bing");
        assert_eq!(response.engines[0].state, EngineState::Empty);
        assert!(response.engines[0].error.is_none());
        assert_ne!(response.engines[0].state, EngineState::Ok);
    }
}
