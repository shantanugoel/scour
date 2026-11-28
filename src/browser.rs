use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chromiumoxide::Browser as CdpBrowser;
use chromiumoxide::detection::{DetectionOptions, default_executable};
use chromiumoxide::error::CdpError;
use chromiumoxide::handler::HandlerConfig;
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::{Mutex, OwnedMutexGuard, oneshot};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep, sleep_until, timeout_at};
use url::Url;

use crate::search::TIMEOUT;
use crate::socks::Forwarder;

pub const CDP_PORT: u16 = 7269;
const POLL: Duration = Duration::from_millis(50);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const XVFB_START: Duration = Duration::from_secs(30);
const CHROME_START: Duration = Duration::from_secs(30);
// Chrome appends this to TMPDIR, and sun_path holds 108 bytes including the NUL.
const CHROME_TMP_SUFFIX: &str = "/org.chromium.Chromium.XXXXXX/SingletonSocket";

pub struct BrowserSettings {
    pub cdp_port: u16,
    pub root: PathBuf,
}

impl BrowserSettings {
    pub fn fixed() -> Self {
        Self {
            cdp_port: CDP_PORT,
            root: PathBuf::from(format!("/dev/shm/scour-{}", unsafe { libc::geteuid() })),
        }
    }
}

#[derive(Deserialize)]
pub struct Rendered {
    pub status: u16,
    pub html: String,
}

pub struct Browser {
    settings: BrowserSettings,
    idle: Duration,
    proxy: Option<Url>,
    state: Arc<Mutex<State>>,
    // Drop the browser state before the root lock, so a new daemon cannot clean under a teardown.
    _root: File,
}

enum State {
    Idle,
    Running(Session),
    Stopped,
}

struct Session {
    // Dropping this aborts the supervisor and the CDP handler.
    _supervisor: JoinSet<()>,
    cdp: Arc<CdpBrowser>,
    last_used: Instant,
    _chrome: Group,
    _forwarder: Option<Forwarder>,
    _dir: RunDir,
    _xvfb: Group,
    _display: DisplayLock,
}

impl Browser {
    pub fn open(
        settings: BrowserSettings,
        idle: Duration,
        proxy: Option<Url>,
    ) -> Result<Self, String> {
        let root = open_root(&settings.root)?;
        Ok(Self {
            settings,
            idle,
            proxy,
            state: Arc::new(Mutex::new(State::Idle)),
            _root: root,
        })
    }

