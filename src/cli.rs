use std::process::ExitCode;

use crate::api::{ADDR, EngineState, SearchResponse};

pub async fn search(query: &str) -> ExitCode {
    let client = match crate::search::http_client() {
        Ok(client) => client,
        Err(err) => {
            eprintln!("scour: cannot start the client: {err}");
            return ExitCode::from(1);
        }
    };
    let mut url = url::Url::parse(&format!("http://{ADDR}/search")).expect("daemon url");
    url.query_pairs_mut().append_pair("q", query);
    let response = match client.get(url.as_str()).send().await {
        Ok(response) => response,
        Err(err) => {
            // The error text includes the daemon URL, and that URL includes the query.
            if err.is_timeout() {
                eprintln!("scour: no answer from {ADDR} in 10 s");
            } else {
                eprintln!("scour: no daemon on {ADDR}. Start it with: scour serve");
            }
            return ExitCode::from(1);
        }
    };
    let status = response.status().as_u16();
    let body = match response.text().await {
        Ok(body) => body,
        Err(_) => {
            eprintln!("scour: no answer from {ADDR} in 10 s");
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
    let success = response
        .engines
        .iter()
        .any(|engine| matches!(engine.state, EngineState::Ok | EngineState::Empty));
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

#[derive(serde::Deserialize)]
struct ApiError {
    error: String,
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
    fn empty_answer_prints_empty_not_ok() {
        let response = SearchResponse {
            query: "x".to_string(),
            results: Vec::new(),
            engines: vec![EngineStatus {
                name: "bing".to_string(),
                state: EngineState::Empty,
                error: None,
            }],
        };
        let presented = present(&response);
        assert!(presented.success);
        assert!(presented.stderr.is_empty());
        assert_eq!(presented.stdout, "bing: empty (0 results)\n");
    }
}
