use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn config_file(name: &str, text: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("scour-test-{name}-{}.toml", std::process::id()));
    std::fs::write(&path, text).unwrap();
    path
}

fn serve_with_config(path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_scour"))
        .arg("serve")
        .arg("--config")
        .arg(path)
        .output()
        .unwrap()
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
        "proxy = \"http://user:hunter2@127.0.0.1:8080\"\n",
    );
    let output = serve_with_config(&path);
    let _ = std::fs::remove_file(&path);
    let stderr = assert_exit_one(&output, "unknown field `proxy`");
    assert!(!stderr.contains("hunter2"), "{stderr}");
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
fn unknown_engine_exits_one() {
    let path = config_file("unknown-engine", "engines = [\"google\"]\n");
    let output = serve_with_config(&path);
    let _ = std::fs::remove_file(&path);
    assert_exit_one(&output, "unknown engine `google`");
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