    pub async fn fetch(&self, url: &Url) -> Result<Rendered, String> {
        let host = url.host_str().unwrap_or("the engine");
        let proxied = self.proxy.is_some();
        let slow = || format!("no answer from {host} in {} s", TIMEOUT.as_secs());
        let cdp_error = |err| match err {
            CdpError::Timeout => slow(),
            _ => "lost the connection to Chrome".into(),
        };
        let mut retried = false;
        let (_cdp, page, deadline) = loop {
            let (cdp, started) = {
                let mut state = self.state.clone().lock_owned().await;
                if let State::Running(session) = &mut *state
                    && session.chrome_exited()
                {
                    end(state, State::Idle).await;
                    state = self.state.clone().lock_owned().await;
                }
                let started = matches!(*state, State::Idle);
                if started {
                    let session = Session::start(
                        &self.settings,
                        Arc::downgrade(&self.state),
                        self.idle,
                        self.proxy.as_ref(),
                    )
                    .await?;
                    *state = State::Running(session);
                }
                match &mut *state {
                    State::Running(session) => {
                        session.last_used = Instant::now();
                        (session.cdp.clone(), started)
                    }
                    State::Stopped => return Err("the browser is stopping".into()),
                    State::Idle => unreachable!(),
                }
            };
            let deadline = Instant::now() + TIMEOUT;
            match timeout_at(deadline, cdp.new_page("about:blank")).await {
                Ok(Ok(page)) => break (cdp, page, deadline),
                Err(_) | Ok(Err(CdpError::Timeout)) => return Err(slow()),
                Ok(Err(err)) if started || retried => return Err(cdp_error(err)),
                Ok(Err(_)) => {
                    // Chrome can close CDP before try_wait sees its exit. End that session if it
                    // is still there and open one new session; an error on the new session is final.
                    let state = self.state.clone().lock_owned().await;
                    if matches!(&*state, State::Running(session) if Arc::ptr_eq(&session.cdp, &cdp))
                    {
                        end(state, State::Idle).await;
                    }
                    retried = true;
                }
            }
        };
        let rendered = timeout_at(deadline, async {
            page.goto(url.as_str()).await.map_err(|err| match err {
                CdpError::ChromeMessage(text) => connect_error(&text, host, proxied),
                _ => cdp_error(err),
            })?;
            let snapshot = page.evaluate(r#"({status: performance.getEntriesByType('navigation')[0]?.responseStatus ?? 0, html: document.documentElement?.outerHTML ?? ""})"#)
                .await.map_err(cdp_error)?;
            snapshot.into_value().map_err(|_| "cannot read Chrome's rendered page".to_string())
        }).await;
        // A timed-out page still needs a close request.
        let closed = timeout_at(deadline, tokio::spawn(page.close())).await;
        let rendered = rendered.map_err(|_| slow())??;
        closed
            .map_err(|_| slow())?
            .map_err(|_| "lost the connection to Chrome")?
            .map_err(cdp_error)?;
        Ok(rendered)
    }

    pub async fn stop(&self) {
        end(self.state.clone().lock_owned().await, State::Stopped).await;
    }
}

impl Session {
    async fn start(
        settings: &BrowserSettings,
        state: Weak<Mutex<State>>,
        idle: Duration,
        proxy: Option<&Url>,
    ) -> Result<Self, String> {
        let port = settings.cdp_port;
        if port != 0 {
            TcpListener::bind((Ipv4Addr::LOCALHOST, port)).map_err(|err| match err.kind() {
                io::ErrorKind::AddrInUse => format!("127.0.0.1:{port} is in use"),
                _ => format!("cannot bind CDP at 127.0.0.1:{port}: {err}"),
            })?;
        }
        // Locals drop in reverse declaration order if startup fails.
        let display_lock;
        let mut xvfb;
        let run = RunDir::new(&settings.root)?;
        let mut forwarder = None;
        let mut chrome;
        let dir = &run.0;
        let deadline = Instant::now() + XVFB_START;
        let (n, lock) = DisplayLock::reserve()?;
        display_lock = lock;
        let display = format!(":{n}");
        let ready = dir.join("display");
        let cannot_start = |err| format!("cannot start Xvfb: {err}");
        let mut command = Command::new("Xvfb");
        command
            .arg(&display)
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "1366x768x24",
                "-nolisten",
                "tcp",
            ])
            .stdout(File::create(&ready).map_err(cannot_start)?)
            .stderr(Stdio::null());
        xvfb = Group::spawn(&mut command, libc::SIGTERM).map_err(cannot_start)?;
        timeout_at(deadline, async {
            loop {
                if let Some(status) = xvfb.0.try_wait().map_err(cannot_start)? {
                    return Err(format!("Xvfb exited with {status}"));
                }
                if fs::read_to_string(&ready).is_ok_and(|text| text.trim().parse() == Ok(n)) {
                    return Ok(());
                }
                sleep(POLL).await;
            }
        })
        .await
        .map_err(|_| format!("Xvfb did not start in {} s", XVFB_START.as_secs()))??;

