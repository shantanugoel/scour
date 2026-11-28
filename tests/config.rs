use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const PROXY_USER: &str = "fixture-user";
const PROXY_PASSWORD: &str = "fixture-password";

fn config_file(name: &str, text: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("scour-test-{name}-{}.toml", std::process::id()));
    std::fs::write(&path, text).unwrap();
    path
}

fn serve_with_config(path: &Path) -> Output {
    serve_with_flags(path, &[])
}

fn serve_with_flags(path: &Path, flags: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_scour"))
        .arg("serve")
        .args(flags)
        .arg("--config")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A config that is wrongly accepted starts a daemon that never exits.
    // Stop it, so that the test fails instead of waiting forever.
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

fn serve_without_config(xdg_config_home: Option<&Path>, home: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_scour"));
    command.arg("serve").env("HOME", home);
    match xdg_config_home {
        Some(path) => command.env("XDG_CONFIG_HOME", path),
        None => command.env_remove("XDG_CONFIG_HOME"),
    };
    command.output().unwrap()
}

fn test_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("scour-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn assert_exit_one(output: &Output, expected: &str) -> String {
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(stderr.contains(expected), "{stderr}");
    stderr
}

#[test]
fn invalid_toml_exits_one_without_the_source_line() {
    let path = config_file(
        "invalid-toml",
        "# the parser must count past this line\nengines = [\"duckduckgo\" \"hunter2\"]\n",
    );
    let output = serve_with_config(&path);
    let _ = std::fs::remove_file(&path);
    let stderr = assert_exit_one(&output, "config line 2");
    assert!(!stderr.contains("hunter2"), "{stderr}");
    assert!(!stderr.contains("duckduckgo"), "{stderr}");
}

#[test]
fn unknown_key_exits_one() {
    let path = config_file(
        "unknown-key",
        "proxi = \"http://user:hunter2@127.0.0.1:8080\"\n",
    );
    let output = serve_with_config(&path);
    let _ = std::fs::remove_file(&path);
    let stderr = assert_exit_one(&output, "unknown field `proxi`");
    assert!(!stderr.contains("hunter2"), "{stderr}");
}

fn assert_no_proxy_credentials(text: &str) {
    assert!(!text.contains(PROXY_USER), "{text}");
    assert!(!text.contains(PROXY_PASSWORD), "{text}");
}

#[test]
fn invalid_proxy_exits_one_without_credentials() {
    let credentials = format!("{PROXY_USER}:{PROXY_PASSWORD}");
    for url in [
        format!("https://{credentials}@127.0.0.1:8080"),
        format!("ftp://{credentials}@127.0.0.1:8080"),
        format!("socks4://{credentials}@127.0.0.1:1080"),
        format!("http://{credentials}@"),
        format!("http://{credentials}@127.0.0.1:bad-port"),
        format!("http:{credentials}@127.0.0.1:8080"),
        "http:///127.0.0.1:8080".to_string(),
        "socks5h:///path".to_string(),
    ] {
        let path = config_file("invalid-proxy", &format!("proxy = \"{url}\"\n"));
        let output = serve_with_config(&path);
        let _ = std::fs::remove_file(&path);
        let stderr = assert_exit_one(&output, "proxy must be");
        assert_no_proxy_credentials(&stderr);
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn proxy_with_a_path_query_or_fragment_exits_one_without_any_part_of_the_url() {
    // An unencoded `#`, `?` or `/` in the password ends the host part early.
    for url in [
        "http://alice:12#secret@proxy.example:8080",
        "http://alice:12?secret@proxy.example:8080",
        "http://alice:12/secret@proxy.example:8080",
        "socks5h://alice:12#secret@proxy.example:1080",
        "http://alice:secret@proxy.example:8080/x",
        "socks5://alice:secret@proxy.example:1080/x",
        "http://alice:secret@proxy.example:8080/?x",
        "http://alice:secret@proxy.example:8080#x",
        "http://alice:secret@proxy.example:8080?",
        "http://alice:secret@proxy.example:8080#",
    ] {
        let path = config_file("proxy-url-parts", &format!("proxy = \"{url}\"\n"));
        let output = serve_with_config(&path);
        let _ = std::fs::remove_file(&path);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        for part in ["alice", "12", "secret", "proxy.example", "8080", "1080"] {
            assert!(
                !stdout.contains(part) && !stderr.contains(part),
                "{url}: {stdout}{stderr}"
            );
        }
        assert_eq!(output.status.code(), Some(1), "{url}: {stderr}");
        assert!(
            stderr.contains("percent-encode") && stderr.contains("password"),
            "{url}: {stderr}"
        );
    }
}

#[test]
fn proxy_errors_from_toml_or_the_http_client_hide_credentials() {
    let credentials = format!("{PROXY_USER}:{PROXY_PASSWORD}");
    for (text, expected) in [
        (
            format!("proxy = \"http://{credentials}@localhost:8080\n"),
            "config line 1",
        ),
        (
            format!("proxy = [\"http://{credentials}@localhost:8080\"]\n"),
            "invalid type: sequence",
        ),
        (
            format!("proxy = \"socks5://{credentials}@bad%20host:1080\"\n"),
            "cannot start the search client",
        ),
    ] {
        let path = config_file("invalid-proxy-input", &text);
        let output = serve_with_config(&path);
        let _ = std::fs::remove_file(&path);
        let stderr = assert_exit_one(&output, expected);
        assert_no_proxy_credentials(&stderr);
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn a_test_url_for_an_engine_that_is_not_enabled_exits_one() {
    for name in ["bing", "unknown"] {
        let path = config_file(
            name,
            &format!("[testing.base_urls]\n{name} = \"http://127.0.0.1:8000\"\n"),
        );
        let output = serve_with_config(&path);
        let _ = std::fs::remove_file(&path);
        assert_exit_one(
            &output,
            &format!("testing.base_urls engine `{name}` is not enabled"),
        );
    }
}

#[test]
fn invalid_testing_settings_exit_one_without_echoing_values() {
    for (settings, expected) in [
        (
            "rest_seconds = 0",
            "testing.rest_seconds must be greater than zero",
        ),
        (
            "rest_seconds = -1",
            "testing.rest_seconds must be greater than zero",
        ),
        ("rest_second = 10", "unknown field `rest_second`"),
        (r#"rest_seconds = "hunter2""#, "invalid type: string"),
        (
            r#"base_urls.duckduckgo = "file:///hunter2""#,
            "testing.base_urls.duckduckgo must be an HTTP or HTTPS URL",
        ),
        (
            r#"base_urls.duckduckgo = "http://hunter2@""#,
            "testing.base_urls.duckduckgo must be an HTTP or HTTPS URL",
        ),
    ] {
        let path = config_file("invalid-testing", &format!("[testing]\n{settings}\n"));
        let output = serve_with_config(&path);
        let _ = std::fs::remove_file(&path);
        let stderr = assert_exit_one(&output, expected);
        assert!(!stderr.contains("hunter2"), "{stderr}");
    }
}

#[test]
fn invalid_limits_exit_one_with_the_key_and_without_the_value() {
    for (key, max, description) in [
        ("burst", "1001", "a whole number from 1 to 1000"),
        (
            "refill_seconds",
            "86401",
            "a whole number of seconds from 1 to 86400",
        ),
        ("daily", "4294967296", "a whole number from 1 to 4294967295"),
    ] {
        for value in ["0", "-1", r#""fixture-value""#, "1.5", max] {
            let path = config_file("invalid-limits", &format!("[limits]\n{key} = {value}\n"));
            let output = serve_with_config(&path);
            std::fs::remove_file(&path).unwrap();
            let expected = format!("limits.{key} must be {description}");
            let stderr = assert_exit_one(&output, &expected);
            assert_eq!(stderr.trim(), format!("scour: {expected}"));
            assert!(output.stdout.is_empty());
        }
    }
}

#[test]
fn per_engine_limit_overrides_exit_one() {
    let path = config_file("engine-limits", "[limits.google]\nburst = 1\n");
    let output = serve_with_config(&path);
    std::fs::remove_file(&path).unwrap();
    assert_exit_one(&output, "unknown field `google`");
}

#[test]
fn unknown_engine_exits_one() {
    let path = config_file("unknown-engine", "engines = [\"yahoo\"]\n");
    let output = serve_with_config(&path);
    let _ = std::fs::remove_file(&path);
    assert_exit_one(&output, "unknown engine `yahoo`");
}

#[test]
fn duplicate_engine_exits_one() {
    let path = config_file("duplicate-engine", "engines = [\"bing\", \"bing\"]\n");
    let output = serve_with_config(&path);
    let _ = std::fs::remove_file(&path);
    assert_exit_one(&output, "engine `bing` is listed more than once");
}

#[test]
fn empty_engines_list_exits_one() {
    let path = config_file("empty-engines", "engines = []\n");
    let output = serve_with_config(&path);
    let _ = std::fs::remove_file(&path);
    assert_exit_one(&output, "the engines list is empty");
}

#[test]
fn missing_config_file_exits_one() {
    let path = std::env::temp_dir().join(format!(
        "scour-test-missing-config-{}.toml",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let output = serve_with_config(&path);
    assert_exit_one(&output, "cannot read ");
}

#[test]
fn default_config_uses_xdg_config_home() {
    let root = test_dir("xdg-config-home");
    let xdg_config_home = root.join("xdg");
    let home = root.join("home");
    std::fs::create_dir_all(xdg_config_home.join("scour")).unwrap();
    std::fs::write(
        xdg_config_home.join("scour").join("config.toml"),
        "engines = []\n",
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".config").join("scour")).unwrap();
    std::fs::write(
        home.join(".config").join("scour").join("config.toml"),
        "engines = [\"google\"]\n",
    )
    .unwrap();
    let output = serve_without_config(Some(&xdg_config_home), &home);
    let _ = std::fs::remove_dir_all(&root);
    assert_exit_one(&output, "the engines list is empty");
}

#[test]
fn default_config_falls_back_to_home() {
    let root = test_dir("home-config");
    let home = root.join("home");
    std::fs::create_dir_all(home.join(".config").join("scour")).unwrap();
    std::fs::write(
        home.join(".config").join("scour").join("config.toml"),
        "engines = []\n",
    )
    .unwrap();
    let output = serve_without_config(None, &home);
    let _ = std::fs::remove_dir_all(&root);
    assert_exit_one(&output, "the engines list is empty");
}

#[test]
fn a_browser_engine_with_the_browser_off_exits_one_naming_the_item_and_the_flag() {
    let path = config_file("browser-engine", "engines = [\"duckduckgo@browser\"]\n");
    let output = serve_with_config(&path);
    let _ = std::fs::remove_file(&path);
    let stderr = assert_exit_one(&output, "duckduckgo@browser");
    assert!(stderr.contains("--browser"), "{stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn google_with_the_browser_off_exits_one_naming_the_engine_and_the_flag() {
    let path = config_file("google-engine", "engines = [\"duckduckgo\", \"google\"]\n");
    let output = serve_with_config(&path);
    let _ = std::fs::remove_file(&path);
    let stderr = assert_exit_one(&output, "google");
    assert!(stderr.contains("--browser"), "{stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn google_over_http_exits_one_with_the_browser_on() {
    let path = config_file("google-http", "engines = [\"google@http\"]\n");
    let output = serve_with_flags(&path, &["--browser"]);
    let _ = std::fs::remove_file(&path);
    let stderr = assert_exit_one(&output, "google@http");
    assert!(!stderr.contains("unknown engine"), "{stderr}");
    assert!(output.stdout.is_empty());
}
