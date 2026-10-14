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
        .args(["--repo", dir, "--profile", "alice", "push"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "error: `sss push` is not implemented yet\n"
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

/// Runs `sss pandora auth` for profile `alice` against `server`, with the
/// state directory under `state` and the token on stdin.
fn pandora_auth(server: &httpmock::MockServer, repo: &str, state: &std::path::Path) -> Command {
    let mut cmd = sss();
    cmd.env("SSS_PANDORA_BASE_URL", server.base_url())
        .env("XDG_STATE_HOME", state)
        .args(["--repo", repo, "--profile", "alice", "pandora", "auth"]);
    cmd
}

#[test]
fn pandora_auth_verifies_and_stores_the_session_privately() {
    use httpmock::prelude::*;
    use std::os::unix::fs::PermissionsExt;

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/api/v1/station/getStations")
            .header("X-AuthToken", "tok-secret-value");
        then.status(200)
            .json_body(serde_json::json!({"stations": [{"creatorWebname": "listener"}]}));
    });
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss().args(["--repo", repo, "init"]).assert().code(0);
    let out = pandora_auth(&server, repo, tmp.path())
        .write_stdin("tok-secret-value\n")
        .assert()
        .code(0)
        .get_output()
        .clone();
    mock.assert_calls(1);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("webname listener"), "{stdout}");
    assert!(
        stderr.contains("X-AuthToken"),
        "the prompt goes to stderr: {stderr}"
    );
    assert!(!stdout.contains("tok-secret-value") && !stderr.contains("tok-secret-value"));

    let session = tmp
        .path()
        .join("streaming-service-sync/alice/pandora-session.json");
    let mode = std::fs::metadata(&session)
        .expect("session stored")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let text = std::fs::read_to_string(&session).expect("read session");
    assert!(text.contains("tok-secret-value") && text.contains("\"source\": \"prompt\""));
    // Nothing secret lands in the data repository.
    for entry in walk(std::path::Path::new(repo)) {
        let bytes = std::fs::read(&entry).expect("read repo file");
        assert!(!String::from_utf8_lossy(&bytes).contains("tok-secret-value"));
    }
}

#[test]
fn pandora_auth_with_a_refused_token_exits_1_and_stores_nothing() {
    use httpmock::prelude::*;

    let server = MockServer::start();
    server.mock(|_, then| {
        then.status(401)
            .json_body(serde_json::json!({"errorCode": 1001}));
    });
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss().args(["--repo", repo, "init"]).assert().code(0);
    let token = tmp.path().join("token.txt");
    std::fs::write(&token, "tok-secret-value\n").expect("write token file");
    let out = pandora_auth(&server, repo, tmp.path())
        .arg("--from-file")
        .arg(&token)
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with("error: ") && stderr.contains("re-run `sss pandora auth`"),
        "{stderr}"
    );
    assert!(!tmp.path().join("streaming-service-sync").exists());
}

/// Every file under `dir`, recursively.
fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

/// An initialized repository with a stored session for profile `alice`,
/// verified against `server`.
fn repo_with_session(server: &httpmock::MockServer, tmp: &std::path::Path) -> String {
    use httpmock::prelude::*;

    let repo = tmp.join("data");
    let repo = repo.to_str().expect("utf-8 path").to_owned();
    sss().args(["--repo", &repo, "init"]).assert().code(0);
    let mut verify = server.mock(|when, then| {
        when.method(POST)
            .path("/api/v1/station/getStations")
            .json_body(serde_json::json!({"pageSize": 1}));
        then.status(200)
            .json_body(serde_json::json!({"stations": []}));
    });
    pandora_auth(server, &repo, tmp)
        .write_stdin("tok-secret-value\n")
        .assert()
        .code(0);
    verify.delete();
    repo
}