        let executable = default_executable(DetectionOptions {
            msedge: false,
            unstable: false,
        })
        .map_err(|_| "cannot find Chrome; set CHROME or install chromium")?;
        let proxy_server = match proxy {
            Some(url) => chrome_proxy_server(url),
            None => {
                let started = crate::socks::start()
                    .await
                    .map_err(|err| format!("cannot start the IPv4 forwarder: {err}"))?;
                let server = format!("socks5://127.0.0.1:{}", started.port);
                forwarder = Some(started);
                server
            }
        };
        let resolver = resolver_rules(proxy);
        let deadline = Instant::now() + CHROME_START;
        let mut profile_arg = OsString::from("--user-data-dir=");
        profile_arg.push(dir.join("profile"));
        let mut command = Command::new(executable);
        command
            .env("DISPLAY", display)
            .env("TMPDIR", dir.join("tmp"))
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("XAUTHORITY")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .arg(format!("--remote-debugging-port={port}"))
            .arg(profile_arg)
            .arg(format!("--proxy-server={proxy_server}"))
            .arg("--proxy-bypass-list=<-loopback>")
            .arg(format!("--host-resolver-rules={resolver}"))
            .args([
                "--no-sandbox",
                "--remote-debugging-address=127.0.0.1",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                "--disable-background-networking",
                "--disable-features=AimEnabled,NetworkTimeServiceQuerying,OptimizationHints,PreconnectToSearch,SearchEnginePreconnect2,SafeBrowsingHashPrefixRealTimeLookups",
                "--enable-features=SkipListAccountsWithoutGaiaCookies",
                "--disable-sync",
                "--disable-component-update",
                "--disable-default-apps",
                "--safebrowsing-disable-auto-update",
                "--disable-gpu",
                "--lang=en-US",
                "--window-size=1366,768",
                "--password-store=basic",
                "--use-mock-keychain",
                "--ozone-platform=x11",
                "about:blank",
            ]);
        chrome = Group::spawn(&mut command, libc::SIGKILL)
            .map_err(|err| format!("cannot start Chrome: {err}"))?;
        let (cdp, handler) = connect(&mut chrome.0, port, deadline).await?;
        let cdp = Arc::new(cdp);
        let mut supervisor = JoinSet::new();
        supervisor.spawn(supervise(state, Arc::downgrade(&cdp), handler, idle));
        Ok(Self {
            _supervisor: supervisor,
            cdp,
            last_used: Instant::now(),
            _chrome: chrome,
            _forwarder: forwarder,
            _dir: run,
            _xvfb: xvfb,
            _display: display_lock,
        })
    }

    fn busy(&self) -> bool {
        // A query holds a clone for as long as it uses the browser.
        Arc::strong_count(&self.cdp) > 1
    }

    fn chrome_exited(&mut self) -> bool {
        self._chrome
            .0
            .try_wait()
            .is_ok_and(|status| status.is_some())
    }
}

fn chrome_proxy_server(url: &Url) -> String {
    let address = crate::config::proxy_address(url);
    if url.scheme() == "socks5h" {
        // Chrome rejects socks5h. Its socks5:// sends the host name to the proxy.
        address.replacen("socks5h://", "socks5://", 1)
    } else {
        address
    }
}

fn resolver_rules(proxy: Option<&Url>) -> String {
    let mut rules = String::from("MAP * ~NOTFOUND , EXCLUDE 127.0.0.1");
    if let Some(host) = proxy.map(proxy_host)
        && host != "127.0.0.1"
    {
        rules.push_str(&format!(" , EXCLUDE {host}"));
    }
    rules
}

fn proxy_host(url: &Url) -> String {
    let host = url.host_str().expect("validated proxy host");
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
        .to_string()
}

fn connect_error(text: &str, host: &str, proxied: bool) -> String {
    if text.contains("ERR_PROXY_CONNECTION_FAILED") {
        if proxied {
            "cannot connect to the proxy".to_string()
        } else {
            format!("cannot connect to {host}")
        }
    } else if text.contains("ERR_SOCKS_CONNECTION_FAILED")
        || (proxied && text.contains("ERR_TUNNEL_CONNECTION_FAILED"))
    {
        if proxied {
            format!("the proxy cannot connect to {host}")
        } else {
            format!("cannot connect to {host} over IPv4")
        }
    } else {
        format!("cannot connect to {host}")
    }
}

