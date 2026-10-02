use std::process::Command;

#[test]
fn version_flag_reports_package_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_tines-runner-rs"))
        .arg("--version")
        .output()
        .expect("runner binary should start");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("version output should be UTF-8"),
        format!("tines-runner-rs {}\n", env!("CARGO_PKG_VERSION"))
    );
}
