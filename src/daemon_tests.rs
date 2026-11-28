use std::net::{Ipv4Addr, SocketAddr};
use std::process::{Command, ExitCode};

use tokio::net::TcpSocket;

use crate::{cli, start_daemon};

#[tokio::test]
async fn daemon_redacts_proxy_start_line_and_cli_connects_directly() {
    const CHILD: &str = "SCOUR_PROXY_TEST_CHILD";
    const USER: &str = "fixture-user";
    const PASSWORD: &str = "fixture-password";

    if std::env::var_os(CHILD).is_some() {
        let server = start_daemon(None, (Ipv4Addr::LOCALHOST, 0).into())
            .await
            .expect("start daemon");
        let address = server.local_addr().unwrap();
        tokio::select! {
            result = server.into_future() => panic!("daemon stopped: {result:?}"),
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