async fn supervise(
    state: Weak<Mutex<State>>,
    cdp: Weak<CdpBrowser>,
    mut handler: JoinSet<()>,
    idle: Duration,
) {
    loop {
        let Some(browser) = state.upgrade() else {
            return;
        };
        let deadline = {
            let state = browser.lock().await;
            match &*state {
                State::Running(session) if Weak::ptr_eq(&cdp, &Arc::downgrade(&session.cdp)) => {
                    if session.busy() {
                        // A query holds the CDP client. Check again after another idle time.
                        Instant::now() + idle
                    } else {
                        session.last_used + idle
                    }
                }
                _ => return,
            }
        };
        tokio::select! {
            _ = handler.join_next() => {
                let Some(browser) = state.upgrade() else {
                    return;
                };
                let guard = browser.lock_owned().await;
                if matches!(&*guard, State::Running(session) if Weak::ptr_eq(&cdp, &Arc::downgrade(&session.cdp)))
                {
                    end(guard, State::Idle).await;
                }
                return;
            }
            _ = sleep_until(deadline) => {
                let Some(browser) = state.upgrade() else {
                    return;
                };
                let guard = browser.lock_owned().await;
                match &*guard {
                    State::Running(session)
                        if Weak::ptr_eq(&cdp, &Arc::downgrade(&session.cdp))
                            && (session.busy()
                                || session.last_used + idle > Instant::now()) =>
                    {
                        drop(guard);
                        continue;
                    }
                    State::Running(session)
                        if Weak::ptr_eq(&cdp, &Arc::downgrade(&session.cdp)) =>
                    {
                        end(guard, State::Idle).await;
                        return;
                    }
                    _ => return,
                }
            }
        }
    }
}

async fn end(mut guard: OwnedMutexGuard<State>, next: State) {
    let old = std::mem::replace(&mut *guard, next);
    if let State::Running(session) = old {
        // Keep the lock until the old session is gone, so a new start never overlaps its teardown.
        let _ = tokio::task::spawn_blocking(move || {
            drop(session);
            drop(guard);
        })
        .await;
    }
}

fn open_root(path: &Path) -> Result<File, String> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(format!(
                "cannot make the browser directory {}: {err}",
                path.display()
            ));
        }
    }
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|err| match err.raw_os_error() {
            Some(libc::ELOOP) | Some(libc::ENOTDIR) => format!(
                "cannot use {}: it is not a directory of this user",
                path.display()
            ),
            _ => format!("cannot use {}: {err}", path.display()),
        })?;
    let meta = root
        .metadata()
        .map_err(|err| format!("cannot use {}: {err}", path.display()))?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(format!(
            "cannot use {}: it is not a directory of this user",
            path.display()
        ));
    }
    match root.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(format!(
                "{} is in use by another scour daemon",
                path.display()
            ));
        }
        Err(TryLockError::Error(err)) => {
            return Err(format!("cannot lock {}: {err}", path.display()));
        }
    }
    clean_root(&root, path)?;
    Ok(root)
}

fn clean_root(root: &File, path: &Path) -> Result<(), String> {
    // Read and remove through the locked descriptor, so a renamed root path cannot redirect us.
    let opened = PathBuf::from(format!("/proc/self/fd/{}", root.as_raw_fd()));
    let entries =
        fs::read_dir(&opened).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        let name = entry.file_name();
        if let Some(pid) = name
            .to_string_lossy()
            .split_once('-')
            .and_then(|(pid, _)| pid.parse::<libc::pid_t>().ok())
            && (pid == std::process::id() as libc::pid_t || !alive(pid))
        {
            remove_x_locks(pid);
        }
        remove_entry(&entry.path(), &path.join(&name))?;
    }
    Ok(())
}

fn alive(pid: libc::pid_t) -> bool {
    if pid <= 0 {
        return true;
    }
    (unsafe { libc::kill(pid, 0) == 0 })
        || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn remove_x_locks(pid: libc::pid_t) {
    let owner = format!("{pid:>10}\n");
    for n in 0..100 {
        let path = format!("/tmp/.X{n}-lock");
        if read_lock(Path::new(&path)).is_some_and(|text| text == owner) {
            let _ = fs::remove_file(path);
        }
    }
}

fn read_lock(path: &Path) -> Option<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } {
        return None;
    }
    let mut text = String::new();
    file.take(64).read_to_string(&mut text).ok()?;
    Some(text)
}

