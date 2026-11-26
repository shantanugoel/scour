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

fn assert_exit_one(output: &Output, expected: &str) -> String {
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(stderr.contains(expected), "{stderr}");
    stderr
}

#[test]
fn invalid_toml_exits_one_without_the_source_line() {
    let path = config_file("invalid-toml", "engines = [\"duckduckgo\" \"hunter2\"]\n");
    let output = serve_with_config(&path);
    let _ = std::fs::remove_file(&path);
    let stderr = assert_exit_one(&output, "config line 1");
    assert!(!stderr.contains("hunter2"), "{stderr}");
    assert!(!stderr.contains("duckduckgo"), "{stderr}");
}

#[test]
fn unknown_key_exits_one() {
    let path = config_file("unknown-key", "proxy = \"http://127.0.0.1:8080\"\n");
    let output = serve_with_config(&path);
    let _ = std::fs::remove_file(&path);
    assert_exit_one(&output, "unknown field `proxy`");
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
