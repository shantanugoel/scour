use std::net::{Ipv4Addr, SocketAddr};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::net::{TcpListener, TcpSocket};

use crate::{cli, start_daemon};

mod browser;

#[tokio::test]
async fn daemon_redacts_proxy_start_line_and_cli_connects_directly() {
    const CHILD: &str = "SCOUR_PROXY_TEST_CHILD";
    const USER: &str = "fixture-user";
    const PASSWORD: &str = "fixture-password";

    if std::env::var_os(CHILD).is_some() {
        let server = start_daemon(None, (Ipv4Addr::LOCALHOST, 0).into(), None)
            .await
            .expect("start daemon");
        let address = server.local_addr().unwrap();
        tokio::select! {
            result = server.run() => panic!("daemon stopped: {result:?}"),
            result = cli::status(address) => assert_eq!(result, ExitCode::SUCCESS),
        }
        return;
    }

    // A bound socket that is not listening refuses connections without a port race.
    let proxy = TcpSocket::new_v4().unwrap();
    proxy.bind((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
    let address = proxy.local_addr().unwrap();
    let root = std::env::temp_dir().join(format!("scour-test-cli-proxy-{}", std::process::id()));
    std::fs::create_dir_all(root.join("scour")).unwrap();
    std::fs::write(
        root.join("scour/config.toml"),
        format!("proxy = \"http://{USER}:{PASSWORD}@{address}\"\n"),
    )
    .unwrap();

    // Give the child its own proxy environment without changing it for other tests.
    let proxy_url = format!("http://{address}");
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "daemon_tests::daemon_redacts_proxy_start_line_and_cli_connects_directly",
            "--quiet",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("XDG_CONFIG_HOME", &root)
        .env("HTTP_PROXY", &proxy_url)
        .env("ALL_PROXY", &proxy_url)
        .env("NO_PROXY", "")
        .env_remove("REQUEST_METHOD")
        .output();
    let _ = std::fs::remove_dir_all(&root);

    let output = output.unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("duckduckgo: idle (0/100 daily)"),
        "{stdout}"
    );
    assert!(!stdout.contains(USER), "{stdout}");
    assert!(!stdout.contains(PASSWORD), "{stdout}");
    let (listen, proxy) = stdout
        .lines()
        .find_map(|line| line.strip_prefix("scour: listening on http://"))
        .and_then(|line| line.split_once(" (proxy "))
        .expect("daemon start line");
    let listen: SocketAddr = listen.parse().unwrap();
    assert_eq!(listen.ip(), Ipv4Addr::LOCALHOST);
    assert_ne!(listen.port(), 0);
    assert_eq!(proxy, format!("{proxy_url})"));
}

#[tokio::test]
async fn cli_engines_option_asks_only_the_named_engine_and_prints_a_bad_item() {
    const CHILD: &str = "SCOUR_ENGINES_TEST_CHILD";

    if std::env::var_os(CHILD).is_some() {
        let duckduckgo = Arc::new(AtomicUsize::new(0));
        let bing = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route(
                "/html/",
                axum::routing::get({
                    let duckduckgo = duckduckgo.clone();
                    move || {
                        duckduckgo.fetch_add(1, Ordering::SeqCst);
                        async { include_str!("../fixtures/duckduckgo/results.html") }
                    }
                }),
            )
            .route(
                "/search",
                axum::routing::get({
                    let bing = bing.clone();
                    move || {
                        bing.fetch_add(1, Ordering::SeqCst);
                        async { include_str!("../fixtures/bing/results.html") }
                    }
                }),
            );
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let engine = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = std::path::PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap());
        std::fs::write(
            config.join("scour/config.toml"),
            format!(
                "engines = [\"duckduckgo\", \"bing\"]\n[testing.base_urls]\n\
                 duckduckgo = \"{engine}\"\nbing = \"{engine}\"\n"
            ),
        )
        .unwrap();

        let server = start_daemon(None, (Ipv4Addr::LOCALHOST, 0).into(), None)
            .await
            .expect("start daemon");
        let address = server.local_addr().unwrap();
        let counts = || {
            (
                duckduckgo.load(Ordering::SeqCst),
                bing.load(Ordering::SeqCst),
            )
        };
        tokio::select! {
            result = server.run() => panic!("daemon stopped: {result:?}"),
            () = async {
                let code = cli::search(address, "rust", Some("duckduckgo")).await;
                assert_eq!(code, ExitCode::SUCCESS);
                assert_eq!(counts(), (1, 0));
                for list in ["duckduckgo,yahoo", "duckduckgo@browser"] {
                    let code = cli::search(address, "rust", Some(list)).await;
                    assert_eq!(code, ExitCode::from(1), "--engines {list}");
                }
                assert_eq!(counts(), (1, 0));
            } => {}
        }
        return;
    }

    let root = std::env::temp_dir().join(format!("scour-test-cli-engines-{}", std::process::id()));
    std::fs::create_dir_all(root.join("scour")).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "daemon_tests::cli_engines_option_asks_only_the_named_engine_and_prints_a_bad_item",
            "--quiet",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("XDG_CONFIG_HOME", &root)
        .output();
    let _ = std::fs::remove_dir_all(&root);

    let output = output.unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("duckduckgo@http: ok ("), "{stdout}");
    assert!(!stdout.contains("bing"), "{stdout}");
    for item in ["yahoo", "duckduckgo@browser", "--browser"] {
        assert!(stderr.contains(item), "{item}: {stderr}");
    }
}