fn remove_entry(entry: &Path, path: &Path) -> Result<(), String> {
    let meta = match fs::symlink_metadata(entry) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("cannot delete {}: {err}", path.display())),
    };
    let result = if meta.file_type().is_dir() {
        fs::remove_dir_all(entry)
    } else {
        fs::remove_file(entry)
    };
    result.map_err(|err| format!("cannot delete {}: {err}", path.display()))
}

async fn connect(
    chrome: &mut Child,
    port: u16,
    deadline: Instant,
) -> Result<(CdpBrowser, JoinSet<()>), String> {
    let mut stderr = BufReader::new(chrome.stderr.take().expect("Chrome stderr pipe"));
    let (send, receive) = oneshot::channel();
    thread::Builder::new()
        .spawn(move || {
            let url = stderr
                .by_ref()
                .split(b'\n')
                .map_while(Result::ok)
                .find_map(|line| {
                    let line = std::str::from_utf8(&line).ok()?.trim();
                    let (_, url) = line.split_once("DevTools listening on ")?;
                    Some(url.to_owned())
                });
            let _ = send.send(url);
            let _ = io::copy(&mut stderr, &mut io::sink());
        })
        .map_err(|err| format!("cannot read Chrome's CDP address: {err}"))?;
    let url = timeout_at(deadline, async {
        if let Ok(Some(url)) = receive.await {
            return Ok(url);
        }
        loop {
            if let Some(status) = chrome
                .try_wait()
                .map_err(|err| format!("cannot wait for Chrome: {err}"))?
            {
                return Err(format!("Chrome exited with {status} before it opened CDP"));
            }
            sleep(POLL).await;
        }
    })
    .await
    .map_err(|_| format!("Chrome did not open CDP in {} s", CHROME_START.as_secs()))??;

    let invalid = "Chrome returned an invalid CDP URL";
    let (authority, id) = url
        .strip_prefix("ws://")
        .and_then(|url| url.split_once("/devtools/browser/"))
        .ok_or(invalid)?;
    let address: SocketAddr = authority.parse().map_err(|_| invalid)?;
    // Chrome can fall back to [::1] if another process binds the IPv4 port.
    if address.ip() != Ipv4Addr::LOCALHOST {
        return Err(format!("Chrome opened CDP on {address}, not on 127.0.0.1"));
    }
    let valid_id = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if address.port() == 0 || !valid_id {
        return Err(invalid.into());
    }
    if port != 0 && address.port() != port {
        return Err(format!(
            "Chrome opened CDP at {address}, expected 127.0.0.1:{port}"
        ));
    }
    // The default ignores certificate errors; search pages must verify TLS.
    let config = HandlerConfig {
        ignore_https_errors: false,
        ..Default::default()
    };
    let (mut cdp, mut stream) = timeout_at(deadline, CdpBrowser::connect_with_config(url, config))
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or_else(|| format!("cannot connect to Chrome over CDP at {address}"))?;
    let mut handler = JoinSet::new();
    handler.spawn(async move {
        // A fatal handler error closes its channels, so pending calls fail at once.
        while matches!(stream.next().await, Some(Ok(()))) {}
    });
    // CDP-created tabs use the default profile unless we set a query context.
    timeout_at(deadline, cdp.start_incognito_context())
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or("cannot create the query context in Chrome")?;
    Ok((cdp, handler))
}

struct Group(Child, libc::c_int);

