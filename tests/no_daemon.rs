use std::process::Command;

#[test]
fn search_without_a_daemon_tells_the_person_to_start_one() {
    let output = Command::new(env!("CARGO_BIN_EXE_scour"))
        .args(["search", "x"])
        .output()
        .expect("run scour");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert_eq!(
        stderr,
        "scour: no daemon on 127.0.0.1:7268. Start it with: scour serve\n"
    );
}
