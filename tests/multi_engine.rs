use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use axum::{Router, http::StatusCode, routing::get};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::time::{sleep, timeout};

const DAEMON: &str = "http://127.0.0.1:7268";
const BINARY: &str = env!("CARGO_BIN_EXE_scour");
const QUERY: &str = "rust";

// Both tests exercise the CLI's fixed daemon address.
static DAEMON_SLOT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Daemon {
    child: Child,
    config: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.config);
    }
}

async fn start_daemon() -> Daemon {
    let engines = Router::new()
        .route("/html/", get(|| async { StatusCode::FORBIDDEN }))
        .route(
            "/search",
            get(|| async {
                r#"<ol id="b_results"><li class="b_algo"><h2>
                <a href="https://tokio.rs/">Tokio</a></h2></li></ol>"#
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, engines).await.unwrap() });

    let config =
        std::env::temp_dir().join(format!("scour-multi-engine-{}.toml", std::process::id()));
    std::fs::write(
        &config,
        format!(
            "engines = [\"duckduckgo\", \"bing\"]\n[testing.base_urls]\nduckduckgo = \"{base}\"\nbing = \"{base}\"\n"
        ),
    )
    .unwrap();
    let child = Command::new(BINARY)
        .args(["serve", "--config"])
        .arg(&config)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut daemon = Daemon { child, config };
    let stdout = daemon.child.stdout.take().unwrap();
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    assert_eq!(line.trim_end(), format!("scour: listening on {DAEMON}"));
    daemon
}

fn client() -> wreq::Client {
    wreq::Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .unwrap()
}

async fn get_json(client: &wreq::Client, path: &str) -> Value {
    let response = client.get(format!("{DAEMON}{path}")).send().await.unwrap();
    assert_eq!(response.status().as_u16(), 200);
    response.json().await.unwrap()
}

async fn search(client: &wreq::Client) -> Value {
    get_json(client, &format!("/search?q={QUERY}")).await
}

async fn mcp_search(client: &wreq::Client) -> Value {
    let response = client
        .post(format!("{DAEMON}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "search", "arguments": {"query": QUERY}}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let mut body: Value = response.json().await.unwrap();
    body["result"].take()
}

async fn cli_search() -> Output {
    tokio::task::spawn_blocking(|| {
        Command::new(BINARY)
            .args(["search", QUERY])
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn a_blocked_engine_leaves_the_other_engines_results_available() {
    let _slot = DAEMON_SLOT.lock().await;
    let _daemon = start_daemon().await;
    let client = client();

    let api = search(&client).await;
    let mcp = mcp_search(&client).await;
    assert_eq!(mcp["isError"], false);
    for response in [&api, &mcp["structuredContent"]] {
        let engines = response["engines"].as_array().unwrap();
        assert_eq!(engines.len(), 2);
        assert_eq!(engines[0]["name"], "duckduckgo");
        assert_eq!(engines[0]["state"], "blocked");
        assert_eq!(engines[1]["name"], "bing");
        assert_eq!(engines[1]["state"], "ok");
        assert_eq!(
            response["results"],
            json!([{"title": "Tokio", "url": "https://tokio.rs/", "content": "", "engine": "bing", "position": 1}])
        );
    }

    let output = cli_search().await;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "1. Tokio\nhttps://tokio.rs/\n\n\nbing: ok (1 results) (3/200 daily)\n"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    let ready_at = api["engines"][0]["ready_at"].as_str().unwrap();
    assert!(
        stderr.starts_with(&format!(
            "scour: duckduckgo: blocked (1/200 daily) until {ready_at} (in "
        )),
        "{stderr}"
    );
    assert_eq!(stderr.lines().count(), 1);
}

#[tokio::test]
async fn all_resting_engines_report_their_deadlines_and_no_answer() {
    let _slot = DAEMON_SLOT.lock().await;
    let _daemon = start_daemon().await;
    let client = client();

    for count in 1..=5 {
        assert_eq!(search(&client).await["engines"][1]["daily_count"], count);
    }
    // Reserve Bing's next token so another search would wait longer than 30 seconds.
    let waiting = tokio::spawn({
        let client = client.clone();
        async move { search(&client).await }
    });
    let health = timeout(Duration::from_secs(5), async {
        loop {
            let health = get_json(&client, "/status").await;
            if health["engines"][1]["ready_at"].is_string() {
                break health;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("Bing should reserve its next pace slot");

    let api = search(&client).await;
    assert_eq!(api["results"], json!([]));
    let engines = api["engines"].as_array().unwrap();
    assert_eq!(engines.len(), 2);
    let output = cli_search().await;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 2);
    for (index, (name, state, count)) in [("duckduckgo", "blocked", 1), ("bing", "limited", 6)]
        .into_iter()
        .enumerate()
    {
        let engine = &engines[index];
        let ready_at = engine["ready_at"].as_str().unwrap();
        assert_eq!(engine["name"], name);
        assert_eq!(engine["state"], state);
        assert_eq!(engine["daily_count"], count);
        assert_eq!(health["engines"][index]["ready_at"], ready_at);
        assert!(
            stderr.lines().nth(index).unwrap().starts_with(&format!(
                "scour: {name}: {state} ({count}/200 daily) until {ready_at} (in "
            )),
            "{stderr}"
        );
    }

    let mcp = mcp_search(&client).await;
    assert_eq!(mcp["isError"], true);
    assert_eq!(mcp["structuredContent"], api);
    waiting.abort();
}
