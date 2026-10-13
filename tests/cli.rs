//! Exit status and output streams of the built `sss` binary.

use assert_cmd::Command;

fn sss() -> Command {
    Command::cargo_bin("sss").expect("sss binary is built")
}

#[test]
fn usage_error_prints_error_to_stderr_and_exits_1() {
    let out = sss().arg("--bogus").assert().code(1).get_output().clone();
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("error: "));
}

#[test]
fn help_and_version_print_to_stdout_and_exit_0() {
    for flag in ["--help", "--version"] {
        let out = sss().arg(flag).assert().code(0).get_output().clone();
        assert!(!out.stdout.is_empty(), "{flag}: stdout empty");
        assert!(out.stderr.is_empty(), "{flag}: stderr not empty");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn failed_output_write_prints_error_and_exits_1() {
    for flag in ["--help", "--version"] {
        let full = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .expect("open /dev/full");
        let out = std::process::Command::new(assert_cmd::cargo::cargo_bin("sss"))
            .arg(flag)
            .stdout(full)
            .output()
            .expect("run sss");
        assert_eq!(out.status.code(), Some(1), "{flag}");
        assert!(
            String::from_utf8_lossy(&out.stderr).starts_with("error: writing output: "),
            "{flag}"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn failed_command_output_write_prints_error_and_exits_1() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .expect("open /dev/full");
    let out = std::process::Command::new(assert_cmd::cargo::cargo_bin("sss"))
        .args(["--repo", tmp.path().to_str().expect("utf-8 path"), "init"])
        .stdout(full)
        .output()
        .expect("run sss");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.starts_with("error: writing output"), "{stderr}");
}

#[test]
fn init_defaults_to_xdg_data_home_and_is_idempotent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("streaming-service-sync");
    for verb in ["created", "kept"] {
        let out = sss()
            .env("XDG_DATA_HOME", tmp.path())
            .arg("init")
            .assert()
            .code(0)
            .get_output()
            .clone();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(stdout.matches(verb).count(), 2, "{stdout}");
        assert!(out.stderr.is_empty());
    }
    assert!(repo.join("config.toml").is_file());
    assert!(repo.join("README.md").is_file());
}

#[test]
fn unimplemented_command_reports_an_error_and_exits_1() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_str().expect("utf-8 path");
    sss().args(["--repo", dir, "init"]).assert().code(0);
    let out = sss()
        .args(["--repo", dir, "--profile", "alice", "pandora", "export"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "error: `sss pandora export` is not implemented yet\n"
    );
}

#[test]
fn command_on_an_uninitialized_repo_exits_1() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = sss()
        .args(["--repo", tmp.path().to_str().expect("utf-8 path"), "status"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with("error: ") && stderr.contains("sss init"),
        "{stderr}"
    );
}
