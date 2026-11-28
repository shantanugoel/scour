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

#[test]
fn serve_help_lists_browser_without_a_port_option() {
    let output = Command::new(env!("CARGO_BIN_EXE_scour"))
        .args(["serve", "--help"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{stdout}");
    let options: Vec<_> = stdout
        .lines()
        .filter_map(|line| {
            line.split_whitespace()
                .take_while(|word| word.starts_with('-'))
                .find(|word| word.starts_with("--"))
        })
        .collect();
    assert_eq!(options, ["--config", "--browser", "--help"]);
}
