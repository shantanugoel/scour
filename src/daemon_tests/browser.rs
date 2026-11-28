use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};
use url::Url;

use crate::api::{EngineState, SearchResponse};
use crate::browser::BrowserSettings;
use crate::engines::Method;
use crate::{cli, start_daemon};

const CHILD_ROOT: &str = "SCOUR_TEST_BROWSER_ROOT";
const CHILD_PORT: &str = "SCOUR_TEST_CDP_PORT";
const CHILD_ENDPOINT: &str = "SCOUR_TEST_CDP_ENDPOINT";
const QUERY: &str = "scour-query-fixture-v6xM43sw";
const TITLE: &str = "scour-result-fixture-B7zA55tj";
const FIXTURE: &str = r#"<!doctype html><body>
<script>
document.body.insertAdjacentHTML('beforeend',
    '<div class="result"><a class="result__a" href="https://example.test/">scour-result-fixture-B7zA55tj</a></div>');
</script>"#;

// A test that snapshots X paths must see a stable set of other test displays.
static DISPLAYS: Mutex<()> = Mutex::const_new(());

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
        0,
        None,
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

struct TestDaemon {
    root: PathBuf,
    child: Child,
    address: SocketAddr,
}

impl TestDaemon {
    async fn start(
        label: char,
        engine: &str,
        address: SocketAddr,
        port: u16,
        endpoint: Option<&str>,
    ) -> Self {
        Self::start_url(label, engine, format!("http://{address}"), port, endpoint).await
    }

    async fn start_url(
        label: char,
        engine: &str,
        base_url: String,
        port: u16,
        endpoint: Option<&str>,
    ) -> Self {
        // Chrome builds a Unix socket path under TMPDIR, which must fit in sun_path.
        let root = PathBuf::from(format!("/tmp/s{:x}{label}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("b")).unwrap();
        let name = engine.split('@').next().unwrap();
        fs::write(
            root.join("config.toml"),
            format!("engines = [\"{engine}\"]\n[testing.base_urls]\n{name} = \"{base_url}\"\n"),
        )
        .unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "daemon_tests::browser::signal_shutdown_reaps_chrome_and_preserves_existing_x_displays", "--nocapture"])
            .env(CHILD_ROOT, &root).env(CHILD_PORT, port.to_string())
            .stdout(Stdio::null());
        if let Some(endpoint) = endpoint {
            let chrome = root.join("chrome");
            fs::write(
                &chrome,
                r#"#!/bin/sh
cd "$SCOUR_TEST_BROWSER_ROOT" || exit 1
printf '%s %s\n' "$$" "$DISPLAY" > fake
while [ ! -f announce ]; do sleep 0.05; done
printf 'DevTools listening on %s\n' "$SCOUR_TEST_CDP_ENDPOINT" >&2
exec sleep 60
"#,
            )
            .unwrap();
            fs::set_permissions(&chrome, fs::Permissions::from_mode(0o700)).unwrap();
            command.env("CHROME", chrome).env(CHILD_ENDPOINT, endpoint);
        }
        let mut daemon = Self {
            root,
            child: command.spawn().unwrap(),
            address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        };
        daemon.address = poll("daemon address", Duration::from_secs(10), || {
            assert!(
                daemon.child.try_wait().unwrap().is_none(),
                "daemon exited at startup"
            );
            fs::read_to_string(daemon.root.join("address"))
                .ok()?
                .parse()
                .ok()
        })
        .await;
        daemon
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
    let mut url = Url::parse(&format!("http://{address}/search")).unwrap();
    url.query_pairs_mut().append_pair("q", QUERY);
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

async fn serve(app: Router) -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
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
    assert_eq!(response.engines[0].daily_count, 1);
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

#[track_caller]
fn find_pid(matches: impl Fn(&[&str]) -> bool) -> i32 {
    for entry in fs::read_dir("/proc").unwrap().flatten() {
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
            return pid;
        }
    }
    panic!("could not find the running browser or Xvfb process");
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

fn assert_stopped(chrome: i32, xvfb: i32, display: &str) {
    assert_eq!(
        unsafe { libc::kill(-chrome, 0) },
        -1,
        "Chrome group {chrome} remains"
    );
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    assert!(
        !Path::new(&format!("/proc/{xvfb}")).exists(),
        "Xvfb {xvfb} remains"
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
