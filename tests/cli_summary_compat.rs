use assert_cmd::Command;

#[test]
fn native_exit_option_is_explicit_and_limited_to_test() {
    let output = Command::cargo_bin("kioku")
        .unwrap()
        .args(["test", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("--native-exit-code"));

    let output = Command::cargo_bin("kioku")
        .unwrap()
        .args(["select", "plan", "--native-exit-code"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}
