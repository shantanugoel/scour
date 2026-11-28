use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chromiumoxide::Browser as CdpBrowser;
use chromiumoxide::detection::{DetectionOptions, default_executable};
use chromiumoxide::error::CdpError;
use chromiumoxide::handler::HandlerConfig;
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep, timeout_at};
use url::Url;

use crate::search::TIMEOUT;

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
            // A deep TMPDIR overflows Chrome's process-singleton socket path.
            root: PathBuf::from("/tmp"),
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
    state: Mutex<State>,
}

enum State {
    Idle,
    Running(Session),
    Stopped,
}

struct Session {
    _handler: JoinSet<()>,
    cdp: Arc<CdpBrowser>,
    _chrome: Group,
    _dir: RunDir,
    _xvfb: Group,
    _display: DisplayLock,
}

impl Browser {
    pub fn new(settings: BrowserSettings) -> Self {
        Self {
            settings,
            state: Mutex::new(State::Idle),
        }
    }

    pub async fn fetch(&self, url: &Url) -> Result<Rendered, String> {
        let cdp = {
            let mut state = self.state.lock().await;
            if matches!(*state, State::Idle) {
                *state = State::Running(Session::start(&self.settings).await?);
            }
            match &*state {
                State::Running(session) => session.cdp.clone(),
                State::Stopped => return Err("the browser is stopping".into()),
                State::Idle => unreachable!(),
            }
        };
        let host = url.host_str().unwrap_or("the engine");
        let slow = || format!("no answer from {host} in {} s", TIMEOUT.as_secs());
        let cdp_error = |err| match err {
            CdpError::Timeout => slow(),
            _ => "lost the connection to Chrome".into(),
        };
        let deadline = Instant::now() + TIMEOUT;
        let page = timeout_at(deadline, cdp.new_page("about:blank"))
            .await
            .map_err(|_| slow())?
            .map_err(cdp_error)?;
        let rendered = timeout_at(deadline, async {
            page.goto(url.as_str()).await.map_err(|err| match err {
                CdpError::ChromeMessage(_) => format!("cannot connect to {host}"),
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
        let state = std::mem::replace(&mut *self.state.lock().await, State::Stopped);
        if let State::Running(session) = state {
            let _ = tokio::task::spawn_blocking(move || drop(session)).await;
        }
    }
}

impl Session {
    async fn start(settings: &BrowserSettings) -> Result<Self, String> {
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
            .args([
                "--no-sandbox",
                "--remote-debugging-address=127.0.0.1",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                "--disable-background-networking",
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
        Ok(Self {
            _handler: handler,
            cdp: Arc::new(cdp),
            _chrome: chrome,
            _dir: run,
            _xvfb: xvfb,
            _display: display_lock,
        })
    }
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
        let path = root.join(format!("scour-browser-{}-{nanos}", std::process::id()));
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
