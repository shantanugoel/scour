use std::process::ExitCode;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::api::{ADDR, EngineState, SearchResponse};

/// Two engines can each take the 10 s search timeout, so wait longer than 20 s.
const DAEMON_WAIT: Duration = Duration::from_secs(25);

pub async fn search(query: &str) -> ExitCode {
    let endpoint = format!("http://{ADDR}/search");
    let response = match request_daemon(&endpoint, query).await {
        Ok(response) => response,
        Err(line) => {
            eprintln!("{line}");
            return ExitCode::from(1);
        }
    };
    let status = response.status().as_u16();
    let body = match response.text().await {
        Ok(body) => body,
        Err(err) => {
            eprintln!("{}", body_read_failure_message(&err));
            return ExitCode::from(1);
        }
    };
    if status != 200 {
        let message = serde_json::from_str::<ApiError>(&body)
            .map(|error| error.error)
            .unwrap_or_else(|_| "the search failed".to_string());
        eprintln!("scour: {message}");
        return ExitCode::from(1);
    }
    let parsed = match serde_json::from_str::<SearchResponse>(&body) {
        Ok(parsed) => parsed,
        Err(_) => {
            eprintln!("scour: the search failed");
            return ExitCode::from(1);
        }
    };
    let presented = present(&parsed);
    if !presented.stdout.is_empty() {
        print!("{}", presented.stdout);
    }
    if !presented.stderr.is_empty() {
        eprint!("{}", presented.stderr);
    }
    if presented.success {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// The whole answer as one text, for a caller that has no separate error stream, such as MCP.
pub fn render(response: &SearchResponse) -> String {
    let presented = present(response);
    format!("{}{}", presented.stdout, presented.stderr)
}

struct Presented {
    stdout: String,
    stderr: String,
    success: bool,
}

fn present(response: &SearchResponse) -> Presented {
    let mut stdout = String::new();
    let mut stderr = String::new();
    for (index, result) in response.results.iter().enumerate() {
        if index > 0 {
            stdout.push('\n');
        }
        stdout.push_str(&format!(
            "{}. {}\n{}\n{}\n",
            result.position, result.title, result.url, result.content
        ));
    }
    let success = response.has_answer();
    if !stdout.is_empty() && success {
        stdout.push('\n');
    }
    for engine in &response.engines {
        let count = response
            .results
            .iter()
            .filter(|result| result.engine == engine.name)
            .count();
        match engine.state {
            EngineState::Ok => {
                stdout.push_str(&format!("{}: ok ({count} results)\n", engine.name));
            }
            EngineState::Empty => {
                stdout.push_str(&format!("{}: empty ({count} results)\n", engine.name));
            }
            EngineState::Blocked => {
                let rest = engine
                    .ready_at
                    .as_deref()
                    .map_or_else(String::new, |ready_at| {
                        format!(" until {ready_at}{}", remaining_time(ready_at))
                    });
                stderr.push_str(&format!("scour: {}: blocked{rest}\n", engine.name));
            }
            EngineState::ParseFailed => {
                stderr.push_str(&format!("scour: {}: parse_failed\n", engine.name));
            }
            EngineState::Error => {
                let message = engine.error.as_deref().unwrap_or("error");
                stderr.push_str(&format!("scour: {}: error: {message}\n", engine.name));
            }
        }
    }
    if !success {
        stdout.clear();
    }
    Presented {
        stdout,
        stderr,
        success,
    }
}

fn remaining_time(ready_at: &str) -> String {
    let Ok(deadline) = DateTime::parse_from_rfc3339(ready_at) else {
        return String::new();
    };
    let remaining = (deadline.to_utc() - Utc::now())
        .to_std()
        .unwrap_or_default();
    let seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
    if remaining.as_secs() < 60 {
        format!(" (in {seconds} s)")
    } else {
        format!(" (in {} min)", seconds.div_ceil(60))
    }
}

#[derive(serde::Deserialize)]
struct ApiError {
    error: String,
}

async fn request_daemon(endpoint: &str, query: &str) -> Result<wreq::Response, String> {
    let client = crate::search::http_client()
        .map_err(|err| format!("scour: cannot start the client: {err}"))?;
    let mut url = url::Url::parse(endpoint).expect("daemon url");
    url.query_pairs_mut().append_pair("q", query);
    client
        .get(url.as_str())
        .timeout(DAEMON_WAIT)
        .send()
        .await
        .map_err(|err| connect_failure_message(&err))
}

fn connect_failure_message(err: &wreq::Error) -> String {
    // The error text includes the daemon URL, and that URL includes the query.
    if err.is_timeout() {
        timeout_message()
    } else {
        format!("scour: no daemon on {ADDR}. Start it with: scour serve")
    }
}

fn body_read_failure_message(err: &wreq::Error) -> String {
    if err.is_timeout() {
        timeout_message()
    } else {
        "scour: the search failed".to_string()
    }
}

fn timeout_message() -> String {
    format!(
        "scour: no answer from {ADDR} in {} s",
        DAEMON_WAIT.as_secs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{EngineStatus, ResultItem};

    #[test]
    fn ok_search_prints_results_and_names_bing() {
        let response = SearchResponse {
            query: "rust async runtime".to_string(),
            results: vec![ResultItem {
                url: "https://tokio.rs/".to_string(),
                title: "Tokio".to_string(),
                content: "An asynchronous runtime".to_string(),
                engine: "bing".to_string(),
                position: 1,
            }],
            engines: vec![EngineStatus {
                name: "bing".to_string(),
                state: EngineState::Ok,
                error: None,
                ready_at: None,
            }],
        };
        let presented = present(&response);
        assert!(presented.success);
        assert!(presented.stderr.is_empty());
        assert_eq!(
            presented.stdout,
            "1. Tokio\nhttps://tokio.rs/\nAn asynchronous runtime\n\nbing: ok (1 results)\n"
        );
    }

    #[test]
    fn engine_error_does_not_look_like_ok_or_empty() {
        let response = SearchResponse {
            query: "x".to_string(),
            results: Vec::new(),
            engines: vec![EngineStatus {
                name: "bing".to_string(),
                state: EngineState::Error,
                error: Some("cannot connect to www.bing.com".to_string()),
                ready_at: None,
            }],
        };
        let presented = present(&response);
        assert!(!presented.success);
        assert!(presented.stdout.is_empty());
        assert_eq!(
            presented.stderr,
            "scour: bing: error: cannot connect to www.bing.com\n"
        );
        assert!(!presented.stderr.contains("ok"));
        assert!(!presented.stderr.contains("empty"));
    }

    #[test]
    fn blocked_and_parse_failed_answers_fail_and_print_the_state() {
        for (state, name) in [
            (EngineState::Blocked, "blocked"),
            (EngineState::ParseFailed, "parse_failed"),
        ] {
            let response = SearchResponse {
                query: "x".to_string(),
                results: Vec::new(),
                engines: vec![EngineStatus {
                    name: "duckduckgo".to_string(),
                    state,
                    error: None,
                    ready_at: None,
                }],
            };
            let presented = present(&response);
            assert!(!presented.success, "{name}");
            assert!(presented.stdout.is_empty(), "{name}");
            assert!(
                presented
                    .stderr
                    .starts_with(&format!("scour: duckduckgo: {name}")),
                "{}",
                presented.stderr
            );
            assert!(!presented.stderr.contains("empty"), "{name}");
            assert!(!presented.stderr.contains(" ok"), "{name}");
        }
    }

    #[test]
    fn empty_answer_prints_empty_not_ok() {
        let response = SearchResponse {
            query: "x".to_string(),
            results: Vec::new(),
            engines: vec![EngineStatus {
                name: "bing".to_string(),
                state: EngineState::Empty,
                error: None,
                ready_at: None,
            }],
        };
        let presented = present(&response);
        assert!(presented.success);
        assert!(presented.stderr.is_empty());
        assert_eq!(presented.stdout, "bing: empty (0 results)\n");
    }

    #[test]
    fn each_enabled_engine_gets_a_summary_line() {
        let response = SearchResponse {
            query: "x".to_string(),
            results: vec![
                ResultItem {
                    url: "https://one.example/".to_string(),
                    title: "One".to_string(),
                    content: String::new(),
                    engine: "duckduckgo".to_string(),
                    position: 1,
                },
                ResultItem {
                    url: "https://two.example/".to_string(),
                    title: "Two".to_string(),
                    content: String::new(),
                    engine: "bing".to_string(),
                    position: 2,
                },
            ],
            engines: vec![
                EngineStatus {
                    name: "duckduckgo".to_string(),
                    state: EngineState::Ok,
                    error: None,
                    ready_at: None,
                },
                EngineStatus {
                    name: "bing".to_string(),
                    state: EngineState::Ok,
                    error: None,
                    ready_at: None,
                },
            ],
        };
        let presented = present(&response);
        assert!(presented.success);
        assert!(presented.stdout.contains("duckduckgo: ok (1 results)\n"));
        assert!(presented.stdout.contains("bing: ok (1 results)\n"));
        assert!(presented.stderr.is_empty());
    }

    #[tokio::test]
    async fn closed_loopback_port_tells_the_person_to_start_the_daemon() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        drop(listener);
        assert_ne!(address.port(), ADDR.port());
        let endpoint = format!("http://{address}/search");
        let err = request_daemon(&endpoint, "x")
            .await
            .expect_err("closed port");
        assert_eq!(
            err,
            "scour: no daemon on 127.0.0.1:7268. Start it with: scour serve"
        );
    }
}