impl Group {
    fn spawn(command: &mut Command, stop_signal: libc::c_int) -> io::Result<Self> {
        let parent = std::process::id() as libc::pid_t;
        // PDEATHSIG fires when the thread that spawned the child exits. Spawn children only on the
        // runtime thread: `main` keeps the current_thread runtime, and pool threads retire.
        unsafe {
            command.pre_exec(move || {
                let death_signal = libc::prctl(
                    libc::PR_SET_PDEATHSIG,
                    stop_signal as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                );
                if death_signal == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
        let child = command.process_group(0).stdin(Stdio::null()).spawn()?;
        Ok(Self(child, stop_signal))
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        let group = -(self.0.id() as i32);
        unsafe { libc::kill(group, self.1) };
        let deadline = std::time::Instant::now() + STOP_TIMEOUT;
        while std::time::Instant::now() < deadline {
            let _ = self.0.try_wait();
            if unsafe { libc::kill(group, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return;
            }
            thread::sleep(POLL);
        }
        if self.1 == libc::SIGTERM {
            unsafe { libc::kill(group, libc::SIGKILL) };
        }
        let _ = self.0.wait();
    }
}

struct RunDir(PathBuf);

impl RunDir {
    fn new(root: &Path) -> Result<Self, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = root.join(format!("{}-{nanos}", std::process::id()));
        let failed = |err| format!("cannot make the browser directory: {err}");
        DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(failed)?;
        let dir = Self(path);
        let tmp = dir.0.join("tmp");
        // The kernel limit is 108 bytes including the NUL terminator.
        if tmp.as_os_str().len() + CHROME_TMP_SUFFIX.len() > 107 {
            return Err(format!(
                "cannot use the browser directory {}: the path is too long for Chrome",
                dir.0.display()
            ));
        }
        fs::create_dir(&tmp).map_err(failed)?;
        Ok(dir)
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct DisplayLock(PathBuf);

impl DisplayLock {
    fn reserve() -> Result<(u8, Self), String> {
        let unix = fs::read_to_string("/proc/net/unix")
            .map_err(|err| format!("cannot read X sockets: {err}"))?;
        for n in 0..100 {
            let socket = format!("/tmp/.X11-unix/X{n}");
            let abstract_socket = format!(" @{socket}");
            if unix.lines().any(|line| line.ends_with(&abstract_socket)) {
                continue;
            }
            let failed = |err| format!("cannot lock X display :{n}: {err}");
            match fs::symlink_metadata(&socket) {
                Ok(_) => continue,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(failed(err)),
            }
            let path = PathBuf::from(format!("/tmp/.X{n}-lock"));
            // Xvfb does not acquire its own lock with -displayfd.
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o444)
                .open(&path)
            {
                Ok(mut file) => {
                    let lock = Self(path);
                    writeln!(file, "{:>10}", std::process::id()).map_err(failed)?;
                    return Ok((n, lock));
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(failed(err)),
            }
        }
        Err("no free X display from :0 to :99".into())
    }
}

impl Drop for DisplayLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::{chrome_proxy_server, connect_error, resolver_rules};
    use url::Url;

    #[test]
    fn a_loopback_proxy_is_excluded_once() {
        let proxy = Url::parse("http://127.0.0.1:8080").unwrap();
        assert_eq!(
            resolver_rules(Some(&proxy)),
            "MAP * ~NOTFOUND , EXCLUDE 127.0.0.1"
        );
    }

    #[test]
    fn an_ipv6_proxy_gets_an_unbracketed_resolver_exclusion() {
        let proxy = Url::parse("http://[::1]:8080").unwrap();
        assert_eq!(chrome_proxy_server(&proxy), "http://[::1]:8080");
        assert_eq!(
            resolver_rules(Some(&proxy)),
            "MAP * ~NOTFOUND , EXCLUDE 127.0.0.1 , EXCLUDE ::1"
        );
    }

    #[test]
    fn a_forwarder_error_without_a_proxy_does_not_name_the_proxy() {
        let text = "net::ERR_PROXY_CONNECTION_FAILED";
        assert_eq!(
            connect_error(text, "engine.test", false),
            "cannot connect to engine.test"
        );
        assert_eq!(
            connect_error(text, "engine.test", true),
            "cannot connect to the proxy"
        );
    }
}
