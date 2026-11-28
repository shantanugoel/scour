use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::Query;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep, timeout};
use url::Url;

use crate::api::{EngineState, SearchResponse, StatusResponse};
use crate::browser::{Browser, BrowserSettings, TABS};
use crate::engines::Method;
use crate::{cli, start_daemon};

const CHILD_ROOT: &str = "SCOUR_TEST_BROWSER_ROOT";
const CHILD_PORT: &str = "SCOUR_TEST_CDP_PORT";
const CHILD_ENDPOINT: &str = "SCOUR_TEST_CDP_ENDPOINT";
const QUERY: &str = "scour-query-fixture-v6xM43sw";
const HUNG_QUERY: &str = "hung-page";
const TITLE: &str = "scour-result-fixture-B7zA55tj";
const FIXTURE: &str = r#"<!doctype html><body>
<script>
document.body.insertAdjacentHTML('beforeend',
    '<div class="result"><a class="result__a" href="https://example.test/">scour-result-fixture-B7zA55tj</a></div>');
</script>"#;

// A test that snapshots X paths must see a stable set of other test displays.
static DISPLAYS: Mutex<()> = Mutex::const_new(());

#[tokio::test]
async fn concurrent_browser_queries_wait_for_free_tabs() {
    let _displays = DISPLAYS.lock().await;
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let engine = serve(Router::new().route(
        "/html/",
        get({
            let peak = peak.clone();
            move || {
                let active = active.clone();
                let peak = peak.clone();
                async move {
                    let count = active.fetch_add(1, Ordering::Relaxed) + 1;
                    peak.fetch_max(count, Ordering::Relaxed);
                    sleep(Duration::from_secs(1)).await;
                    active.fetch_sub(1, Ordering::Relaxed);
                    Html(FIXTURE)
                }
            }
        }),
    ))
    .await;
    let mut daemon = TestDaemon::start_url(
        'j',
        "duckduckgo",
        format!("http://{engine}"),
        StartOptions {
            extra: Some("[limits]\nburst = 10"),
            ..Default::default()
        },
    )
    .await;
    let queries = (0..TABS + 2).map(|index| {
        let address = daemon.address;
        async move {
            search_query(
                address,
                &format!("page {index}"),
                Some("duckduckgo@browser"),
            )
            .await
        }
    });
    for answer in futures::future::join_all(queries).await {
        assert_rendered(answer);
    }
    let peak = peak.load(Ordering::Relaxed);
    assert!((2..=TABS).contains(&peak), "{peak} concurrent requests");
    daemon.stop(libc::SIGTERM).await;
}

#[tokio::test]
async fn a_full_browser_returns_an_error_at_the_tab_wait_deadline() {
    let _displays = DISPLAYS.lock().await;
    let (engine, requests) = budget_fixture_server().await;
    let url = Url::parse(&format!("http://{engine}/html/?q={HUNG_QUERY}")).unwrap();
    let root = PathBuf::from(format!("/tmp/s{:x}tab", std::process::id()));
    let browser = Browser::open(
        BrowserSettings {
            cdp_port: 0,
            root: root.clone(),
        },
        Duration::from_secs(90),
        None,
    )
    .unwrap();
    let hold_tabs = futures::future::join_all((0..TABS).map(|index| {
        let browser = &browser;
        let mut url = url.clone();
        url.query_pairs_mut().append_pair("tab", &index.to_string());
        async move {
            let tab = browser
                .open_tab(Instant::now() + Duration::from_secs(20))
                .await
                .unwrap_or_else(|error| panic!("{}", error.message(&url)));
            tab.render(&url).await
        }
    }));
    let wait_for_tab = async {
        poll("all page requests", Duration::from_secs(15), || {
            (requests.load(Ordering::Relaxed) == TABS).then_some(())
        })
        .await;
        let now = Instant::now();
        let result = browser.open_tab(now + Duration::from_secs(2)).await;
        (result, now.elapsed())
    };
    let (_, (result, elapsed)) = tokio::join!(hold_tabs, wait_for_tab);
    let error = result.err().map(|error| error.message(&url));
    browser.stop().await;
    drop(browser);
    fs::remove_dir(root).unwrap();
    assert_eq!(
        error.as_deref(),
        Some("all 4 browser tabs stayed busy for 2 s")
    );
    assert!(
        (Duration::from_secs(2)..Duration::from_secs(3)).contains(&elapsed),
        "tab wait took {elapsed:?}"
    );
}

