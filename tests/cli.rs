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
