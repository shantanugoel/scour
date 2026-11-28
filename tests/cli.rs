use std::process::Command;

#[test]
fn search_help_lists_the_engines_option() {
    let output = Command::new(env!("CARGO_BIN_EXE_scour"))
        .args(["search", "--help"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("--engines"), "{stdout}");
}