#[tokio::test]
async fn a_hung_browser_page_times_out_and_closes_its_target() {
    let _displays = DISPLAYS.lock().await;
    let (engine, _) = budget_fixture_server().await;
    let mut daemon = TestDaemon::start('k', "duckduckgo@browser", engine, 0, None).await;
    assert_eq!(daemon.search(None).await.engines[0].state, EngineState::Ok);
    let active_port =
        fs::read_to_string(daemon.run_dir().join("profile/DevToolsActivePort")).unwrap();
    let targets = format!(
        "http://127.0.0.1:{}/json/list",
        active_port.lines().next().unwrap()
    );
    let client = wreq::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let pages = || async {
        let targets: Vec<serde_json::Value> = client
            .get(&targets)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        targets
            .into_iter()
            .filter(|target| target["type"] == "page")
            .map(|target| target["id"].as_str().unwrap().to_owned())
            .collect::<BTreeSet<_>>()
    };
    let before = pages().await;
    let answer = search_query(daemon.address, HUNG_QUERY, None).await;
    assert_eq!(answer.engines[0].state, EngineState::Error, "{answer:?}");
    assert_eq!(
        answer.engines[0].error.as_deref(),
        Some("no answer from 127.0.0.1 in 10 s")
    );
    assert_eq!(answer.engines[0].daily_count, 2);
    timeout(Duration::from_secs(5), async {
        while pages().await != before {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the timed-out page target should close");
    daemon.stop(libc::SIGTERM).await;
}

#[tokio::test]
async fn a_paced_browser_query_reports_a_page_timeout_before_the_cli_timeout() {
    let _displays = DISPLAYS.lock().await;
    let (engine, requests) = budget_fixture_server().await;
    let mut daemon = TestDaemon::start_url(
        'm',
        "duckduckgo",
        format!("http://{engine}"),
        StartOptions {
            extra: Some("[limits]\nburst = 1\nrefill_seconds = 30"),
            chrome_delay_seconds: Some(7),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(daemon.search(None).await.engines[0].state, EngineState::Ok);
    let now = Instant::now();
    let query = cli::search(daemon.address, HUNG_QUERY, Some("duckduckgo@browser"));
    let sent = async {
        poll("the paced page request", Duration::from_secs(40), || {
            (requests.load(Ordering::Relaxed) == 1).then_some(now.elapsed())
        })
        .await
    };
    let (exit, sent_at) = tokio::join!(query, sent);
    let elapsed = now.elapsed();
    let status: StatusResponse = wreq::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
        .get(format!("http://{}/status", daemon.address))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(exit, ExitCode::from(1));
    assert_eq!(status.engines[0].last_state, Some(EngineState::Error));
    assert_eq!(status.engines[0].daily_count, 2);
    assert!(sent_at >= Duration::from_secs(35), "sent at {sent_at:?}");
    assert!(
        (Duration::from_secs(38)..Duration::from_secs(45)).contains(&elapsed),
        "CLI returned after {elapsed:?}"
    );
    daemon.stop(libc::SIGTERM).await;
}

#[tokio::test]
async fn browser_selections_render_without_storing_queries_or_results() {
    let _displays = DISPLAYS.lock().await;
    let engine = fixture_server().await;
    for (name, selection) in [
        ("duckduckgo", Some("duckduckgo@browser")),
        ("duckduckgo@browser", None),
    ] {
        let mut daemon = TestDaemon::start('r', name, engine, 0, None).await;
        daemon.assert_empty_root();
        if selection.is_some() {
            let http = daemon.search(None).await;
            assert_eq!(http.engines[0].state, EngineState::ParseFailed, "{http:?}");
            daemon.assert_empty_root();
            assert_eq!(
                cli::search(daemon.address, QUERY, selection).await,
                ExitCode::SUCCESS
            );
        }
        assert_rendered(daemon.search(selection).await);
        sleep(Duration::from_secs(3)).await;
        let run = daemon.run_dir();
        assert!(run.join("profile").is_dir());
        assert_no_query_data(&daemon.browser_root());
        daemon.stop(libc::SIGTERM).await;
        daemon.assert_empty_root();
    }
}

#[tokio::test]
async fn http_block_rests_the_browser_method_without_starting_chrome() {
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let engine = serve(Router::new().route(
        "/html/",
        get(move || {
            count.fetch_add(1, Ordering::Relaxed);
            async { StatusCode::FORBIDDEN }
        }),
    ))
    .await;
    let mut daemon = TestDaemon::start('h', "duckduckgo", engine, 0, None).await;
    let http = daemon.search(None).await;
    let browser = daemon.search(Some("duckduckgo@browser")).await;
    assert_eq!(http.engines[0].state, EngineState::Blocked, "{http:?}");
    assert_eq!(
        browser.engines[0].state,
        EngineState::Blocked,
        "{browser:?}"
    );
    assert_eq!(browser.engines[0].method, Method::Browser);
    assert!(http.engines[0].ready_at.is_some());
    assert_eq!(browser.engines[0].ready_at, http.engines[0].ready_at);
    assert_eq!(browser.engines[0].daily_count, 1);
    assert_eq!(requests.load(Ordering::Relaxed), 1);
    daemon.assert_empty_root();
    daemon.stop(libc::SIGTERM).await;
}

#[tokio::test]
async fn a_bad_certificate_gives_error() {
    let _displays = DISPLAYS.lock().await;
    let tls = TlsFixture::start().await;
    let mut daemon = TestDaemon::start_url(
        'c',
        "duckduckgo@browser",
        format!("https://{}", tls.address),
        StartOptions::default(),
    )
    .await;
    let response = daemon.search(None).await;
    assert_eq!(
        response.engines[0].state,
        EngineState::Error,
        "{response:?}"
    );
    let error = response.engines[0].error.as_deref().unwrap_or_default();
    assert!(
        error.contains("cannot connect to 127.0.0.1"),
        "{response:?}"
    );
    daemon.stop(libc::SIGTERM).await;
    daemon.assert_empty_root();
}

#[tokio::test]
async fn the_browser_sends_queries_and_background_traffic_through_the_proxy() {
    let _displays = DISPLAYS.lock().await;
    for (scheme, kind) in [("http", ProxyKind::Http), ("socks5h", ProxyKind::Socks5)] {
        let engine = fixture_server().await;
        let proxy = test_proxy(kind, engine).await;
        let extra = format!("proxy = \"{scheme}://{}\"", proxy.address);
        let mut daemon = TestDaemon::start_url(
            'y',
            "duckduckgo@browser",
            format!("http://engine.scour.test:{}", engine.port()),
            StartOptions {
                extra: Some(&extra),
                ..Default::default()
            },
        )
        .await;
        assert_rendered(daemon.search(None).await);
        // Chrome's background services start with the session. Give them time to show in the log.
        sleep(Duration::from_secs(5)).await;
        let targets = proxy.targets.lock().await.clone();
        assert!(!targets.is_empty(), "{scheme}: {targets:?}");
        assert!(
            targets
                .iter()
                .all(|target| target.starts_with("engine.scour.test:")),
            "{scheme}: {targets:?}"
        );
        daemon.stop(libc::SIGTERM).await;
        daemon.assert_empty_root();
    }
}

#[tokio::test]
async fn an_ipv6_proxy_reaches_the_engine() {
    let _displays = DISPLAYS.lock().await;
    let engine = fixture_server().await;
    let proxy = test_proxy_on(Ipv6Addr::LOCALHOST.into(), ProxyKind::Http, engine).await;
    let extra = format!("proxy = \"http://{}\"", proxy.address);
    let mut daemon = TestDaemon::start_url(
        '6',
        "duckduckgo@browser",
        format!("http://engine.scour.test:{}", engine.port()),
        StartOptions {
            extra: Some(&extra),
            ..Default::default()
        },
    )
    .await;
    assert_rendered(daemon.search(None).await);
    let targets = proxy.targets.lock().await.clone();
    assert!(!targets.is_empty(), "{targets:?}");
    assert!(
        targets
            .iter()
            .all(|target| target.starts_with("engine.scour.test:")),
        "{targets:?}"
    );
    daemon.stop(libc::SIGTERM).await;
    daemon.assert_empty_root();
}

#[tokio::test]
async fn a_stopped_proxy_gives_error_and_no_direct_request() {
    let _displays = DISPLAYS.lock().await;
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let engine = serve(Router::new().route(
        "/html/",
        get(move || {
            count.fetch_add(1, Ordering::SeqCst);
            async { Html(FIXTURE) }
        }),
    ))
    .await;
    // A bound socket that is not listening refuses connections without a port race.
    let proxy = tokio::net::TcpSocket::new_v4().unwrap();
    proxy.bind((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
    let proxy_address = proxy.local_addr().unwrap();
    let _proxy = proxy;
    let extra = format!("proxy = \"http://{proxy_address}\"");
    let mut daemon = TestDaemon::start_url(
        'z',
        "duckduckgo@browser",
        format!("http://{engine}"),
        StartOptions {
            extra: Some(&extra),
            ..Default::default()
        },
    )
    .await;
    let response = daemon.search(None).await;
    assert_eq!(
        response.engines[0].state,
        EngineState::Error,
        "{response:?}"
    );
    assert_eq!(
        response.engines[0].error.as_deref(),
        Some("cannot connect to the proxy")
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    let run = daemon.run_dir();
    let (_, group, _, _) = browser_process(&run);
    sleep(Duration::from_secs(1)).await;
    assert_chrome_tcp_peers_are_loopback(group);
    daemon.stop(libc::SIGTERM).await;
    daemon.assert_empty_root();
}

#[tokio::test]
async fn with_no_proxy_chrome_connects_to_engines_over_ipv4() {
    let _displays = DISPLAYS.lock().await;
    let (engine, ipv4, ipv6) = dual_stack_fixture().await;
    let first = tokio::net::lookup_host(("localhost", engine.port()))
        .await
        .unwrap()
        .next()
        .expect("localhost address");
    assert!(first.is_ipv6(), "localhost resolves to {first} first");

    let mut daemon = TestDaemon::start_url(
        'w',
        "duckduckgo@browser",
        format!("http://localhost:{}", engine.port()),
        StartOptions::default(),
    )
    .await;
    assert_rendered(daemon.search(None).await);
    assert!(ipv4.load(Ordering::SeqCst) > 0);
    assert_eq!(ipv6.load(Ordering::SeqCst), 0);
    daemon.stop(libc::SIGTERM).await;
    daemon.assert_empty_root();

    let requests = ipv4.load(Ordering::SeqCst);
    let mut daemon = TestDaemon::start_url(
        'v',
        "duckduckgo@browser",
        format!("http://[::1]:{}", engine.port()),
        StartOptions::default(),
    )
    .await;
    let response = daemon.search(None).await;
    assert_eq!(
        response.engines[0].state,
        EngineState::Error,
        "{response:?}"
    );
    assert_eq!(
        response.engines[0].error.as_deref(),
        Some("cannot connect to [::1] over IPv4")
    );
    assert_eq!(ipv4.load(Ordering::SeqCst), requests);
    assert_eq!(ipv6.load(Ordering::SeqCst), 0);
    daemon.stop(libc::SIGTERM).await;
    daemon.assert_empty_root();
}

#[tokio::test]
async fn cdp_listens_on_ipv4_loopback_only() {
    let _displays = DISPLAYS.lock().await;
    let mut daemon =
        TestDaemon::start('l', "duckduckgo@browser", fixture_server().await, 0, None).await;
    assert_rendered(daemon.search(None).await);
    let active_port =
        fs::read_to_string(daemon.run_dir().join("profile/DevToolsActivePort")).unwrap();
    let port: u16 = active_port.lines().next().unwrap().parse().unwrap();
    assert_eq!(listeners("/proc/net/tcp", port), ["0100007F"]);
    assert!(listeners("/proc/net/tcp6", port).is_empty());
    daemon.stop(libc::SIGTERM).await;
}

#[tokio::test]
async fn signal_shutdown_reaps_chrome_and_preserves_existing_x_displays() {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        let cdp_port = std::env::var(CHILD_PORT).unwrap().parse().unwrap();
        let browser = BrowserSettings {
            cdp_port,
            root: root.join("b"),
        };
        let daemon = start_daemon(
            Some(&root.join("config.toml")),
            (Ipv4Addr::LOCALHOST, 0).into(),
            Some(browser),
        )
        .await
        .unwrap();
        fs::write(
            root.join("address"),
            daemon.local_addr().unwrap().to_string(),
        )
        .unwrap();
        daemon.run().await.unwrap();
        return;
    }

    let _displays = DISPLAYS.lock().await;
    let engine = fixture_server().await;
    // Give a runner with no desktop an occupied socket path and lock to preserve.
    let _socket = ExistingX::create("/tmp/.X11-unix/X0");
    let _lock = ExistingX::create("/tmp/.X1-lock");
    let before = x_inodes();
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let mut daemon = TestDaemon::start('s', "duckduckgo@browser", engine, 0, None).await;
        assert_rendered(daemon.search(None).await);
        let run = daemon.run_dir();
        let profile = format!("--user-data-dir={}", run.join("profile").display());
        let chrome = find_pid(|args| {
            args.contains(&profile.as_str()) && !args.iter().any(|arg| arg.starts_with("--type="))
        });
        let group = unsafe { libc::getpgid(chrome) };
        assert!(group > 0);
        let display = fs::read_to_string(run.join("display")).unwrap();
        let display = display.trim();
        let xvfb = xvfb_pid(display);
        let lock = fs::read_to_string(format!("/tmp/.X{display}-lock")).unwrap();
        assert_eq!(lock, format!("{:>10}\n", daemon.child.id()));
        assert_inodes(&before);

        daemon.stop(signal).await;
        daemon.assert_empty_root();
        assert_stopped(group, xvfb, display);
        assert_inodes(&before);
    }
}

#[tokio::test]
async fn chrome_start_failures_name_the_cause_and_release_resources() {
    let _displays = DISPLAYS.lock().await;
    let engine = fixture_server().await;
    for (endpoint, error) in [
        (
            "ws://127.0.0.1:1/devtools/browser/x",
            "cannot connect to Chrome over CDP at 127.0.0.1:1",
        ),
        (
            "ws://[::1]:1/devtools/browser/x",
            "Chrome opened CDP on [::1]:1, not on 127.0.0.1",
        ),
    ] {
        let mut daemon =
            TestDaemon::start('f', "duckduckgo@browser", engine, 0, Some(endpoint)).await;
        let request = tokio::spawn(search(daemon.address, None));
        let (chrome, display) = poll("fake Chrome to start", Duration::from_secs(15), || {
            assert!(
                !request.is_finished(),
                "search finished before fake Chrome started"
            );
            let line = fs::read_to_string(daemon.root.join("fake")).ok()?;
            let (pid, display) = line.trim().split_once(" :")?;
            Some((pid.parse().ok()?, display.to_string()))
        })
        .await;
        let xvfb = xvfb_pid(&display);
        fs::write(daemon.root.join("announce"), []).unwrap();
        assert_start_failure(request.await.unwrap(), error);
        daemon.assert_empty_root();
        assert_stopped(chrome, xvfb, &display);
        daemon.stop(libc::SIGTERM).await;
    }

    let held = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = held.local_addr().unwrap().port();
    let before = x_inodes();
    let mut daemon =
        TestDaemon::start('p', "duckduckgo@browser", engine, port, Some("unused")).await;
    assert_start_failure(
        daemon.search(None).await,
        &format!("127.0.0.1:{port} is in use"),
    );
    daemon.assert_empty_root();
    assert!(!daemon.root.join("fake").exists());
    assert_eq!(x_inodes(), before);
    daemon.stop(libc::SIGTERM).await;
}

#[tokio::test]
async fn google_goes_through_the_browser_and_its_sorry_page_starts_a_rest() {
    let _displays = DISPLAYS.lock().await;
    let searches = Arc::new(AtomicUsize::new(0));
    let count = searches.clone();
    let engine = serve(
        Router::new()
            .route(
                "/search",
                get(move || {
                    let first = count.fetch_add(1, Ordering::SeqCst) == 0;
                    async move {
                        if first {
                            Html(include_str!("../../fixtures/google/results.html")).into_response()
                        } else {
                            // Google sends a blocked search to its /sorry/ page.
                            (
                                StatusCode::FOUND,
                                [(header::LOCATION, "/sorry/index?continue=REDACTED")],
                            )
                                .into_response()
                        }
                    }
                }),
            )
            .route(
                "/sorry/index",
                get(|| async {
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        Html(include_str!("../../fixtures/google/sorry.html")),
                    )
                }),
            ),
    )
    .await;
    let mut daemon = TestDaemon::start('g', "google", engine, 0, None).await;

    let found = daemon.search(None).await;
    assert_eq!(found.engines[0].name, "google", "{found:?}");
    assert_eq!(found.engines[0].method, Method::Browser, "{found:?}");
    assert_eq!(found.engines[0].state, EngineState::Ok, "{found:?}");
    assert_eq!(found.results.len(), 8, "{found:?}");
    assert!(found.results.iter().all(|result| result.engine == "google"));
    assert_eq!(
        found.results[0].url,
        "https://www.youtube.com/watch?v=2Oo0615YhFg"
    );
    assert_eq!(
        found.results[2].url,
        "https://www.google.com/goto?url=REDACTED-3"
    );
    assert_eq!(found.results[2].domain.as_deref(), Some("www.facebook.com"));

    let blocked = daemon.search(None).await;
    assert_eq!(
        blocked.engines[0].state,
        EngineState::Blocked,
        "{blocked:?}"
    );
    assert!(blocked.engines[0].ready_at.is_some(), "{blocked:?}");
    assert!(blocked.results.is_empty(), "{blocked:?}");
    assert_eq!(searches.load(Ordering::SeqCst), 2);
    daemon.stop(libc::SIGTERM).await;
    daemon.assert_empty_root();
}

#[tokio::test]
async fn idle_browser_stop_removes_the_profile_and_starts_a_new_chrome() {
    let _displays = DISPLAYS.lock().await;
    let engine = fixture_server().await;
    let mut daemon = TestDaemon::start_idle('i', "duckduckgo@browser", engine, 0, None, 2).await;
    assert_rendered(daemon.search(None).await);
    let run = daemon.run_dir();
    let (_, group, display, xvfb) = browser_process(&run);

    poll("the idle stop", Duration::from_secs(15), || {
        fs::read_dir(daemon.browser_root())
            .unwrap()
            .next()
            .is_none()
            .then_some(())
    })
    .await;
    assert!(!run.exists(), "{}", run.display());
    wait_stopped(group, xvfb, &display).await;

    assert_rendered(daemon.search(None).await);
    assert_ne!(daemon.run_dir(), run);
    daemon.stop(libc::SIGTERM).await;
    daemon.assert_empty_root();
}

#[tokio::test]
async fn a_chrome_crash_during_a_busy_query_ends_the_session() {
    let _displays = DISPLAYS.lock().await;
    let (engine, requests) = slow_fixture_server().await;
    let mut daemon = TestDaemon::start_idle('u', "duckduckgo@browser", engine, 0, None, 6).await;
    let request = tokio::spawn(search(daemon.address, None));
    let run = poll("the browser to start", Duration::from_secs(15), || {
        fs::read_dir(daemon.browser_root())
            .ok()?
            .next()?
            .ok()
            .map(|entry| entry.path())
    })
    .await;
    let chrome = poll("Chrome to start", Duration::from_secs(15), || {
        browser_pid(&run)
    })
    .await;
    let (_, group, display, xvfb) = browser_process(&run);
    poll("the slow page request", Duration::from_secs(15), || {
        (requests.load(Ordering::Relaxed) > 0).then_some(())
    })
    .await;
    // The idle deadline passes while the slow query holds the CDP client. Kill before the
    // page timeout, so the query is still busy on its own and not because of the timeout.
    sleep(Duration::from_secs(7)).await;
    assert!(
        !request.is_finished(),
        "the query finished before the crash"
    );
    assert_eq!(unsafe { libc::kill(chrome, libc::SIGKILL) }, 0);
    poll("the busy crash cleanup", Duration::from_secs(5), || {
        fs::read_dir(daemon.browser_root())
            .unwrap()
            .next()
            .is_none()
            .then_some(())
    })
    .await;
    assert!(!run.exists(), "{}", run.display());
    wait_stopped(group, xvfb, &display).await;
    request.abort();

    daemon.stop(libc::SIGTERM).await;
    daemon.assert_empty_root();
}

#[tokio::test]
async fn chrome_crash_ends_the_session_and_the_next_query_starts_chrome() {
    let _displays = DISPLAYS.lock().await;
    let engine = fixture_server().await;
    let mut daemon = TestDaemon::start('x', "duckduckgo@browser", engine, 0, None).await;
    assert_rendered(daemon.search(None).await);
    let run = daemon.run_dir();
    let (chrome, group, display, xvfb) = browser_process(&run);

    assert_eq!(unsafe { libc::kill(chrome, libc::SIGKILL) }, 0);
    poll("the crash cleanup", Duration::from_secs(10), || {
        fs::read_dir(daemon.browser_root())
            .unwrap()
            .next()
            .is_none()
            .then_some(())
    })
    .await;
    assert!(!run.exists(), "{}", run.display());
    wait_stopped(group, xvfb, &display).await;

    assert_rendered(daemon.search(None).await);
    let crashed = daemon.run_dir();
    assert_ne!(crashed, run);
    let (chrome, _, _, _) = browser_process(&crashed);
    assert_eq!(unsafe { libc::kill(chrome, libc::SIGKILL) }, 0);
    // The query can arrive before try_wait sees the exit. It must end that session and start one
    // new session, not fail with the closed CDP connection.
    assert_rendered(daemon.search(None).await);
    assert!(!crashed.exists(), "{}", crashed.display());
    assert_ne!(daemon.run_dir(), crashed);

    daemon.stop(libc::SIGTERM).await;
    daemon.assert_empty_root();
}

#[tokio::test]
async fn a_second_daemon_cannot_use_a_running_browser_root() {
    let _displays = DISPLAYS.lock().await;
    let engine = fixture_server().await;
    let mut daemon = TestDaemon::start('d', "duckduckgo@browser", engine, 0, None).await;
    assert_rendered(daemon.search(None).await);
    let run = daemon.run_dir();

    let second = start_daemon(
        Some(&daemon.root.join("config.toml")),
        (Ipv4Addr::LOCALHOST, 0).into(),
        Some(BrowserSettings {
            cdp_port: 0,
            root: daemon.browser_root(),
        }),
    )
    .await;
    let error = second.err().expect("the second daemon must not start");
    assert!(
        error.contains(&format!(
            "{} is in use by another scour daemon",
            daemon.browser_root().display()
        )),
        "{error}"
    );
    assert!(run.is_dir(), "{}", run.display());
    assert_rendered(daemon.search(None).await);

    daemon.stop(libc::SIGTERM).await;
    daemon.assert_empty_root();
}

#[tokio::test]
async fn daemon_kill_stops_its_browser_and_the_next_start_cleans_the_root() {
    let _displays = DISPLAYS.lock().await;
    let engine = fixture_server().await;
    // Give a runner with no desktop an occupied socket path and lock to preserve.
    let _socket = ExistingX::create("/tmp/.X11-unix/X0");
    let _lock = ExistingX::create("/tmp/.X1-lock");
    let before = x_inodes();

    // Kill during the start, while fake Chrome waits for announce.
    let mut daemon = TestDaemon::start('k', "duckduckgo@browser", engine, 0, Some("unused")).await;
    let request = tokio::spawn(search(daemon.address, None));
    let (chrome, display) = poll("fake Chrome to start", Duration::from_secs(15), || {
        let line = fs::read_to_string(daemon.root.join("fake")).ok()?;
        let (pid, display) = line.trim().split_once(" :")?;
        Some((pid.parse().ok()?, display.to_string()))
    })
    .await;
    let group = unsafe { libc::getpgid(chrome) };
    assert!(group > 0);
    let xvfb = xvfb_pid(&display);
    let shim = shim_pid(&display);
    let run = daemon.run_dir();
    let lock = format!("/tmp/.X{display}-lock");
    let owner = fs::read_to_string(&lock).unwrap();
    assert_eq!(owner, format!("{:>10}\n", daemon.child.id()));

    request.abort();
    daemon.kill().await;
    wait_processes_stopped(group, xvfb, &display).await;
    if let Some(shim) = shim {
        assert_no_shim_container(shim).await;
    }
    assert!(run.is_dir(), "{}", run.display());
    assert_eq!(fs::read_to_string(&lock).unwrap(), owner);
    assert_inodes(&before);

    daemon.restart().await;
    daemon.assert_empty_root();
    assert!(
        !Path::new(&lock).exists(),
        "the start-up cleanup did not remove the X lock"
    );
    assert_inodes(&before);
    daemon.stop(libc::SIGTERM).await;

    // Kill after a real query.
    let mut daemon = TestDaemon::start('q', "duckduckgo@browser", engine, 0, None).await;
    assert_rendered(daemon.search(None).await);
    let run = daemon.run_dir();
    let (_, group, display, xvfb) = browser_process(&run);
    let shim = shim_pid(&display);
    let lock = format!("/tmp/.X{display}-lock");
    let owner = fs::read_to_string(&lock).unwrap();

    daemon.kill().await;
    wait_processes_stopped(group, xvfb, &display).await;
    if let Some(shim) = shim {
        assert_no_shim_container(shim).await;
    }
    assert!(run.is_dir(), "{}", run.display());
    assert_eq!(fs::read_to_string(&lock).unwrap(), owner);
    assert_inodes(&before);

    daemon.restart().await;
    daemon.assert_empty_root();
    assert!(
        !Path::new(&lock).exists(),
        "the start-up cleanup did not remove the X lock"
    );
    assert_inodes(&before);
    daemon.stop(libc::SIGTERM).await;
}

#[tokio::test]
async fn a_symlinked_browser_root_exits_one_and_keeps_the_target() {
    let root = PathBuf::from(format!("/tmp/s{:x}symlink", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root).unwrap();
    fs::write(root.join("config.toml"), "engines = [\"duckduckgo\"]\n").unwrap();
    let target = root.join("target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("keep"), "fixture").unwrap();
    let link = root.join("b");
    symlink(&target, &link).unwrap();

    let error = start_daemon(
        Some(&root.join("config.toml")),
        (Ipv4Addr::LOCALHOST, 0).into(),
        Some(BrowserSettings {
            cdp_port: 0,
            root: link,
        }),
    )
    .await
    .err()
    .expect("a symlinked browser root must not start");
    assert!(error.contains("is not a directory of this user"), "{error}");
    assert_eq!(fs::read_to_string(target.join("keep")).unwrap(), "fixture");
    fs::remove_dir_all(&root).unwrap();
}

struct TestDaemon {
    root: PathBuf,
    child: Child,
    address: SocketAddr,
    cdp_port: u16,
    endpoint: Option<String>,
}

#[derive(Default)]
struct StartOptions<'a> {
    port: u16,
    endpoint: Option<&'a str>,
    idle: Option<u32>,
    extra: Option<&'a str>,
    chrome_delay_seconds: Option<u32>,
}

impl TestDaemon {
    async fn start(
        label: char,
        engine: &str,
        address: SocketAddr,
        port: u16,
        endpoint: Option<&str>,
    ) -> Self {
        Self::start_url(
            label,
            engine,
            format!("http://{address}"),
            StartOptions {
                port,
                endpoint,
                ..Default::default()
            },
        )
        .await
    }

    async fn start_idle(
        label: char,
        engine: &str,
        address: SocketAddr,
        port: u16,
        endpoint: Option<&str>,
        idle: u32,
    ) -> Self {
        Self::start_url(
            label,
            engine,
            format!("http://{address}"),
            StartOptions {
                port,
                endpoint,
                idle: Some(idle),
                ..Default::default()
            },
        )
        .await
    }

    async fn start_url(
        label: char,
        engine: &str,
        base_url: String,
        StartOptions {
            port,
            endpoint,
            idle,
            extra,
            chrome_delay_seconds,
        }: StartOptions<'_>,
    ) -> Self {
        // Chrome builds a Unix socket path under TMPDIR, which must fit in sun_path.
        let root = PathBuf::from(format!("/tmp/s{:x}{label}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("b")).unwrap();
        let name = engine.split('@').next().unwrap();
        let browser = idle
            .map(|seconds| format!("[browser]\nidle_seconds = {seconds}\n"))
            .unwrap_or_default();
        let extra = extra.map(|line| format!("{line}\n")).unwrap_or_default();
        fs::write(
            root.join("config.toml"),
            format!(
                "engines = [\"{engine}\"]\n{browser}{extra}[testing.base_urls]\n{name} = \"{base_url}\"\n"
            ),
        )
        .unwrap();
        let chrome_script = if endpoint.is_some() {
            Some(
                r#"#!/bin/sh
cd "$SCOUR_TEST_BROWSER_ROOT" || exit 1
printf '%s %s\n' "$$" "$DISPLAY" > fake
while [ ! -f announce ]; do sleep 0.05; done
printf 'DevTools listening on %s\n' "$SCOUR_TEST_CDP_ENDPOINT" >&2
exec sleep 60
"#
                .to_owned(),
            )
        } else {
            chrome_delay_seconds.map(|seconds| {
                format!("#!/bin/sh\nsleep {seconds}\nexec \"$SCOUR_TEST_REAL_CHROME\" \"$@\"\n")
            })
        };
        if let Some(script) = chrome_script {
            let chrome = root.join("chrome");
            fs::write(&chrome, script).unwrap();
            fs::set_permissions(&chrome, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let child = Self::command(&root, port, endpoint).spawn().unwrap();
        let mut daemon = Self {
            root,
            child,
            address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            cdp_port: port,
            endpoint: endpoint.map(str::to_string),
        };
        daemon.address = daemon.wait_address().await;
        daemon
    }

    fn command(root: &Path, port: u16, endpoint: Option<&str>) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "daemon_tests::browser::signal_shutdown_reaps_chrome_and_preserves_existing_x_displays", "--nocapture"])
            .env(CHILD_ROOT, root).env(CHILD_PORT, port.to_string())
            .stdout(Stdio::null());
        if let Some(endpoint) = endpoint {
            command
                .env("CHROME", root.join("chrome"))
                .env(CHILD_ENDPOINT, endpoint);
        } else if root.join("chrome").is_file() {
            let executable = chromiumoxide::detection::default_executable(
                chromiumoxide::detection::DetectionOptions {
                    msedge: false,
                    unstable: false,
                },
            )
            .expect("Chrome for the delayed start");
            command
                .env("CHROME", root.join("chrome"))
                .env("SCOUR_TEST_REAL_CHROME", executable);
        }
        command
    }

    async fn wait_address(&mut self) -> SocketAddr {
        poll("daemon address", Duration::from_secs(10), || {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "daemon exited at startup"
            );
            fs::read_to_string(self.root.join("address"))
                .ok()?
                .parse()
                .ok()
        })
        .await
    }

    async fn restart(&mut self) {
        let _ = fs::remove_file(self.root.join("address"));
        self.child = Self::command(&self.root, self.cdp_port, self.endpoint.as_deref())
            .spawn()
            .unwrap();
        self.address = self.wait_address().await;
    }

    async fn kill(&mut self) {
        assert_eq!(
            unsafe { libc::kill(self.child.id() as i32, libc::SIGKILL) },
            0
        );
        poll("daemon to die", Duration::from_secs(10), || {
            self.child.try_wait().unwrap()
        })
        .await;
    }

    async fn search(&self, selection: Option<&str>) -> SearchResponse {
        search(self.address, selection).await
    }

    fn browser_root(&self) -> PathBuf {
        self.root.join("b")
    }

    fn assert_empty_root(&self) {
        assert!(fs::read_dir(self.browser_root()).unwrap().next().is_none());
    }

    fn run_dir(&self) -> PathBuf {
        fs::read_dir(self.browser_root())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }

    async fn stop(&mut self, signal: libc::c_int) {
        assert_eq!(unsafe { libc::kill(self.child.id() as i32, signal) }, 0);
        let status = poll("daemon to stop", Duration::from_secs(10), || {
            self.child.try_wait().unwrap()
        })
        .await;
        assert!(status.success(), "{status}");
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if self.child.try_wait().is_ok_and(|status| status.is_none()) {
            unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while self.child.try_wait().is_ok_and(|status| status.is_none()) {
                if std::time::Instant::now() >= deadline {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct TlsFixture {
    root: PathBuf,
    child: Child,
    address: SocketAddr,
}

impl TlsFixture {
    async fn start() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = PathBuf::from(format!("/tmp/t{:x}-{nanos}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let cert = root.join("cert.pem");
        let key = root.join("key.pem");
        let generated = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=127.0.0.1",
                "-addext",
                "subjectAltName=IP:127.0.0.1",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("cannot run openssl");
        assert!(generated.success(), "openssl could not make a certificate");
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let mut child = Command::new("openssl")
            .args(["s_server", "-accept"])
            .arg(format!("127.0.0.1:{}", address.port()))
            .arg("-cert")
            .arg(&cert)
            .arg("-key")
            .arg(&key)
            .args(["-www", "-quiet"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("cannot run openssl s_server");
        poll("the certificate server", Duration::from_secs(5), || {
            assert!(
                child.try_wait().unwrap().is_none(),
                "openssl s_server exited"
            );
            std::net::TcpStream::connect_timeout(&address, Duration::from_millis(100))
                .ok()
                .map(|_| ())
        })
        .await;
        Self {
            root,
            child,
            address,
        }
    }
}

impl Drop for TlsFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

async fn poll<T>(what: &str, limit: Duration, mut ready: impl FnMut() -> Option<T>) -> T {
    timeout(limit, async {
        loop {
            if let Some(value) = ready() {
                return value;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

async fn search(address: SocketAddr, selection: Option<&str>) -> SearchResponse {
    search_query(address, QUERY, selection).await
}

async fn search_query(address: SocketAddr, query: &str, selection: Option<&str>) -> SearchResponse {
    let mut url = Url::parse(&format!("http://{address}/search")).unwrap();
    url.query_pairs_mut().append_pair("q", query);
    if let Some(selection) = selection {
        url.query_pairs_mut().append_pair("engines", selection);
    }
    wreq::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(45))
        .build()
        .unwrap()
        .get(url.as_str())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn fixture_server() -> SocketAddr {
    serve(Router::new().route(
        "/html/",
        get(|| async {
            (
                [(header::CACHE_CONTROL, "public, max-age=600")],
                Html(FIXTURE),
            )
        }),
    ))
    .await
}

async fn budget_fixture_server() -> (SocketAddr, Arc<AtomicUsize>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let address = serve(Router::new().route(
        "/html/",
        get(move |Query(query): Query<BTreeMap<String, String>>| {
            let count = count.clone();
            async move {
                if query.get("q").is_some_and(|query| query == HUNG_QUERY) {
                    count.fetch_add(1, Ordering::Relaxed);
                    std::future::pending().await
                } else {
                    Html("<div class=\"result\"><a class=\"result__a\" href=\"https://example.test/\">fixture</a></div>")
                }
            }
        }),
    ))
    .await;
    (address, requests)
}

async fn slow_fixture_server() -> (SocketAddr, Arc<AtomicUsize>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let address = serve(Router::new().route(
        "/html/",
        get(move || {
            count.fetch_add(1, Ordering::Relaxed);
            async {
                sleep(Duration::from_secs(30)).await;
                (
                    [(header::CACHE_CONTROL, "public, max-age=600")],
                    Html(FIXTURE),
                )
            }
        }),
    ))
    .await;
    (address, requests)
}

async fn serve(app: Router) -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
}

async fn dual_stack_fixture() -> (SocketAddr, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let (ipv4, address, ipv6) = loop {
        let ipv4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = ipv4.local_addr().unwrap();
        match TcpListener::bind((Ipv6Addr::LOCALHOST, address.port())).await {
            Ok(ipv6) => break (ipv4, address, ipv6),
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => continue,
            Err(err) => panic!("cannot bind the IPv6 fixture: {err}"),
        }
    };
    let ipv4_requests = Arc::new(AtomicUsize::new(0));
    let ipv6_requests = Arc::new(AtomicUsize::new(0));
    let ipv4_app = Router::new().route(
        "/html/",
        get({
            let requests = ipv4_requests.clone();
            move || {
                requests.fetch_add(1, Ordering::SeqCst);
                async {
                    (
                        [(header::CACHE_CONTROL, "public, max-age=600")],
                        Html(FIXTURE),
                    )
                }
            }
        }),
    );
    let ipv6_app = Router::new().route(
        "/html/",
        get({
            let requests = ipv6_requests.clone();
            move || {
                requests.fetch_add(1, Ordering::SeqCst);
                async {
                    (
                        [(header::CACHE_CONTROL, "public, max-age=600")],
                        Html(FIXTURE),
                    )
                }
            }
        }),
    );
    tokio::spawn(async move { axum::serve(ipv4, ipv4_app).await.unwrap() });
    tokio::spawn(async move { axum::serve(ipv6, ipv6_app).await.unwrap() });
    (address, ipv4_requests, ipv6_requests)
}

#[derive(Clone, Copy)]
enum ProxyKind {
    Http,
    Socks5,
}

struct TestProxy {
    address: SocketAddr,
    targets: Arc<Mutex<Vec<String>>>,
}

async fn test_proxy(kind: ProxyKind, target: SocketAddr) -> TestProxy {
    test_proxy_on(Ipv4Addr::LOCALHOST.into(), kind, target).await
}

async fn test_proxy_on(bind: IpAddr, kind: ProxyKind, target: SocketAddr) -> TestProxy {
    let listener = TcpListener::bind((bind, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let targets = Arc::new(Mutex::new(Vec::new()));
    let log = targets.clone();
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else {
                break;
            };
            let log = log.clone();
            tokio::spawn(async move {
                match kind {
                    ProxyKind::Http => {
                        let _ = http_proxy(client, target, log).await;
                    }
                    ProxyKind::Socks5 => {
                        let _ = socks_proxy(client, target, log).await;
                    }
                }
            });
        }
    });
    TestProxy { address, targets }
}

async fn http_proxy(
    mut client: TcpStream,
    target: SocketAddr,
    targets: Arc<Mutex<Vec<String>>>,
) -> io::Result<()> {
    let head = read_request_head(&mut client).await?;
    let text = String::from_utf8(head)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "proxy request"))?;
    let mut lines = text.split("\r\n");
    let mut parts = lines.next().unwrap_or_default().split(' ');
    let method = parts.next().unwrap_or_default();
    let request_target = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if method == "CONNECT" {
        let Some((host, port)) = request_target.rsplit_once(':') else {
            return Ok(());
        };
        targets.lock().await.push(format!("{host}:{port}"));
        if !host.ends_with(".scour.test") {
            client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await?;
            return Ok(());
        }
        let mut upstream = TcpStream::connect(target).await?;
        client.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await?;
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        return Ok(());
    }

    let url = Url::parse(request_target)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "proxy target"))?;
    let host = url.host_str().unwrap_or_default().to_string();
    let port = url.port_or_known_default().unwrap_or(80);
    targets.lock().await.push(format!("{host}:{port}"));
    if !host.ends_with(".scour.test") {
        client
            .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            .await?;
        return Ok(());
    }
    let path = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    let mut request = format!("{method} {path} {version}\r\n");
    for line in lines {
        if line.is_empty() {
            break;
        }
        if line.split_once(':').is_some_and(|(name, _)| {
            name.eq_ignore_ascii_case("connection") || name.eq_ignore_ascii_case("proxy-connection")
        }) {
            continue;
        }
        request.push_str(line);
        request.push_str("\r\n");
    }
    // Close the upstream connection after one response, so Chrome logs every request
    // on a new proxy connection.
    request.push_str("Connection: close\r\n\r\n");
    let mut upstream = TcpStream::connect(target).await?;
    upstream.write_all(request.as_bytes()).await?;
    let _ = tokio::io::copy(&mut upstream, &mut client).await;
    Ok(())
}

async fn socks_proxy(
    mut client: TcpStream,
    target: SocketAddr,
    targets: Arc<Mutex<Vec<String>>>,
) -> io::Result<()> {
    let mut greeting = [0u8; 2];
    client.read_exact(&mut greeting).await?;
    let mut methods = vec![0u8; greeting[1] as usize];
    client.read_exact(&mut methods).await?;
    client.write_all(&[5, 0]).await?;

    let mut head = [0u8; 4];
    client.read_exact(&mut head).await?;
    let (host, port) = match head[3] {
        1 => {
            let mut addr = [0u8; 4];
            client.read_exact(&mut addr).await?;
            let mut port = [0u8; 2];
            client.read_exact(&mut port).await?;
            (Ipv4Addr::from(addr).to_string(), u16::from_be_bytes(port))
        }
        3 => {
            let mut len = [0u8; 1];
            client.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            client.read_exact(&mut name).await?;
            let mut port = [0u8; 2];
            client.read_exact(&mut port).await?;
            (
                String::from_utf8_lossy(&name).into_owned(),
                u16::from_be_bytes(port),
            )
        }
        4 => {
            let mut addr = [0u8; 16];
            client.read_exact(&mut addr).await?;
            let mut port = [0u8; 2];
            client.read_exact(&mut port).await?;
            (Ipv6Addr::from(addr).to_string(), u16::from_be_bytes(port))
        }
        _ => {
            client.write_all(&[5, 8, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            return Ok(());
        }
    };
    targets.lock().await.push(format!("{host}:{port}"));
    if !host.ends_with(".scour.test") {
        client.write_all(&[5, 2, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Ok(());
    }
    let mut upstream = TcpStream::connect(target).await?;
    client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

async fn read_request_head(client: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        client.read_exact(&mut byte).await?;
        head.push(byte[0]);
        if head.len() > 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "proxy request is too large",
            ));
        }
    }
    Ok(head)
}

fn assert_chrome_tcp_peers_are_loopback(group: i32) {
    // Collect the inodes first. A later /proc/net read then includes every socket
    // that Chrome had when its process group was scanned.
    let inodes: Vec<_> = group_pids(group)
        .into_iter()
        .flat_map(|pid| {
            socket_inodes(pid)
                .into_iter()
                .map(move |inode| (pid, inode))
        })
        .collect();
    let sockets = tcp_sockets();
    let mut checked = 0;
    for (pid, inode) in inodes {
        let Some((peer, state)) = sockets.get(&inode) else {
            continue;
        };
        // The CDP listener has no peer.
        if *state == 0x0a {
            continue;
        }
        assert!(
            is_loopback_peer(peer),
            "process {pid} socket {inode} has peer {peer}"
        );
        checked += 1;
    }
    assert!(checked > 0, "no TCP sockets in Chrome group {group}");
}

fn group_pids(group: i32) -> Vec<i32> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_string_lossy().parse().ok())
        .filter(|pid| process_group(*pid) == Some(group))
        .collect()
}

fn process_group(pid: i32) -> Option<i32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let mut fields = rest.split_whitespace();
    fields.next()?;
    fields.next()?;
    fields.next()?.parse().ok()
}

fn socket_inodes(pid: i32) -> BTreeSet<u64> {
    let mut inodes = BTreeSet::new();
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return inodes;
    };
    for entry in entries.flatten() {
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        let Some(inode) = target
            .to_str()
            .and_then(|target| target.strip_prefix("socket:["))
            .and_then(|target| target.strip_suffix(']'))
        else {
            continue;
        };
        if let Ok(inode) = inode.parse() {
            inodes.insert(inode);
        }
    }
    inodes
}

fn tcp_sockets() -> BTreeMap<u64, (String, u8)> {
    let mut sockets = BTreeMap::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(table) = fs::read_to_string(path) else {
            continue;
        };
        for line in table.lines().skip(1) {
            let fields: Vec<_> = line.split_whitespace().collect();
            let Some(inode) = fields.get(9).and_then(|inode| inode.parse().ok()) else {
                continue;
            };
            let Some(peer) = fields
                .get(2)
                .and_then(|peer| peer.rsplit_once(':'))
                .map(|(address, _)| address)
            else {
                continue;
            };
            let state = fields
                .get(3)
                .and_then(|state| u8::from_str_radix(state, 16).ok())
                .unwrap_or(0);
            sockets.insert(inode, (peer.to_string(), state));
        }
    }
    sockets
}

fn is_loopback_peer(peer: &str) -> bool {
    match peer.len() {
        8 => u32::from_str_radix(peer, 16).is_ok_and(|address| address & 0xff == 0x7f),
        32 => {
            let peer = peer.to_ascii_uppercase();
            peer == "00000000000000000000000001000000"
                || (peer.starts_with("0000000000000000FFFF0000") && peer.ends_with("0100007F"))
        }
        _ => false,
    }
}

fn assert_rendered(response: SearchResponse) {
    assert_eq!(response.engines[0].method, Method::Browser, "{response:?}");
    assert_eq!(response.engines[0].state, EngineState::Ok, "{response:?}");
    assert_eq!(response.results[0].title, TITLE);
}

fn assert_start_failure(response: SearchResponse, error: &str) {
    assert_eq!(
        response.engines[0].state,
        EngineState::Error,
        "{response:?}"
    );
    assert_eq!(response.engines[0].error.as_deref(), Some(error));
    assert_eq!(response.engines[0].daily_count, 0);
}

fn assert_no_query_data(root: &Path) {
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            assert_no_query_data(&path);
        } else if kind.is_file() {
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => panic!("{}: {err}", path.display()),
            };
            for marker in [QUERY, TITLE] {
                let utf16: Vec<_> = marker.encode_utf16().flat_map(u16::to_le_bytes).collect();
                for pattern in [marker.as_bytes(), &utf16] {
                    assert!(
                        !bytes.windows(pattern.len()).any(|window| window == pattern),
                        "browser stored fixture data in {}",
                        path.display()
                    );
                }
            }
        }
    }
}

fn listeners(path: &str, port: u16) -> Vec<String> {
    let table = match fs::read_to_string(path) {
        Ok(table) => table,
        Err(err) if path.ends_with("tcp6") && err.kind() == io::ErrorKind::NotFound => {
            return vec![];
        }
        Err(err) => panic!("{path}: {err}"),
    };
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let (address, listen_port) = fields.nth(1)?.split_once(':')?;
            (fields.nth(1)? == "0A" && u16::from_str_radix(listen_port, 16).ok() == Some(port))
                .then(|| address.to_owned())
        })
        .collect()
}

struct ExistingX {
    path: &'static str,
    inode: u64,
}

impl ExistingX {
    fn create(path: &'static str) -> Option<Self> {
        match fs::File::create_new(path) {
            Ok(mut file) => {
                let fixture = Self {
                    path,
                    inode: file.metadata().unwrap().ino(),
                };
                writeln!(file, "{:>10}", std::process::id()).unwrap();
                Some(fixture)
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::NotFound
                ) =>
            {
                None
            }
            Err(err) => panic!("{path}: {err}"),
        }
    }
}

impl Drop for ExistingX {
    fn drop(&mut self) {
        if fs::symlink_metadata(self.path).is_ok_and(|meta| meta.ino() == self.inode) {
            fs::remove_file(self.path).unwrap();
        }
    }
}

fn x_inodes() -> BTreeMap<PathBuf, u64> {
    let mut inodes = BTreeMap::new();
    for (dir, prefix, suffix) in [("/tmp/.X11-unix", "X", ""), ("/tmp", ".X", "-lock")] {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => panic!("{dir}: {err}"),
        };
        for entry in entries {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(prefix) && name.ends_with(suffix) {
                inodes.insert(
                    entry.path(),
                    fs::symlink_metadata(entry.path()).unwrap().ino(),
                );
            }
        }
    }
    inodes
}

fn assert_inodes(before: &BTreeMap<PathBuf, u64>) {
    let current = x_inodes();
    for (path, inode) in before {
        assert_eq!(current.get(path), Some(inode), "{}", path.display());
    }
}

fn find_pid_opt(matches: impl Fn(&[&str]) -> bool) -> Option<i32> {
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse() else {
            continue;
        };
        let Ok(line) = fs::read_to_string(entry.path().join("cmdline")) else {
            continue;
        };
        // Chromium can rewrite its NUL-separated arguments as one process title.
        let args: Vec<_> = line
            .split(|c: char| c == '\0' || c.is_ascii_whitespace())
            .filter(|arg| !arg.is_empty())
            .collect();
        if matches(&args) {
            return Some(pid);
        }
    }
    None
}

#[track_caller]
fn find_pid(matches: impl Fn(&[&str]) -> bool) -> i32 {
    find_pid_opt(matches).expect("could not find the running browser or Xvfb process")
}

fn browser_pid(run: &Path) -> Option<i32> {
    let profile = format!("--user-data-dir={}", run.join("profile").display());
    find_pid_opt(|args| {
        args.contains(&profile.as_str()) && !args.iter().any(|arg| arg.starts_with("--type="))
    })
}

#[track_caller]
fn xvfb_pid(display: &str) -> i32 {
    let display = format!(":{display}");
    find_pid(|args| {
        args.first()
            .and_then(|exe| Path::new(exe).file_name())
            .is_some_and(|exe| exe == "Xvfb")
            && args.get(1) == Some(&display.as_str())
    })
}

fn browser_process(run: &Path) -> (i32, i32, String, i32) {
    let profile = format!("--user-data-dir={}", run.join("profile").display());
    let chrome = find_pid(|args| {
        args.contains(&profile.as_str()) && !args.iter().any(|arg| arg.starts_with("--type="))
    });
    let group = unsafe { libc::getpgid(chrome) };
    assert!(group > 0);
    let display = fs::read_to_string(run.join("display"))
        .unwrap()
        .trim()
        .to_string();
    let xvfb = xvfb_pid(&display);
    (chrome, group, display, xvfb)
}

fn processes_stopped(chrome: i32, xvfb: i32, display: &str) -> bool {
    let group_gone = unsafe { libc::kill(-chrome, 0) } == -1
        && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    group_gone
        && !Path::new(&format!("/proc/{xvfb}")).exists()
        && !Path::new(&format!("/tmp/.X11-unix/X{display}")).exists()
}

async fn wait_processes_stopped(chrome: i32, xvfb: i32, display: &str) {
    poll(
        "the browser processes to stop",
        Duration::from_secs(10),
        || processes_stopped(chrome, xvfb, display).then_some(()),
    )
    .await;
}

async fn wait_stopped(chrome: i32, xvfb: i32, display: &str) {
    wait_processes_stopped(chrome, xvfb, display).await;
    let lock = format!("/tmp/.X{display}-lock");
    poll("the X lock to go", Duration::from_secs(5), || {
        fs::symlink_metadata(&lock)
            .is_err_and(|err| err.kind() == io::ErrorKind::NotFound)
            .then_some(())
    })
    .await;
    assert_stopped(chrome, xvfb, display);
}

fn assert_stopped(chrome: i32, xvfb: i32, display: &str) {
    assert!(
        processes_stopped(chrome, xvfb, display),
        "Chrome group {chrome} or Xvfb {xvfb} remains"
    );
    for path in [
        format!("/tmp/.X{display}-lock"),
        format!("/tmp/.X11-unix/X{display}"),
    ] {
        assert_eq!(
            fs::symlink_metadata(&path).unwrap_err().kind(),
            io::ErrorKind::NotFound,
            "{path}"
        );
    }
}

fn shim_pid(display: &str) -> Option<i32> {
    let display = format!(":{display}");
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse() else {
            continue;
        };
        let Ok(line) = fs::read_to_string(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<_> = line
            .split(|c: char| c == '\0' || c.is_ascii_whitespace())
            .filter(|arg| !arg.is_empty())
            .collect();
        if args.iter().any(|arg| arg.ends_with("xvfb-docker/Xvfb"))
            && args.contains(&display.as_str())
        {
            return Some(pid);
        }
    }
    None
}

fn shim_containers(xvfb: i32) -> Option<Vec<String>> {
    let output = Command::new("docker")
        .args([
            "ps",
            "-aq",
            "--filter",
            &format!("name=^/scour-check-xvfb-{xvfb}-"),
        ])
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    })
}

async fn assert_no_shim_container(xvfb: i32) {
    poll("the shim container to go", Duration::from_secs(10), || {
        shim_containers(xvfb).filter(Vec::is_empty).map(|_| ())
    })
    .await;
}