/// Mocks an account with nothing in it; `playlists_status` answers the
/// playlist listing.
fn empty_account(server: &httpmock::MockServer, playlists_status: u16) {
    use httpmock::prelude::*;
    use serde_json::json;

    let replies = [
        (
            "v1/station/getStations",
            200,
            json!({"totalStations": 0, "stations": []}),
        ),
        (
            "v6/collections/getSortedPlaylists",
            playlists_status,
            json!({"items": []}),
        ),
        ("v6/collections/getItems", 200, json!({"items": []})),
        ("v1/bookmark/getBookmarks", 200, json!({})),
    ];
    for (path, status, body) in replies {
        server.mock(|when, then| {
            when.method(POST)
                .path(format!("/api/{path}"))
                .header("X-AuthToken", "tok-secret-value");
            then.status(status).json_body(body);
        });
    }
}

fn pandora_export(server: &httpmock::MockServer, repo: &str, state: &std::path::Path) -> Command {
    let mut cmd = sss();
    cmd.env("SSS_PANDORA_BASE_URL", server.base_url())
        .env("XDG_STATE_HOME", state)
        .args(["--repo", repo, "--profile", "alice", "pandora", "export"]);
    cmd
}

#[test]
fn pandora_export_writes_a_complete_run() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_session(&server, tmp.path());
    empty_account(&server, 200);
    let out = pandora_export(&server, &repo, tmp.path())
        .assert()
        .code(0)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("bookmarks        ok      0"), "{stdout}");
    assert!(stdout.contains(" complete"), "{stdout}");
    assert!(stdout.contains("normalized run "), "{stdout}");
    let pandora = std::path::Path::new(&repo).join("profiles/alice/pandora");
    for file in [
        "stations.json",
        "playlists.json",
        "collection.json",
        "bookmarks.json",
        "tracks.json",
    ] {
        assert!(pandora.join(file).is_file(), "{file}");
    }
    let status = sss()
        .args(["--repo", &repo, "status"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(status.starts_with("profile alice\n"), "{status}");
    assert!(status.contains(": complete, 5 requests"), "{status}");
    assert!(
        status.contains("    bookmarks        ok      0\n"),
        "{status}"
    );
    sss()
        .args([
            "--repo",
            &repo,
            "--profile",
            "alice",
            "pandora",
            "normalize",
        ])
        .assert()
        .code(0);
    let raw = pandora.join("raw");
    let files = walk(&raw);
    // stations, playlists, collection, two bookmark requests, the manifest.
    assert_eq!(files.len(), 6, "{files:?}");
    let manifest = files
        .iter()
        .find(|p| p.ends_with("manifest.json"))
        .expect("manifest");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(manifest).expect("read")).expect("json");
    assert_eq!(manifest["complete"], true);
    assert_eq!(manifest["requests"], 5);
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read raw file");
        assert!(!text.contains("tok-secret-value"), "{}", file.display());
    }
}

#[test]
fn pandora_export_with_a_failed_step_exits_1_and_keeps_the_run() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_session(&server, tmp.path());
    empty_account(&server, 404);
    let out = pandora_export(&server, &repo, tmp.path())
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("playlists        FAILED"), "{stdout}");
    assert!(
        stderr.starts_with("error: run ")
            && stderr.contains("incomplete: failed steps playlists, playlist_tracks"),
        "{stderr}"
    );
    let pandora = std::path::Path::new(&repo).join("profiles/alice/pandora");
    assert!(
        walk(&pandora.join("raw"))
            .iter()
            .any(|p| p.ends_with("manifest.json"))
    );
    assert!(
        !pandora.join("stations.json").exists(),
        "incomplete runs are not normalized"
    );
    let out = sss()
        .args([
            "--repo",
            &repo,
            "--profile",
            "alice",
            "pandora",
            "normalize",
        ])
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "error: no complete export run to normalize; run `sss pandora export`\n"
    );
}

#[test]
fn pandora_export_without_a_session_says_to_authenticate() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss().args(["--repo", repo, "init"]).assert().code(0);
    let out = sss()
        .env("XDG_STATE_HOME", tmp.path())
        .args(["--repo", repo, "--profile", "alice", "pandora", "export"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "error: no Pandora session for profile alice; run `sss pandora auth`\n"
    );
}
