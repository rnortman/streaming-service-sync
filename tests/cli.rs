//! Exit status and output streams of the built `sss` binary.
//!
//! No test runs the binary against the host's state root: every invocation
//! sets `XDG_STATE_HOME` to a temporary directory.

use std::path::{Path, PathBuf};

use assert_cmd::Command;

/// The built `sss` with its state root under `state`.
fn sss(state: &Path) -> Command {
    let mut cmd = Command::cargo_bin("sss").expect("sss binary is built");
    cmd.env("XDG_STATE_HOME", state);
    cmd
}

/// The built `sss` as a plain process, with its state root under `state`.
fn sss_process(state: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("sss"));
    cmd.env("XDG_STATE_HOME", state);
    cmd
}

#[test]
fn usage_error_prints_error_to_stderr_and_exits_1() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = sss(tmp.path())
        .arg("--bogus")
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("error: "));
}

#[test]
fn help_and_version_print_to_stdout_and_exit_0() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for flag in ["--help", "--version"] {
        let out = sss(tmp.path())
            .arg(flag)
            .assert()
            .code(0)
            .get_output()
            .clone();
        assert!(!out.stdout.is_empty(), "{flag}: stdout empty");
        assert!(out.stderr.is_empty(), "{flag}: stderr not empty");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn failed_output_write_prints_error_and_exits_1() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for flag in ["--help", "--version"] {
        let full = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .expect("open /dev/full");
        let out = sss_process(tmp.path())
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
    let out = sss_process(tmp.path())
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
        let out = sss(tmp.path())
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
fn a_failing_command_reports_an_error_and_exits_1() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", dir, "init"])
        .assert()
        .code(0);
    let out = sss(tmp.path())
        .args(["--repo", dir, "--profile", "alice", "report"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr,
        "error: no export for profile alice; run `sss pandora export` or `sss youtube import DIR`\n"
    );
}

#[test]
fn command_on_an_uninitialized_repo_exits_1() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = sss(tmp.path())
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
fn pandora_auth(server: &httpmock::MockServer, repo: &str, state: &Path) -> Command {
    let mut cmd = sss(state);
    cmd.env("SSS_PANDORA_BASE_URL", server.base_url()).args([
        "--repo",
        repo,
        "--profile",
        "alice",
        "pandora",
        "auth",
    ]);
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
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
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
    for entry in walk(Path::new(repo)) {
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
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
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
        stderr.starts_with("Verifying the token with Pandora...\nerror: ")
            && stderr.contains("re-run `sss pandora auth`"),
        "{stderr}"
    );
    assert!(!tmp.path().join("streaming-service-sync").exists());
}

#[test]
fn pandora_auth_json_for_another_webname_warns_on_stderr_and_names_it() {
    use httpmock::prelude::*;

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    for webname in ["listener", "other"] {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/api/v1/station/getStations");
            then.status(200)
                .json_body(serde_json::json!({"stations": [{"creatorWebname": webname}]}));
        });
        let out = pandora_auth(&server, repo, tmp.path())
            .arg("--json")
            .write_stdin("tok-secret-value\n")
            .assert()
            .code(0)
            .get_output()
            .clone();
        let doc: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("stdout is one JSON document");
        assert_eq!(doc["webname"], webname);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let warning = "warning: profile alice held a session for Pandora webname listener; \
                       it now holds webname other\n";
        if webname == "listener" {
            assert_eq!(doc["replaced_webname"], serde_json::Value::Null);
            assert!(!stderr.contains("warning"), "{stderr}");
        } else {
            assert_eq!(doc["replaced_webname"], "listener");
            assert!(stderr.ends_with(warning), "{stderr}");
        }
    }
}

/// Every file under `dir`, recursively.
fn walk(dir: &Path) -> Vec<PathBuf> {
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
fn repo_with_session(server: &httpmock::MockServer, tmp: &Path) -> String {
    use httpmock::prelude::*;

    let repo = tmp.join("data");
    let repo = repo.to_str().expect("utf-8 path").to_owned();
    sss(tmp).args(["--repo", &repo, "init"]).assert().code(0);
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

fn pandora_export(server: &httpmock::MockServer, repo: &str, state: &Path) -> Command {
    let mut cmd = sss(state);
    cmd.env("SSS_PANDORA_BASE_URL", server.base_url()).args([
        "--repo",
        repo,
        "--profile",
        "alice",
        "pandora",
        "export",
    ]);
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
    let pandora = Path::new(&repo).join("profiles/alice/pandora");
    for file in [
        "stations.json",
        "playlists.json",
        "collection.json",
        "bookmarks.json",
        "tracks.json",
    ] {
        assert!(pandora.join(file).is_file(), "{file}");
    }
    let status = sss(tmp.path())
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
    sss(tmp.path())
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
    let pandora = Path::new(&repo).join("profiles/alice/pandora");
    assert!(
        walk(&pandora.join("raw"))
            .iter()
            .any(|p| p.ends_with("manifest.json"))
    );
    assert!(
        !pandora.join("stations.json").exists(),
        "incomplete runs are not normalized"
    );
    let out = sss(tmp.path())
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
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    let out = sss(tmp.path())
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

#[test]
fn pandora_auth_joins_a_two_line_paste_and_reports_progress() {
    use httpmock::prelude::*;

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
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    let out = pandora_auth(&server, repo, tmp.path())
        .write_stdin("tok-sec\nret-value\n")
        .assert()
        .code(0)
        .get_output()
        .clone();
    mock.assert_calls(1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("select the Network tab")
            && stderr.contains("press Enter on an empty line")
            && stderr.contains("Verifying the token with Pandora"),
        "{stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("Pandora session verified") && !stdout.contains("Verifying"),
        "{stdout}"
    );
}

#[test]
fn the_only_profile_is_the_default_and_status_shows_its_session() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_session(&server, tmp.path());

    let out = sss(tmp.path())
        .args(["--repo", &repo, "status"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let status = String::from_utf8_lossy(&out.stdout);
    assert!(
        status.starts_with("profile alice\n  pandora session: stored ")
            && status.contains(
                ", from prompt\n  spotify token: none (run `sss spotify auth --profile alice`)\n  \
                 export: no run yet\n"
            ),
        "{status}"
    );
    assert!(
        !Path::new(&repo).join("profiles").exists(),
        "auth creates nothing in the data repository"
    );

    empty_account(&server, 200);
    let out = sss(tmp.path())
        .env("SSS_PANDORA_BASE_URL", server.base_url())
        .args(["--repo", &repo, "pandora", "export"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    assert!(String::from_utf8_lossy(&out.stdout).contains(" complete"));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .starts_with("profile alice (the only one; pass --profile NAME to choose)\n"),
        "the inferred profile is named on stderr"
    );
    assert!(
        Path::new(&repo)
            .join("profiles/alice/pandora/stations.json")
            .is_file()
    );

    let config = Path::new(&repo).join("config.toml");
    let mut text = std::fs::read_to_string(&config).expect("read config");
    text.push_str("\n[profiles.bob]\n");
    std::fs::write(&config, text).expect("write config");
    let out = sss(tmp.path())
        .env("SSS_PANDORA_BASE_URL", server.base_url())
        .args(["--repo", &repo, "pandora", "export"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "error: `sss pandora export` needs --profile NAME (profiles: alice, bob)\n"
    );
}

#[test]
fn spotify_sign_in_in_two_steps_stores_the_token_privately() {
    use std::os::unix::fs::PermissionsExt;

    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    std::fs::write(
        Path::new(repo).join("config.toml"),
        "[spotify]\nclient_id = \"test-client-id\"\n",
    )
    .expect("write config");

    let out = sss(tmp.path())
        .env("SSS_SPOTIFY_AUTH_BASE_URL", server.base_url())
        .args(["--repo", repo, "--profile", "dave", "spotify", "auth"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let address = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    assert!(
        address.starts_with(&format!("{}/authorize?", server.base_url())),
        "{address}"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--callback --profile dave"),
        "instructions on stderr"
    );
    let state_dir = tmp.path().join("streaming-service-sync/dave");
    let pending = state_dir.join("spotify-auth-pending.json");
    assert!(pending.is_file());
    let state = url_param(&address, "state");

    let token = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/api/token")
            .form_urlencoded_tuple("grant_type", "authorization_code")
            .form_urlencoded_tuple("code", "c0de");
        then.status(200).json_body(serde_json::json!({
            "access_token": "access-secret", "token_type": "Bearer", "expires_in": 3600,
            "refresh_token": "refresh-secret", "scope": "user-library-read"}));
    });
    let out = sss(tmp.path())
        .env("SSS_SPOTIFY_AUTH_BASE_URL", server.base_url())
        .args([
            "--repo",
            repo,
            "--profile",
            "dave",
            "spotify",
            "auth",
            "--callback",
            "--json",
        ])
        .write_stdin(format!(
            "https://127.0.0.1/callback?code=c0de&state={state}\n\n"
        ))
        .assert()
        .code(0)
        .get_output()
        .clone();
    token.assert();
    let doc = json_document(&out.stdout);
    assert_eq!(doc["ok"], true);
    assert_eq!(doc["command"], "spotify auth");
    assert_eq!(doc["scopes"], serde_json::json!(["user-library-read"]));
    let token_file = state_dir.join("spotify-token.json");
    assert_eq!(
        std::fs::metadata(&token_file)
            .expect("token file")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(!pending.exists());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("secret"));

    // A Spotify-only profile is the default and status shows its token.
    let out = sss(tmp.path())
        .args(["--repo", repo, "status", "--json"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let doc = json_document(&out.stdout);
    let profile = &doc["profiles"][0];
    assert_eq!(profile["name"], "dave");
    assert_eq!(
        profile["spotify_token"]["scopes"],
        serde_json::json!(["user-library-read"])
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("secret"));
}

#[test]
fn spotify_auth_reads_the_callback_from_a_file_and_only_with_callback() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    std::fs::write(
        Path::new(repo).join("config.toml"),
        "[spotify]\nclient_id = \"test-client-id\"\n",
    )
    .expect("write config");
    let state_dir = tmp.path().join("streaming-service-sync/dave");
    let pending = state_dir.join("spotify-auth-pending.json");
    let address_file = tmp.path().join("address.txt");

    // `--from-file` without `--callback` is refused and starts nothing.
    let out = sss(tmp.path())
        .env("SSS_SPOTIFY_AUTH_BASE_URL", server.base_url())
        .args(["--repo", repo, "--profile", "dave", "spotify", "auth"])
        .arg("--from-file")
        .arg(&address_file)
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(
        String::from_utf8_lossy(&out.stderr).starts_with("error: "),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty());
    assert!(!pending.exists());

    let out = sss(tmp.path())
        .env("SSS_SPOTIFY_AUTH_BASE_URL", server.base_url())
        .args(["--repo", repo, "--profile", "dave", "spotify", "auth"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let address = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let state = url_param(&address, "state");
    std::fs::write(
        &address_file,
        format!("https://127.0.0.1/callback?code=c0de&state={state}\n"),
    )
    .expect("write address");
    let token = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/api/token")
            .form_urlencoded_tuple("grant_type", "authorization_code")
            .form_urlencoded_tuple("code", "c0de");
        then.status(200).json_body(serde_json::json!({
            "access_token": "access-secret", "token_type": "Bearer", "expires_in": 3600,
            "refresh_token": "refresh-secret", "scope": "user-library-read"}));
    });
    sss(tmp.path())
        .env("SSS_SPOTIFY_AUTH_BASE_URL", server.base_url())
        .args([
            "--repo",
            repo,
            "--profile",
            "dave",
            "spotify",
            "auth",
            "--callback",
            "--from-file",
        ])
        .arg(&address_file)
        .write_stdin("")
        .assert()
        .code(0);
    token.assert();
    assert!(state_dir.join("spotify-token.json").is_file());
    assert!(!pending.exists());
}

/// The value of query parameter `name` in `address`.
fn url_param(address: &str, name: &str) -> String {
    let query = address.split_once('?').expect("a query").1;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name} in {address}"))
        .to_owned()
}

#[test]
fn status_with_no_profiles_says_how_to_add_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    let out = sss(tmp.path())
        .args(["--repo", repo, "status"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "no profiles yet; run `sss pandora auth --profile NAME` or `sss youtube import \
         --profile NAME DIR` first\n"
    );
    let out = sss(tmp.path())
        .args(["--repo", repo, "pandora", "auth"])
        .write_stdin("")
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "error: `sss pandora auth` needs --profile NAME (no profiles yet)\n"
    );
}

/// The single JSON document on `stdout`.
fn json_document(stdout: &[u8]) -> serde_json::Value {
    let text = String::from_utf8_lossy(stdout);
    assert!(text.ends_with("}\n"), "one document and a newline: {text}");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

#[test]
fn init_json_reports_created_then_kept_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    for (present, absent) in [("created", "kept"), ("kept", "created")] {
        let out = sss(tmp.path())
            .args(["--repo", repo, "init", "--json"])
            .assert()
            .code(0)
            .get_output()
            .clone();
        let doc = json_document(&out.stdout);
        assert_eq!(doc["ok"], true);
        assert_eq!(doc["command"], "init");
        assert_eq!(doc["repo"], repo);
        assert_eq!(doc[present].as_array().map(Vec::len), Some(2), "{doc}");
        assert_eq!(doc[absent], serde_json::json!([]), "{doc}");
        assert!(out.stderr.is_empty());
    }
}

#[test]
fn pandora_auth_json_reports_the_session_without_the_token() {
    use httpmock::prelude::*;

    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST)
            .path("/api/v1/station/getStations")
            .header("X-AuthToken", "tok-secret-value");
        then.status(200)
            .json_body(serde_json::json!({"stations": [{"creatorWebname": "listener"}]}));
    });
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    let out = pandora_auth(&server, repo, tmp.path())
        .arg("--json")
        .write_stdin("tok-secret-value\n")
        .assert()
        .code(0)
        .get_output()
        .clone();
    let doc = json_document(&out.stdout);
    assert_eq!(doc["ok"], true);
    assert_eq!(doc["command"], "pandora auth");
    assert_eq!(doc["profile"], "alice");
    assert_eq!(doc["webname"], "listener");
    assert_eq!(doc["source"], "prompt");
    assert_eq!(doc["replaced_webname"], serde_json::Value::Null);
    assert!(
        doc["session_path"]
            .as_str()
            .is_some_and(|p| p.ends_with("alice/pandora-session.json")),
        "{doc}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stdout.contains("tok-secret-value") && !stderr.contains("tok-secret-value"));
    assert!(
        stderr.contains("select the Network tab")
            && stderr.contains("Verifying the token with Pandora"),
        "{stderr}"
    );
}

#[test]
fn pandora_export_json_is_one_document_with_progress_on_stderr() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_session(&server, tmp.path());
    empty_account(&server, 200);
    let out = pandora_export(&server, &repo, tmp.path())
        .arg("--json")
        .assert()
        .code(0)
        .get_output()
        .clone();
    let doc = json_document(&out.stdout);
    let text = String::from_utf8_lossy(&out.stdout);
    let at = |key: &str| {
        text.find(&format!("\n  \"{key}\": "))
            .unwrap_or_else(|| panic!("top-level {key}: {text}"))
    };
    let order = [
        "ok",
        "command",
        "profile",
        "run_id",
        "run_dir",
        "manifest",
        "normalized",
    ]
    .map(at);
    assert!(order.is_sorted(), "top-level key order: {text}");
    assert_eq!(doc["ok"], true);
    assert_eq!(doc["command"], "pandora export");
    assert_eq!(doc["profile"], "alice");
    assert_eq!(doc["manifest"]["complete"], true);
    assert_eq!(doc["manifest"]["requests"], 5);
    assert_eq!(doc["manifest"]["steps"]["bookmarks"]["status"], "ok");
    assert_eq!(doc["normalized"]["run_id"], doc["run_id"]);
    assert_eq!(doc["normalized"]["stations"], 0);
    assert_eq!(doc["normalized"]["tracks"], 0);
    assert!(
        Path::new(doc["run_dir"].as_str().expect("run_dir"))
            .join("manifest.json")
            .is_file(),
        "{doc}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("bookmarks        ok      0") && stderr.contains("run "),
        "{stderr}"
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("tok-secret-value"));
}

#[test]
fn pandora_export_json_with_a_failed_step_is_kind_incomplete() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_session(&server, tmp.path());
    empty_account(&server, 404);
    let out = pandora_export(&server, &repo, tmp.path())
        .arg("--json")
        .assert()
        .code(1)
        .get_output()
        .clone();
    let doc = json_document(&out.stdout);
    assert_eq!(doc["ok"], false);
    assert_eq!(doc["command"], "pandora export");
    assert_eq!(doc["kind"], "incomplete");
    assert_eq!(
        doc["details"]["failed_steps"],
        serde_json::json!(["playlists", "playlist_tracks"])
    );
    assert_eq!(doc["details"]["manifest"]["complete"], false);
    assert_eq!(
        doc["details"]["run_id"],
        doc["details"]["manifest"]["run_id"]
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let error = doc["error"].as_str().expect("error text");
    assert!(
        stderr.ends_with(&format!("error: {error}\n")),
        "the same text on stderr: {stderr}"
    );
}

#[test]
fn pandora_export_json_with_a_rejected_session_is_kind_auth() {
    use httpmock::prelude::*;

    let server = MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_session(&server, tmp.path());
    server.mock(|when, then| {
        when.method(POST).path("/api/v1/station/getStations");
        then.status(401)
            .json_body(serde_json::json!({"errorCode": 1001}));
    });
    let out = pandora_export(&server, &repo, tmp.path())
        .arg("--json")
        .assert()
        .code(1)
        .get_output()
        .clone();
    let doc = json_document(&out.stdout);
    assert_eq!(doc["ok"], false);
    assert_eq!(doc["kind"], "auth");
    assert!(
        doc["error"]
            .as_str()
            .is_some_and(|e| e.contains("re-run `sss pandora auth`")),
        "{doc}"
    );
    let run_id = doc["details"]["run_id"].as_str().expect("details.run_id");
    let run_dir = Path::new(doc["details"]["run_dir"].as_str().expect("details.run_dir"));
    assert!(run_dir.ends_with(run_id), "{doc}");
    assert!(run_dir.join("manifest.json").is_file());
    assert!(doc["details"].get("manifest").is_none(), "{doc}");
}

#[test]
fn status_json_parses() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_session(&server, tmp.path());
    let out = sss(tmp.path())
        .args(["--repo", &repo, "--json", "status"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let doc = json_document(&out.stdout);
    assert_eq!(doc["ok"], true);
    assert_eq!(doc["command"], "status");
    let profile = &doc["profiles"][0];
    assert_eq!(profile["name"], "alice");
    assert_eq!(profile["pandora_session"]["source"], "prompt");
    assert_eq!(profile["export"]["latest_run"], serde_json::Value::Null);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("tok-secret-value"));
}

#[test]
fn usage_error_under_json_leaves_stdout_empty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = sss(tmp.path())
        .args(["--bogus", "--json"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("error: "));
}

#[test]
fn json_without_a_state_root_is_kind_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = sss(tmp.path())
        .env_remove("XDG_STATE_HOME")
        .env_remove("HOME")
        .args([
            "status",
            "--json",
            "--repo",
            tmp.path().to_str().expect("utf-8 path"),
        ])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let doc = json_document(&out.stdout);
    assert_eq!(doc["ok"], false);
    assert_eq!(doc["command"], "status");
    assert_eq!(doc["kind"], "config");
    assert!(
        doc["error"]
            .as_str()
            .is_some_and(|e| e.contains("$XDG_STATE_HOME")),
        "{doc}"
    );
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("error: "));
}

/// A search result body with one track per `(uri, name, isrc)`.
fn search_body(tracks: &[(&str, &str, Option<&str>)]) -> serde_json::Value {
    let items: Vec<serde_json::Value> = tracks
        .iter()
        .map(|(uri, name, isrc)| {
            let mut t = serde_json::json!({
                "uri": uri, "name": name, "artists": [{"name": "The Band"}],
                "album": {"name": "Record", "album_type": "album", "release_date": "2004"},
                "duration_ms": 200_000, "explicit": false});
            if let Some(code) = isrc {
                t["external_ids"] = serde_json::json!({ "isrc": code });
            }
            t
        })
        .collect();
    serde_json::json!({"tracks": {"items": items}})
}

#[test]
fn match_decides_by_isrc_text_and_the_llm_and_honours_the_limit() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    std::fs::write(
        Path::new(repo).join("config.toml"),
        "[spotify]\nclient_id = \"test-client-id\"\n",
    )
    .expect("write config");

    // A normalized export: three thumbs-up on one station.
    let pandora = Path::new(repo).join("profiles/alice/pandora");
    std::fs::create_dir_all(&pandora).expect("pandora dir");
    let thumb = |id: &str| {
        serde_json::json!({"feedback_id": id, "track_pandora_id": id, "positive": true,
                           "created": null, "station_id": "1"})
    };
    let track = |id: &str, title: &str, isrc: Option<&str>| {
        serde_json::json!({"pandora_id": id, "music_id": null, "title": title,
            "artist": "The Band", "album": null, "duration_s": 200, "track_num": null,
            "disc_num": null, "explicit_hint": false, "isrc": isrc,
            "artist_pandora_id": null, "album_pandora_id": null})
    };
    let files = [
        (
            "stations.json",
            serde_json::json!([{"station_id": "1", "pandora_id": "ST:1", "name": "Station",
                "created": null, "last_played": null, "is_thumbprint": false,
                "is_shuffle": false, "genres": [], "positive_feedback_count": null,
                "negative_feedback_count": null, "seeds": [],
                "feedback": [thumb("TR:1"), thumb("TR:2"), thumb("TR:3")]}]),
        ),
        ("playlists.json", serde_json::json!([])),
        (
            "collection.json",
            serde_json::json!({"tracks": [], "albums": [], "artists": []}),
        ),
        (
            "bookmarks.json",
            serde_json::json!({"tracks": [], "albums": []}),
        ),
        (
            "tracks.json",
            serde_json::json!({
                "TR:1": track("TR:1", "First", Some("USAAA0400001")),
                "TR:2": track("TR:2", "Second", None),
                "TR:3": track("TR:3", "Third", None),
            }),
        ),
    ];
    for (name, value) in files {
        std::fs::write(pandora.join(name), value.to_string()).expect("write export");
    }
    let state_dir = tmp.path().join("streaming-service-sync/alice");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    std::fs::write(
        state_dir.join("spotify-token.json"),
        serde_json::json!({"stored_at": "2026-10-15T00:00:00Z", "token": {
            "access_token": "access-secret", "refresh_token": "refresh-secret",
            "expires_at": "2099-01-01T00:00:00Z", "scopes": ["user-library-read"]}})
        .to_string(),
    )
    .expect("write token");

    let isrc = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/search")
            .query_param("q", "isrc:USAAA0400001")
            .query_param("type", "track")
            .header("authorization", "Bearer access-secret");
        then.status(200).json_body(search_body(&[(
            "spotify:track:first",
            "First",
            Some("USAAA0400001"),
        )]));
    });
    let second = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/search")
            .query_param("q", "track:\"second\" artist:\"the band\"");
        then.status(200)
            .json_body(search_body(&[("spotify:track:second", "Second", None)]));
    });
    let third_body = search_body(&[
        ("spotify:track:t1", "Third (Live)", None),
        ("spotify:track:t2", "Third Reprise", None),
    ]);
    let third = ["track:\"third\" artist:\"the band\"", "third the band"].map(|q| {
        server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/v1/search")
                .query_param("q", q);
            then.status(200).json_body(third_body.clone());
        })
    });
    let match_cmd = |extra: &[&str]| {
        let mut cmd = sss(tmp.path());
        cmd.env("SSS_SPOTIFY_API_BASE_URL", server.url("/v1"))
            .args([
                "--repo",
                repo,
                "--profile",
                "alice",
                "match",
                "--no-llm",
                "--json",
            ])
            .args(extra);
        cmd
    };

    let out = match_cmd(&["--limit", "1"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let doc = json_document(&out.stdout);
    assert_eq!(doc["command"], "match");
    assert_eq!(doc["stopped"], "limit");
    assert_eq!(doc["kinds"]["tracks"]["processed"], 1);
    assert_eq!(doc["kinds"]["tracks"]["by_method"]["isrc"], 1);
    isrc.assert_calls(1);
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("track TR:1 \"First\" by The Band: matched (isrc)"),
        "progress on stderr"
    );

    let out = match_cmd(&[]).assert().code(0).get_output().clone();
    let doc = json_document(&out.stdout);
    assert_eq!(doc["stopped"], serde_json::Value::Null);
    assert_eq!(doc["kinds"]["tracks"]["processed"], 2);
    assert_eq!(doc["kinds"]["tracks"]["by_status"]["matched"], 1);
    assert_eq!(doc["kinds"]["tracks"]["by_status"]["ambiguous"], 1);
    assert_eq!(doc["kinds"]["tracks"]["by_method"]["exact"], 1);
    isrc.assert_calls(1);
    second.assert_calls(1);
    for mock in &third {
        mock.assert_calls(1);
    }

    let entries =
        std::fs::read_to_string(Path::new(repo).join("profiles/alice/matches/tracks.jsonl"))
            .expect("match file");
    assert_eq!(entries.lines().count(), 3);
    assert!(!entries.contains("secret"));

    // The ambiguous track is up for review, with a search link.
    let out = sss(tmp.path())
        .args([
            "--repo",
            repo,
            "--profile",
            "alice",
            "match",
            "review",
            "--json",
        ])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let doc = json_document(&out.stdout);
    assert_eq!(doc["items"][0]["item_id"], "TR:3");
    assert_eq!(
        doc["items"][0]["search_url"],
        "https://open.spotify.com/search/Third%20The%20Band"
    );

    // Without --no-llm the ambiguous track goes to the matcher through
    // the fake ccc. One whose `which` cannot resolve the agent refuses the
    // run before any batch.
    let fake = tmp.path().join("fake-ccc");
    std::fs::create_dir_all(&fake).expect("fake dir");
    let llm_cmd = |script: &Path| {
        let mut cmd = sss(tmp.path());
        cmd.env("SSS_SPOTIFY_API_BASE_URL", server.url("/v1"))
            .env("SSS_CCC", script)
            .args(["--repo", repo, "--profile", "alice", "match", "--json"]);
        cmd
    };
    let unresolved = write_fake_ccc(&fake, "unresolved", 2);
    let out = llm_cmd(&unresolved).assert().code(1).get_output().clone();
    let doc = json_document(&out.stdout);
    assert_eq!(doc["kind"], "config", "{doc}");
    assert!(doc["error"].as_str().unwrap().contains("exit 2"), "{doc}");
    assert_eq!(std::fs::read_to_string(fake.join("count")).ok(), None);
    for log in ["argv", "env"] {
        std::fs::remove_file(fake.join(log)).expect("clear the log");
    }

    // A malformed first reply is sent back with the complaint; the second
    // is taken.
    let reply = |result: &str| serde_json::json!({"session": "sess-1", "result": result});
    std::fs::write(
        fake.join("out1"),
        reply("Here you go: {\"decisions\": []}").to_string(),
    )
    .expect("reply 1");
    std::fs::write(
        fake.join("out2"),
        reply(
            "```json\n{\"decisions\":[{\"item\":\"TR:3\",\"choice\":\"spotify:track:t2\",\
             \"confidence\":\"medium\",\"reason\":\"the same song\"}]}\n```",
        )
        .to_string(),
    )
    .expect("reply 2");
    let good = write_fake_ccc(&fake, "good", 0);
    let out = llm_cmd(&good).assert().code(0).get_output().clone();
    let doc = json_document(&out.stdout);
    assert_eq!(doc["kinds"]["tracks"]["processed"], 1, "{doc}");
    assert_eq!(doc["kinds"]["tracks"]["by_method"]["llm"], 1);
    assert_eq!(doc["kinds"]["tracks"]["llm_batches"], 1);
    let argv = std::fs::read_to_string(fake.join("argv")).expect("argv log");
    let argv: Vec<&str> = argv.lines().collect();
    assert_eq!(argv[0], "which music-matcher");
    assert!(
        argv[1].starts_with("run music-matcher --json --cwd "),
        "{argv:?}"
    );
    assert_eq!(
        argv[2],
        "resume sess-1 --json no decision for TR:3. Reply with only the JSON object, nothing else."
    );
    let agents_config = tmp.path().join("streaming-service-sync/ccc/agents.toml");
    let env = std::fs::read_to_string(fake.join("env")).expect("env log");
    assert!(env.lines().all(|l| Path::new(l) == agents_config), "{env}");
    let entries =
        std::fs::read_to_string(Path::new(repo).join("profiles/alice/matches/tracks.jsonl"))
            .expect("match file");
    let last: serde_json::Value =
        serde_json::from_str(entries.lines().last().expect("an entry")).expect("json");
    assert_eq!(last["item_id"], "TR:3");
    assert_eq!(last["status"], "matched");
    assert_eq!(last["spotify_uri"], "spotify:track:t2");
    assert_eq!(last["method"], "llm");
    assert_eq!(last["session"], "sess-1");
    assert_eq!(last["reason"], "the same song");
    assert!(
        Path::new(repo)
            .join("profiles/alice/matches/llm/sess-1.json")
            .exists()
    );
    for mock in &third {
        mock.assert_calls(1);
    }
}

/// Writes a fake `ccc` named `name` into `dir`: it logs its arguments and
/// `CCC_AGENTS_CONFIG`, exits `which_exit` for `which`, and otherwise
/// prints `outN` and exits `exitN` (default 0) on its Nth call.
///
/// The script is written by a child `sh`, so this process never holds a
/// write descriptor on it: a test thread forking meanwhile would inherit
/// one, and exec'ing the script would then fail with ETXTBSY.
fn write_fake_ccc(dir: &Path, name: &str, which_exit: i32) -> PathBuf {
    use std::io::Write;
    let path = dir.join(name);
    let d = dir.display();
    let script = format!(
        "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{d}/argv'\n\
             printf '%s\\n' \"$CCC_AGENTS_CONFIG\" >> '{d}/env'\n\
             if [ \"$1\" = which ]; then echo 'no definition for agent type' >&2; exit {which_exit}; fi\n\
             n=$(( $(cat '{d}/count' 2>/dev/null || echo 0) + 1 ))\n\
             echo $n > '{d}/count'\n\
             cat \"{d}/out$n\" 2>/dev/null\n\
             exit $(cat \"{d}/exit$n\" 2>/dev/null || echo 0)\n"
    );
    let mut sh = std::process::Command::new("sh")
        .args(["-c", "cat > \"$0\" && chmod 755 \"$0\""])
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("spawn sh");
    sh.stdin
        .take()
        .expect("sh stdin")
        .write_all(script.as_bytes())
        .expect("write fake ccc");
    assert!(sh.wait().expect("wait sh").success(), "write fake ccc");
    path
}

/// A track as `tracks.json` holds it, by The Band.
fn track_json(id: &str, title: &str) -> serde_json::Value {
    serde_json::json!({"pandora_id": id, "music_id": null, "title": title,
        "artist": "The Band", "album": null, "duration_s": 200, "track_num": null,
        "disc_num": null, "explicit_hint": false, "isrc": null,
        "artist_pandora_id": null, "album_pandora_id": null})
}

/// Writes profile alice's normalized export: one station whose feedback is
/// `thumbs` (`(track id, title, positive)`), each track in `tracks.json`.
fn write_export(repo: &str, thumbs: &[(&str, &str, bool)]) {
    let feedback: Vec<_> = thumbs
        .iter()
        .enumerate()
        .map(|(i, (id, _, positive))| {
            serde_json::json!({"feedback_id": format!("F{i}"), "track_pandora_id": id,
                "positive": positive, "created": null, "station_id": "1"})
        })
        .collect();
    let tracks: serde_json::Map<_, _> = thumbs
        .iter()
        .map(|(id, title, _)| ((*id).to_owned(), track_json(id, title)))
        .collect();
    let pandora = Path::new(repo).join("profiles/alice/pandora");
    std::fs::create_dir_all(&pandora).expect("pandora dir");
    let files = [
        (
            "stations.json",
            serde_json::json!([{"station_id": "1", "pandora_id": "ST:1", "name": "Station",
                "created": null, "last_played": null, "is_thumbprint": false,
                "is_shuffle": false, "genres": [], "positive_feedback_count": null,
                "negative_feedback_count": null, "seeds": [], "feedback": feedback}]),
        ),
        ("playlists.json", serde_json::json!([])),
        (
            "collection.json",
            serde_json::json!({"tracks": [], "albums": [], "artists": []}),
        ),
        (
            "bookmarks.json",
            serde_json::json!({"tracks": [], "albums": []}),
        ),
        ("tracks.json", serde_json::Value::Object(tracks)),
    ];
    for (name, value) in files {
        std::fs::write(pandora.join(name), value.to_string()).expect("write export");
    }
}

#[test]
fn match_review_lists_only_what_the_push_switches_select() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    write_export(
        repo,
        &[("TR:1", "Liked", true), ("TR:2", "Disliked", false)],
    );
    let matches = Path::new(repo).join("profiles/alice/matches");
    std::fs::create_dir_all(&matches).expect("matches dir");
    let entries: String = ["TR:1", "TR:2"]
        .iter()
        .map(|id| {
            format!(
                "{}\n",
                serde_json::json!({"pandora_id": id, "status": "unmatched",
                    "decided_at": "2026-10-15T00:00:00Z"})
            )
        })
        .collect();
    std::fs::write(matches.join("tracks.jsonl"), entries).expect("write matches");
    let review = |thumbs_down_playlist: bool| {
        std::fs::write(
            Path::new(repo).join("config.toml"),
            format!("[push]\nthumbs_down_playlist = {thumbs_down_playlist}\n"),
        )
        .expect("write config");
        let out = sss(tmp.path())
            .args([
                "--repo",
                repo,
                "--profile",
                "alice",
                "match",
                "review",
                "--json",
            ])
            .assert()
            .code(0)
            .get_output()
            .clone();
        let doc = json_document(&out.stdout);
        let ids: Vec<String> = doc["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|i| i["item_id"].as_str().expect("id").to_owned())
            .collect();
        ids
    };
    // Push sends no thumbs-down track with the switch off, so there is
    // nothing to decide about one.
    assert_eq!(review(false), ["TR:1"]);
    assert_eq!(review(true), ["TR:1", "TR:2"]);
}

#[test]
fn push_dry_run_writes_nothing_then_push_creates_and_a_second_push_is_idle() {
    const TRACK: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    std::fs::write(
        Path::new(repo).join("config.toml"),
        "[spotify]\nclient_id = \"test-client-id\"\n[push]\nliked_songs = false\n",
    )
    .expect("write config");

    write_export(repo, &[("TR:1", "First", true)]);
    sss(tmp.path())
        .args([
            "--repo",
            repo,
            "--profile",
            "alice",
            "match",
            "set",
            "TR:1",
            TRACK,
        ])
        .assert()
        .code(0);
    let state_dir = tmp.path().join("streaming-service-sync/alice");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    std::fs::write(
        state_dir.join("spotify-token.json"),
        serde_json::json!({"stored_at": "2026-10-15T00:00:00Z", "token": {
            "access_token": "access-secret", "refresh_token": "refresh-secret",
            "expires_at": "2099-01-01T00:00:00Z",
            "scopes": ["playlist-modify-private", "user-library-modify", "user-library-read",
                "user-follow-read", "playlist-read-private", "playlist-read-collaborative"]}})
        .to_string(),
    )
    .expect("write token");
    let push = |extra: &[&str]| {
        let out = sss(tmp.path())
            .env("SSS_SPOTIFY_API_BASE_URL", server.url("/v1"))
            .args(["--repo", repo, "--profile", "alice", "push", "--json"])
            .args(extra)
            .assert()
            .code(0)
            .get_output()
            .clone();
        json_document(&out.stdout)
    };
    let ledger = Path::new(repo).join("profiles/alice/spotify/state.json");

    let mut any = server.mock(|_, then| {
        then.status(500);
    });
    let doc = push(&["--dry-run"]);
    any.assert_calls(0);
    any.delete();
    assert_eq!(doc["command"], "push");
    assert_eq!(doc["dry_run"], true);
    let planned = &doc["parts"]["stations"]["playlists"][0];
    assert_eq!(planned["key"], "ST:1");
    assert_eq!(planned["action"], "create");
    assert_eq!(planned["to_add"], 1);
    assert_eq!(planned["added"], 0);
    assert!(!ledger.exists(), "a dry run writes no ledger");

    let create = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/me/playlists")
            .header("authorization", "Bearer access-secret");
        then.status(201)
            .json_body(serde_json::json!({"id": "pl1", "snapshot_id": "s0"}));
    });
    let add = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/playlists/pl1/items")
            .json_body(serde_json::json!({ "uris": [TRACK] }));
        then.status(201)
            .json_body(serde_json::json!({"snapshot_id": "s1"}));
    });
    let items = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/playlists/pl1/items");
        then.status(200)
            .json_body(serde_json::json!({"items": [{"item": {"uri": TRACK}}], "next": null}));
    });
    let on_account = on_account_mock(&server, "pl1");

    let doc = push(&[]);
    assert_eq!(doc["dry_run"], false);
    let pushed = &doc["parts"]["stations"]["playlists"][0];
    assert_eq!(pushed["action"], "create");
    assert_eq!(pushed["added"], 1);
    assert_eq!(pushed["spotify_id"], "pl1");
    create.assert_calls(1);
    add.assert_calls(1);
    items.assert_calls(0);
    on_account.assert_calls(0);
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger).expect("ledger")).expect("json");
    assert_eq!(state["playlists"]["ST:1"]["status"], "created");
    assert_eq!(state["playlists"]["ST:1"]["spotify_id"], "pl1");

    let doc = push(&[]);
    let again = &doc["parts"]["stations"]["playlists"][0];
    assert_eq!(again["action"], "skip");
    assert_eq!(again["added"], 0);
    create.assert_calls(1);
    add.assert_calls(1);
    items.assert_calls(1);
    on_account.assert_calls(1);
}

/// `GET me/library/contains` for playlist `id` alone, answering that the
/// account holds it.
fn on_account_mock<'a>(server: &'a httpmock::MockServer, id: &str) -> httpmock::Mock<'a> {
    let uri = format!("spotify:playlist:{id}");
    server.mock(move |when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/me/library/contains")
            .query_param("uris", uri);
        then.status(200).json_body(serde_json::json!([true]));
    })
}

#[test]
fn report_writes_the_profile_report_and_prints_its_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    write_export(
        repo,
        &[("TR:1", "Liked", true), ("TR:2", "Disliked", false)],
    );
    let path = Path::new(repo).join("profiles/alice/report.md");
    let out = sss(tmp.path())
        .args(["--repo", repo, "--profile", "alice", "report", "--json"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let doc = json_document(&out.stdout);
    assert_eq!(doc["ok"], true);
    assert_eq!(doc["command"], "report");
    assert_eq!(doc["path"], path.to_str().expect("utf-8 path"));
    let report = std::fs::read_to_string(&path).expect("report.md");
    assert!(report.starts_with("# alice: to Spotify\n"), "{report}");
    assert!(
        report.contains("\"Disliked\" by The Band: [search]("),
        "{report}"
    );

    let out = sss(tmp.path())
        .args(["--repo", repo, "--profile", "alice", "report"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{}\n", path.display())
    );
}

#[test]
fn match_questions_and_answer_close_the_loop_without_the_network() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    write_export(repo, &[("TR:1", "Song", true)]);
    let matches = Path::new(repo).join("profiles/alice/matches");
    std::fs::create_dir_all(&matches).expect("matches dir");
    let candidates: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|id| serde_json::json!({"uri": format!("spotify:track:{id}"), "name": "Song", "score": 0.5}))
        .collect();
    std::fs::write(
        matches.join("tracks.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({"pandora_id": "TR:1", "status": "ambiguous",
                "candidates": candidates, "decided_at": "2026-10-15T00:00:00Z"})
        ),
    )
    .expect("write matches");
    // Nothing listens on port 1: a Spotify request would fail the command.
    let run = |args: &[&str], stdin: &str| {
        sss(tmp.path())
            .env("SSS_SPOTIFY_API_BASE_URL", "http://127.0.0.1:1")
            .args(["--repo", repo, "--profile", "alice", "match"])
            .args(args)
            .arg("--json")
            .write_stdin(stdin.to_owned())
            .assert()
            .get_output()
            .clone()
    };

    let out = run(&["questions"], "");
    assert_eq!(out.status.code(), Some(0));
    let doc = json_document(&out.stdout);
    assert_eq!(doc["command"], "match questions");
    assert_eq!(doc["items"][0]["item"], "TR:1");
    assert_eq!(doc["items"][0]["kind"], "track");
    let offered: Vec<_> = doc["items"][0]["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .map(|c| c["id"].as_str().expect("id").to_owned())
        .collect();
    assert_eq!(
        offered,
        ["spotify:track:a", "spotify:track:b", "spotify:track:c"]
    );
    assert_eq!(doc["remaining"], 0);

    let out = run(
        &["answer"],
        r#"{"decisions":[{"item":"TR:1","choice":"spotify:track:z","confidence":"high"}]}"#,
    );
    assert_eq!(out.status.code(), Some(1));
    let doc = json_document(&out.stdout);
    assert_eq!(doc["kind"], "input");
    assert_eq!(doc["details"]["problems"][0]["item"], "TR:1");
    assert!(String::from_utf8_lossy(&out.stderr).contains("error: 1 problem"));

    let out = run(
        &["answer"],
        "{\"decisions\":[{\"item\":\"TR:1\",\"choice\":\"spotify:track:c\",\n\"confidence\":\"medium\",\"reason\":\"same\"}]}\n\n",
    );
    assert_eq!(out.status.code(), Some(0));
    let doc = json_document(&out.stdout);
    assert_eq!(
        doc["recorded"],
        serde_json::json!([{"item": "TR:1", "status": "matched",
            "spotify_uri": "spotify:track:c", "confidence": "medium"}])
    );
    let review = json_document(&run(&["review"], "").stdout);
    assert_eq!(review["items"], serde_json::json!([]));
}

#[test]
fn guide_prints_with_no_data_repository() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs");
    // No state root can be located either: the guide needs none.
    let out = sss(tmp.path())
        .env_remove("XDG_STATE_HOME")
        .env_remove("HOME")
        .env("XDG_DATA_HOME", tmp.path().join("nowhere"))
        .arg("guide")
        .assert()
        .code(0)
        .get_output()
        .clone();
    assert_eq!(
        out.stdout,
        std::fs::read(docs.join("llm-guide.md")).expect("guide")
    );
    assert!(out.stderr.is_empty());
    let out = sss(tmp.path())
        .env("XDG_DATA_HOME", tmp.path().join("nowhere"))
        .args(["guide", "matching", "--json"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let doc = json_document(&out.stdout);
    assert_eq!(doc["topic"], "matching");
    assert_eq!(
        doc["text"].as_str().expect("text").as_bytes(),
        std::fs::read(docs.join("matching-protocol.md")).expect("protocol")
    );
    assert!(!tmp.path().join("nowhere").exists());
}

/// The documentation's public example tracks.
const TRACK_A: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";
const TRACK_B: &str = "spotify:track:1301WleyT98MSxVHPZCA6M";

/// Every scope a playback command needs.
const PLAYBACK_SCOPES: &[&str] = &["user-read-playback-state", "user-modify-playback-state"];

/// An initialized repository under `tmp` with a client id and profile
/// alice's Spotify token granted `scopes`; returns the repository path.
fn repo_with_spotify_token(tmp: &Path, scopes: &[&str]) -> String {
    let repo = tmp.join("data");
    let repo = repo.to_str().expect("utf-8 path").to_owned();
    sss(tmp).args(["--repo", &repo, "init"]).assert().code(0);
    std::fs::write(
        Path::new(&repo).join("config.toml"),
        "[spotify]\nclient_id = \"test-client-id\"\n",
    )
    .expect("write config");
    let state_dir = tmp.join("streaming-service-sync/alice");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    std::fs::write(
        state_dir.join("spotify-token.json"),
        serde_json::json!({"stored_at": "2026-10-15T00:00:00Z", "token": {
            "access_token": "access-secret", "refresh_token": "refresh-secret",
            "expires_at": "2099-01-01T00:00:00Z", "scopes": scopes}})
        .to_string(),
    )
    .expect("write token");
    repo
}

/// Runs `sss --repo repo --profile alice <args> --json` against the mock
/// Spotify API at `server`: its exit code and its JSON document.
fn spotify_json(
    tmp: &Path,
    server: &httpmock::MockServer,
    repo: &str,
    args: &[&str],
) -> (i32, serde_json::Value) {
    let out = sss(tmp)
        .env("SSS_SPOTIFY_API_BASE_URL", server.url("/v1"))
        .args(["--repo", repo, "--profile", "alice"])
        .args(args)
        .arg("--json")
        .output()
        .expect("sss runs");
    (
        out.status.code().expect("exit code"),
        json_document(&out.stdout),
    )
}

/// `GET me/player/devices` answers with Firefox, open and inactive.
fn one_inactive_firefox(server: &httpmock::MockServer) -> httpmock::Mock<'_> {
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/me/player/devices");
        then.status(200).json_body(serde_json::json!({"devices": [
            {"id": "dev-firefox", "name": "Firefox", "type": "Computer",
             "is_active": false, "is_restricted": false, "volume_percent": 100}]}));
    })
}

/// `GET me/player` answers Firefox playing `TRACK_A`.
fn playing_on_firefox(server: &httpmock::MockServer) -> httpmock::Mock<'_> {
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/me/player");
        then.status(200).json_body(serde_json::json!({
            "device": {"id": "dev-firefox", "name": "Firefox", "type": "Computer",
                       "is_active": true, "is_restricted": false, "volume_percent": 100},
            "is_playing": true, "progress_ms": 1000, "shuffle_state": false,
            "repeat_state": "off", "context": null,
            "item": {"type": "track", "uri": TRACK_A, "name": "Song",
                     "artists": [{"name": "Artist"}], "duration_ms": 215000}}));
    })
}

#[test]
fn play_sends_the_tracks_to_the_only_open_player_and_reports_the_state() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_spotify_token(tmp.path(), PLAYBACK_SCOPES);
    let devices = one_inactive_firefox(&server);
    let play = server.mock(|when, then| {
        when.method(httpmock::Method::PUT)
            .path("/v1/me/player/play")
            .query_param("device_id", "dev-firefox")
            .header("authorization", "Bearer access-secret")
            .json_body(serde_json::json!({"uris": [TRACK_A]}));
        then.status(204);
    });
    let state = playing_on_firefox(&server);
    let (code, doc) = spotify_json(tmp.path(), &server, &repo, &["play", TRACK_A]);
    assert_eq!(code, 0, "{doc}");
    devices.assert_calls(1);
    play.assert_calls(1);
    state.assert_calls(1);
    assert_eq!(doc["command"], "play");
    assert_eq!(doc["device"]["name"], "Firefox");
    assert_eq!(doc["state"]["is_playing"], true);
    assert_eq!(doc["state"]["device"]["name"], "Firefox");
    assert_eq!(doc["state"]["item"]["id"], TRACK_A);
    assert_eq!(doc["hint"], serde_json::Value::Null);
}

#[test]
fn playback_without_the_playback_scopes_is_kind_auth_before_any_request() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_spotify_token(tmp.path(), &["user-library-modify"]);
    let any = server.mock(|_, then| {
        then.status(500);
    });
    let (code, doc) = spotify_json(tmp.path(), &server, &repo, &["play", TRACK_A]);
    assert_eq!(code, 1);
    any.assert_calls(0);
    assert_eq!(doc["kind"], "auth");
    let error = doc["error"].as_str().expect("error");
    assert!(error.contains("user-modify-playback-state"), "{error}");
    assert!(
        error.contains("sss spotify auth --profile alice"),
        "{error}"
    );
}

/// The scopes `sss spotify auth` asks for, in order.
const ALL_SCOPES: &[&str] = &[
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify-private",
    "playlist-modify-public",
    "user-library-read",
    "user-library-modify",
    "user-follow-read",
    "user-follow-modify",
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-read-currently-playing",
    "user-read-playback-position",
    "user-read-recently-played",
    "user-top-read",
];

#[test]
fn spotify_auth_asks_for_every_scope_whatever_public_playlists_says() {
    for public in ["false", "true"] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = repo_with_spotify_token(tmp.path(), &[]);
        std::fs::write(
            Path::new(&repo).join("config.toml"),
            format!(
                "[spotify]\nclient_id = \"test-client-id\"\n[push]\npublic_playlists = {public}\n"
            ),
        )
        .expect("write config");
        let out = sss(tmp.path())
            .env("SSS_SPOTIFY_AUTH_BASE_URL", "https://accounts.example")
            .args(["--repo", &repo, "--profile", "alice", "spotify", "auth"])
            .assert()
            .code(0)
            .get_output()
            .clone();
        let address = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        assert_eq!(
            url_param(&address, "scope").replace('+', " "),
            ALL_SCOPES.join(" "),
            "public_playlists = {public}"
        );
    }
}

#[test]
fn status_names_the_scopes_an_old_token_lacks() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let old_eight = [
        "playlist-read-private",
        "playlist-modify-private",
        "user-library-read",
        "user-library-modify",
        "user-follow-read",
        "user-follow-modify",
        "user-read-playback-state",
        "user-modify-playback-state",
    ];
    let repo = repo_with_spotify_token(tmp.path(), &old_eight);
    let new_six = [
        "playlist-read-collaborative",
        "playlist-modify-public",
        "user-read-currently-playing",
        "user-read-playback-position",
        "user-read-recently-played",
        "user-top-read",
    ];
    let out = sss(tmp.path())
        .args(["--repo", &repo, "status", "--json"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let doc = json_document(&out.stdout);
    assert_eq!(
        doc["profiles"][0]["spotify_token"]["missing_scopes"],
        serde_json::json!(new_six)
    );
    let out = sss(tmp.path())
        .args(["--repo", &repo, "status"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(&format!(
            "    lacks {}; run `sss spotify auth --profile alice` to sign in again\n",
            new_six.join(", ")
        )),
        "{text}"
    );
}

#[test]
fn play_with_no_player_open_is_kind_player() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_spotify_token(tmp.path(), PLAYBACK_SCOPES);
    server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/me/player/devices");
        then.status(200)
            .json_body(serde_json::json!({"devices": []}));
    });
    let play = server.mock(|when, then| {
        when.method(httpmock::Method::PUT)
            .path("/v1/me/player/play");
        then.status(204);
    });
    let (code, doc) = spotify_json(tmp.path(), &server, &repo, &["play"]);
    assert_eq!(code, 1);
    play.assert_calls(0);
    assert_eq!(doc["kind"], "player");
    assert_eq!(
        doc["details"],
        serde_json::json!({"reason": "NO_ACTIVE_DEVICE", "devices": []})
    );
}

#[test]
fn a_queue_stopped_partway_names_what_landed() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_spotify_token(tmp.path(), PLAYBACK_SCOPES);
    one_inactive_firefox(&server);
    let first = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/me/player/queue")
            .query_param("uri", TRACK_A);
        then.status(200).body("snapshot1");
    });
    let second = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/me/player/queue")
            .query_param("uri", TRACK_B);
        then.status(503);
    });
    let (code, doc) = spotify_json(tmp.path(), &server, &repo, &["queue", TRACK_A, TRACK_B]);
    assert_eq!(code, 1);
    first.assert_calls(1);
    second.assert_calls(1);
    assert_eq!(doc["kind"], "unavailable");
    let error = doc["error"].as_str().expect("error");
    assert!(
        error.starts_with(&format!("queued 1 of 2 ({TRACK_A}), then:")),
        "{error}"
    );
}

#[test]
fn search_sends_one_request_per_query() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    // Search needs no playback scope.
    let repo = repo_with_spotify_token(tmp.path(), &[]);
    let found = |q: &'static str, uri: &'static str| {
        server.mock(move |when, then| {
            when.method(httpmock::Method::GET)
                .path("/v1/search")
                .query_param("q", q)
                .query_param("type", "track")
                .query_param("limit", "10");
            then.status(200)
                .json_body(serde_json::json!({"tracks": {"items": [
                {"uri": uri, "name": q, "artists": [{"name": "Artist"}]}]}}));
        })
    };
    let x = found("x", TRACK_A);
    let y = found("y", TRACK_B);
    let (code, doc) = spotify_json(
        tmp.path(),
        &server,
        &repo,
        &["search", "x", "y", "--only", "tracks"],
    );
    assert_eq!(code, 0, "{doc}");
    x.assert_calls(1);
    y.assert_calls(1);
    assert_eq!(doc["command"], "search");
    let results = doc["results"].as_array().expect("results");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["query"], "x");
    assert_eq!(results[0]["tracks"][0]["id"], TRACK_A);
    assert_eq!(results[1]["tracks"][0]["id"], TRACK_B);
    assert_eq!(results[1]["artists"], serde_json::json!([]));
}

/// The push fixture's export (station ST:1, "Station") and a ledger that
/// records its playlist `pl1`.
fn station_with_playlist(repo: &str) {
    write_export(repo, &[("TR:1", "First", true)]);
    let spotify = Path::new(repo).join("profiles/alice/spotify");
    std::fs::create_dir_all(&spotify).expect("spotify dir");
    std::fs::write(
        spotify.join("state.json"),
        serde_json::json!({"playlists": {"ST:1": {"status": "created", "spotify_id": "pl1"}}})
            .to_string(),
    )
    .expect("write ledger");
}

/// A `PUT me/player/play` mock that answers 204 and keeps the body it got.
fn recording_play(
    server: &httpmock::MockServer,
) -> (
    httpmock::Mock<'_>,
    std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
) {
    let body = std::sync::Arc::new(std::sync::Mutex::new(None));
    let seen = body.clone();
    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::PUT)
            .path("/v1/me/player/play")
            .is_true(move |req| {
                *seen.lock().expect("lock") = serde_json::from_slice(req.body_ref()).ok();
                true
            });
        then.status(204);
    });
    (mock, body)
}

#[test]
fn play_station_plays_its_pushed_playlist() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_spotify_token(tmp.path(), PLAYBACK_SCOPES);
    station_with_playlist(&repo);
    one_inactive_firefox(&server);
    playing_on_firefox(&server);
    let (play, body) = recording_play(&server);
    let (code, doc) = spotify_json(
        tmp.path(),
        &server,
        &repo,
        &["play", "--station", "station"],
    );
    assert_eq!(code, 0, "{doc}");
    play.assert_calls(1);
    assert_eq!(
        body.lock().expect("lock").take(),
        Some(serde_json::json!({"context_uri": "spotify:playlist:pl1"}))
    );
}

#[test]
fn play_station_shuffled_starts_at_a_random_track() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_spotify_token(tmp.path(), PLAYBACK_SCOPES);
    station_with_playlist(&repo);
    one_inactive_firefox(&server);
    playing_on_firefox(&server);
    let count = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/playlists/pl1/items")
            .query_param("fields", "total")
            .query_param("limit", "1");
        then.status(200).json_body(serde_json::json!({"total": 40}));
    });
    let shuffle = server.mock(|when, then| {
        when.method(httpmock::Method::PUT)
            .path("/v1/me/player/shuffle")
            .query_param("state", "true");
        then.status(200).body("snapshot1");
    });
    let (play, body) = recording_play(&server);
    let (code, doc) = spotify_json(
        tmp.path(),
        &server,
        &repo,
        &["play", "--station", "Station", "--shuffle"],
    );
    assert_eq!(code, 0, "{doc}");
    count.assert_calls(1);
    shuffle.assert_calls(1);
    play.assert_calls(1);
    let body = body.lock().expect("lock").take().expect("play body");
    assert_eq!(body["context_uri"], "spotify:playlist:pl1");
    let position = body["offset"]["position"].as_u64().expect("position");
    assert!(position < 40, "{body}");
}

#[test]
fn history_reads_the_last_plays_in_one_request() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_spotify_token(tmp.path(), &["user-read-recently-played"]);
    let read = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/me/player/recently-played")
            .query_param("limit", "50");
        then.status(200).json_body(serde_json::json!({"items": [
            {"played_at": "2026-10-16T13:02:11.483Z",
             "track": {"type": "track", "uri": TRACK_A, "name": "Song",
                       "artists": [{"name": "Artist"}], "duration_ms": 215000},
             "context": null}]}));
    });
    let (code, doc) = spotify_json(tmp.path(), &server, &repo, &["history"]);
    assert_eq!(code, 0, "{doc}");
    read.assert_calls(1);
    assert_eq!(doc["command"], "history");
    assert_eq!(doc["items"][0]["played_at"], "2026-10-16T13:02:11.483Z");
    assert_eq!(doc["items"][0]["track"]["id"], TRACK_A);
}

#[test]
fn history_without_its_scope_is_kind_auth_before_any_request() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_spotify_token(tmp.path(), &["user-library-read"]);
    let any = server.mock(|_, then| {
        then.status(500);
    });
    let (code, doc) = spotify_json(tmp.path(), &server, &repo, &["history"]);
    assert_eq!(code, 1);
    any.assert_calls(0);
    assert_eq!(doc["kind"], "auth");
    let error = doc["error"].as_str().expect("error");
    assert!(error.contains("user-read-recently-played"), "{error}");
}

#[test]
fn top_sends_the_range_and_limit_given() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_spotify_token(tmp.path(), &["user-top-read"]);
    let read = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/me/top/artists")
            .query_param("time_range", "short_term")
            .query_param("limit", "5");
        then.status(200).json_body(serde_json::json!({"items": [
            {"uri": "spotify:artist:0OdUWJ0sBjDrqHygGUXeCF", "name": "Artist"}]}));
    });
    let (code, doc) = spotify_json(
        tmp.path(),
        &server,
        &repo,
        &["top", "artists", "--range", "short", "--limit", "5"],
    );
    assert_eq!(code, 0, "{doc}");
    read.assert_calls(1);
    assert_eq!(doc["kind"], "artists");
    assert_eq!(doc["range"], "short");
    assert_eq!(
        doc["items"][0]["id"],
        "spotify:artist:0OdUWJ0sBjDrqHygGUXeCF"
    );
}

/// The scopes both playlist commands need.
const PLAYLIST_SCOPES: &[&str] = &["playlist-read-private", "playlist-read-collaborative"];

/// A wire playlist owned by `owner`.
fn wire_playlist(id: &str, owner: &str) -> serde_json::Value {
    serde_json::json!({"uri": format!("spotify:playlist:{id}"), "name": "Morning",
        "description": "", "public": false, "collaborative": false,
        "owner": {"id": owner, "display_name": null},
        "items": {"href": "https://api.spotify.com/v1/x", "total": 1}})
}

#[test]
fn playlists_names_the_account_and_reads_every_page() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_spotify_token(tmp.path(), PLAYLIST_SCOPES);
    let me = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/me");
        then.status(200)
            .json_body(serde_json::json!({"id": "alice-spotify", "display_name": "Alice"}));
    });
    let first: Vec<_> = (0..50)
        .map(|n| wire_playlist(&format!("{n:0>22}"), "alice-spotify"))
        .collect();
    let page1 = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/me/playlists")
            .query_param("offset", "0");
        then.status(200).json_body(serde_json::json!({
            "items": first, "next": "https://api.spotify.com/v1/me/playlists?offset=50"}));
    });
    let page2 = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/me/playlists")
            .query_param("offset", "50");
        then.status(200).json_body(serde_json::json!({
            "items": [wire_playlist("37i9dQZF1DXcBWIGoYBM5M", "bob-spotify")],
            "next": null}));
    });
    let (code, doc) = spotify_json(tmp.path(), &server, &repo, &["playlists"]);
    assert_eq!(code, 0, "{doc}");
    me.assert_calls(1);
    page1.assert_calls(1);
    page2.assert_calls(1);
    assert_eq!(doc["user"]["id"], "alice-spotify");
    let playlists = doc["playlists"].as_array().expect("playlists");
    assert_eq!(playlists.len(), 51);
    assert_eq!(playlists[50]["owner"]["id"], "bob-spotify");
    assert_eq!(playlists[50]["owner"]["name"], "bob-spotify");
}

#[test]
fn playlist_reads_the_details_then_the_items() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_spotify_token(tmp.path(), PLAYLIST_SCOPES);
    let id = "37i9dQZF1DXcBWIGoYBM5M";
    let details = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/playlists/{id}"));
        then.status(200)
            .json_body(wire_playlist(id, "alice-spotify"));
    });
    let items = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/playlists/{id}/items"))
            .query_param("limit", "100");
        then.status(200).json_body(serde_json::json!({"items": [
            {"item": {"type": "track", "uri": TRACK_B, "name": "Other song",
                      "artists": [{"name": "Artist"}]}}], "next": null}));
    });
    let (code, doc) = spotify_json(
        tmp.path(),
        &server,
        &repo,
        &["playlist", &format!("spotify:playlist:{id}")],
    );
    assert_eq!(code, 0, "{doc}");
    details.assert_calls(1);
    items.assert_calls(1);
    let entry = &doc["playlists"][0];
    assert_eq!(entry["playlist"]["total"], 1);
    assert_eq!(entry["items"][0]["id"], TRACK_B);
}

/// Writes `files` (path relative to `root`, contents) under `root`.
fn lay_out(root: &Path, files: &[(&str, &str)]) {
    for (rel, text) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }
}

#[test]
fn youtube_import_files_a_takeout_tree_as_a_raw_run() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    let archive = tmp.path().join("takeout");
    let base = "Takeout/YouTube and YouTube Music";
    let history = r#"[{"header": "YouTube Music", "title": "Watched Calm Waters",
        "titleUrl": "https://music.youtube.com/watch?v=abcDEF12345",
        "subtitles": [{"name": "Chill Out Radio - Topic",
                       "url": "https://www.youtube.com/channel/UCcccccccccccccccccccccc"}],
        "time": "2026-01-02T03:04:05.678Z", "products": ["YouTube"]}]"#;
    lay_out(
        &archive,
        &[
            (&format!("{base}/history/watch-history.json"), history),
            (&format!("{base}/history/search-history.json"), "[]"),
            (
                &format!("{base}/playlists/playlists.csv"),
                "Playlist ID,Playlist Title (Original)\n\
                 PLaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa,Road Trip\n\n\n\n\n\n",
            ),
            (
                &format!("{base}/playlists/Road Trip-videos.csv"),
                "Video ID,Playlist Video Creation Timestamp\n \
                 -bcDEF12345,2025-01-02T00:00:00+00:00\n\n\n\n\n\n",
            ),
            (
                &format!("{base}/subscriptions/subscriptions.csv"),
                "Channel Id,Channel Url,Channel Title\n\
                 UCcccccccccccccccccccccc,http://www.youtube.com/channel/UCcccccccccccccccccccccc,Radio \n",
            ),
            (
                &format!("{base}/music (library and uploads)/music library songs.csv"),
                "Video ID,Song Title,Album Title,Artist Name 1\nabcDEF12345,Calm Waters,,Chill\n",
            ),
            (&format!("{base}/video metadata/videos.csv"), "Video ID\n"),
        ],
    );
    let out = sss(tmp.path())
        .args([
            "--repo",
            repo,
            "--profile",
            "alice",
            "--json",
            "youtube",
            "import",
        ])
        .arg(&archive)
        .assert()
        .code(0)
        .get_output()
        .clone();
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON document");
    assert_eq!(doc["ok"], true);
    assert_eq!(doc["command"], "youtube import");
    let manifest = &doc["manifest"];
    assert_eq!(manifest["complete"], true);
    for step in ["history", "playlists", "subscriptions", "library"] {
        assert_eq!(manifest["steps"][step]["status"], "ok", "{step}");
        assert_eq!(manifest["steps"][step]["count"], 1, "{step}");
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("history: ok, 1 records, 0 skipped"),
        "{stderr}"
    );
    let run_dir = PathBuf::from(doc["run_dir"].as_str().expect("run_dir"));
    assert!(run_dir.starts_with(Path::new(repo).join("profiles/alice/youtube/raw")));
    assert_eq!(
        std::fs::read_to_string(run_dir.join("takeout/history/watch-history.json")).expect("copy"),
        history
    );
    let copied: Vec<PathBuf> = walk(&run_dir);
    assert_eq!(copied.len(), 6, "five copies and the manifest: {copied:?}");
    assert!(
        copied
            .iter()
            .all(|p| !p.ends_with("videos.csv") && !p.ends_with("search-history.json")),
        "{copied:?}"
    );

    // The complete run was normalized: the five files and the summary.
    let normalized = &doc["normalized"];
    assert_eq!(normalized["run_id"], doc["run_id"]);
    assert_eq!(normalized["records"], 1);
    assert_eq!(normalized["plays"], 1);
    assert_eq!(
        normalized["videos"], 2,
        "the played song and the playlist item"
    );
    assert_eq!(normalized["songs"], 1);
    assert_eq!(normalized["library_songs"], 1);
    let youtube = Path::new(repo).join("profiles/alice/youtube");
    for file in [
        "history.json",
        "videos.json",
        "playlists.json",
        "subscriptions.json",
        "library.json",
        "runs.json",
    ] {
        assert!(youtube.join(file).is_file(), "{file}");
    }
    let videos: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(youtube.join("videos.json")).expect("videos.json"),
    )
    .expect("json");
    assert_eq!(videos["abcDEF12345"]["kind"], "song");
    assert_eq!(videos["-bcDEF12345"]["kind"], "unknown");

    // `youtube normalize` re-derives the same files.
    let before = std::fs::read(youtube.join("history.json")).expect("history.json");
    std::fs::remove_file(youtube.join("videos.json")).expect("rm");
    let out = sss(tmp.path())
        .args(["--repo", repo, "--profile", "alice", "youtube", "normalize"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("normalized run ") && stdout.contains("1 history records, 1 plays"),
        "{stdout}"
    );
    assert!(youtube.join("videos.json").is_file());
    assert_eq!(
        std::fs::read(youtube.join("history.json")).expect("history.json"),
        before
    );

    // `status` shows the import ledger and no Pandora line.
    let out = sss(tmp.path())
        .args(["--repo", repo, "status"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(": complete, 5 files, finished "),
        "{stdout}"
    );
    assert!(
        stdout.contains("  youtube videos: 2 (0 enriched, 1 unknown); no likes in this archive\n"),
        "{stdout}"
    );
    assert!(!stdout.contains("pandora"), "{stdout}");
}

#[test]
fn youtube_normalize_without_an_import_fails() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    let out = sss(tmp.path())
        .args(["--repo", repo, "--profile", "alice", "youtube", "normalize"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "error: no complete import run to normalize; run `sss youtube import DIR`\n"
    );
}

#[test]
fn youtube_import_refuses_html_history_and_an_empty_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    let html = tmp.path().join("html");
    lay_out(&html, &[("history/watch-history.html", "<html></html>")]);
    let empty = tmp.path().join("empty");
    std::fs::create_dir_all(&empty).expect("mkdir");
    for (dir, says) in [
        (&html, "history in HTML"),
        (&empty, "no YouTube Takeout files"),
    ] {
        let out = sss(tmp.path())
            .args([
                "--repo",
                repo,
                "--profile",
                "alice",
                "--json",
                "youtube",
                "import",
            ])
            .arg(dir)
            .assert()
            .code(1)
            .get_output()
            .clone();
        let doc: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("one JSON document");
        assert_eq!(doc["kind"], "input");
        assert!(
            doc["error"].as_str().expect("error").contains(says),
            "{doc}"
        );
    }
    assert!(!Path::new(repo).join("profiles/alice/youtube").exists());
}

#[test]
fn youtube_import_without_history_is_incomplete_and_kept() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    let archive = tmp.path().join("takeout");
    lay_out(
        &archive,
        &[("subscriptions/subscriptions.csv", "Channel Id\n")],
    );
    let out = sss(tmp.path())
        .args([
            "--repo",
            repo,
            "--profile",
            "alice",
            "--json",
            "youtube",
            "import",
        ])
        .arg(&archive)
        .assert()
        .code(1)
        .get_output()
        .clone();
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON document");
    assert_eq!(doc["kind"], "incomplete");
    assert_eq!(doc["details"]["failed_steps"], serde_json::json!([]));
    assert_eq!(
        doc["details"]["manifest"]["steps"]["history"]["status"],
        "missing"
    );
    let run_dir = PathBuf::from(doc["details"]["run_dir"].as_str().expect("run_dir"));
    assert!(run_dir.join("manifest.json").is_file());
}

#[test]
fn youtube_import_with_an_unparsable_history_names_the_failed_step() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    let archive = tmp.path().join("takeout");
    lay_out(&archive, &[("history/watch-history.json", "not json")]);
    let out = sss(tmp.path())
        .args([
            "--repo",
            repo,
            "--profile",
            "alice",
            "--json",
            "youtube",
            "import",
        ])
        .arg(&archive)
        .assert()
        .code(1)
        .get_output()
        .clone();
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON document");
    assert_eq!(doc["kind"], "incomplete");
    assert_eq!(
        doc["details"]["failed_steps"],
        serde_json::json!(["history"])
    );
    let message = doc["error"].as_str().expect("error message");
    assert!(
        message.ends_with("is incomplete: failed steps history; its copies are kept"),
        "{message}"
    );
}

#[test]
fn match_on_a_youtube_only_profile_scopes_and_filters_by_source() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    std::fs::write(
        Path::new(repo).join("config.toml"),
        "[spotify]\nclient_id = \"test-client-id\"\n",
    )
    .expect("write config");
    // A Topic song, a removed video played in the Music app, and a playlist
    // holding a video seen nowhere else.
    let archive = tmp.path().join("takeout");
    let base = "Takeout/YouTube and YouTube Music";
    let history = r#"[{"header": "YouTube Music", "title": "Watched Calm Waters",
        "titleUrl": "https://music.youtube.com/watch?v=abcDEF12345",
        "subtitles": [{"name": "The Band - Topic",
                       "url": "https://www.youtube.com/channel/UCcccccccccccccccccccccc"}],
        "time": "2026-01-02T03:04:05.678Z", "products": ["YouTube"]},
        {"header": "YouTube Music",
         "title": "Watched https://music.youtube.com/watch?v=goneGONE123",
         "titleUrl": "https://music.youtube.com/watch?v=goneGONE123",
         "time": "2026-01-01T03:04:05.678Z", "products": ["YouTube"]}]"#;
    lay_out(
        &archive,
        &[
            (&format!("{base}/history/watch-history.json"), history),
            (
                &format!("{base}/playlists/playlists.csv"),
                "Playlist ID,Playlist Title (Original)\n\
                 PLaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa,Road Trip\n",
            ),
            (
                &format!("{base}/playlists/Road Trip-videos.csv"),
                "Video ID,Playlist Video Creation Timestamp\n\
                 xyzXYZ12345,2025-01-02T00:00:00+00:00\n",
            ),
        ],
    );
    sss(tmp.path())
        .args(["--repo", repo, "--profile", "alice", "youtube", "import"])
        .arg(&archive)
        .assert()
        .code(0);
    let state_dir = tmp.path().join("streaming-service-sync/alice");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    std::fs::write(
        state_dir.join("spotify-token.json"),
        serde_json::json!({"stored_at": "2026-10-15T00:00:00Z", "token": {
            "access_token": "access-secret", "refresh_token": "refresh-secret",
            "expires_at": "2099-01-01T00:00:00Z", "scopes": ["user-library-read"]}})
        .to_string(),
    )
    .expect("write token");
    let search = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/search");
        then.status(200)
            .json_body(search_body(&[("spotify:track:calm", "Calm Waters", None)]));
    });
    let match_cmd = |extra: &[&str]| {
        let mut cmd = sss(tmp.path());
        cmd.env("SSS_SPOTIFY_API_BASE_URL", server.url("/v1"))
            .args([
                "--repo",
                repo,
                "--profile",
                "alice",
                "match",
                "--no-llm",
                "--json",
            ])
            .args(extra);
        cmd
    };

    // No Pandora export: the Pandora half has nothing to decide.
    let doc = json_document(
        &match_cmd(&["--source", "pandora"])
            .assert()
            .code(0)
            .get_output()
            .stdout,
    );
    assert_eq!(doc["kinds"]["tracks"]["processed"], 0);
    search.assert_calls(0);

    let doc = json_document(
        &match_cmd(&["--source", "youtube"])
            .assert()
            .code(0)
            .get_output()
            .stdout,
    );
    let tracks = &doc["kinds"]["tracks"];
    assert_eq!(tracks["processed"], 2, "the playlist-only video is no item");
    assert_eq!(
        tracks["by_status"]["ambiguous"], 1,
        "an exact title without a duration"
    );
    assert_eq!(
        tracks["unmatched_ids"],
        serde_json::json!(["YV:goneGONE123"])
    );
    let entries =
        std::fs::read_to_string(Path::new(repo).join("profiles/alice/matches/tracks.jsonl"))
            .expect("match file");
    assert!(
        entries.contains(r#""item_id":"YV:abcDEF12345""#),
        "{entries}"
    );
    assert!(entries.contains(r#""reason":"no title""#), "{entries}");
    assert!(!entries.contains("xyzXYZ12345"), "{entries}");

    // With include_unknown the playlist-only video is an item, titleless.
    std::fs::write(
        Path::new(repo).join("config.toml"),
        "[spotify]\nclient_id = \"test-client-id\"\n[youtube]\ninclude_unknown = true\n",
    )
    .expect("write config");
    let doc = json_document(&match_cmd(&[]).assert().code(0).get_output().stdout);
    assert_eq!(
        doc["kinds"]["tracks"]["unmatched_ids"],
        serde_json::json!(["YV:xyzXYZ12345"])
    );

    for args in [
        &[
            "set",
            "YV:abcDEF12345",
            "spotify:track:4uLU6hMCjMI75M1A2tKUQC",
        ][..],
        &["reject", "YC:UCcccccccccccccccccccccc"][..],
    ] {
        sss(tmp.path())
            .args(["--repo", repo, "--profile", "alice", "match"])
            .args(args)
            .assert()
            .code(0);
    }
}

#[test]
fn a_skip_playlists_entry_of_the_wrong_shape_is_kind_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    std::fs::write(
        Path::new(repo).join("config.toml"),
        "[youtube]\nskip_playlists = [\"Road Trip\"]\n",
    )
    .expect("write config");
    let out = sss(tmp.path())
        .args(["--repo", repo, "--profile", "alice", "--json", "status"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let doc = json_document(&out.stdout);
    assert_eq!(doc["kind"], "config");
    assert!(
        doc["error"]
            .as_str()
            .is_some_and(|m| m.contains("skip_playlists")),
        "{doc}"
    );
}

#[test]
fn push_on_a_mixed_profile_plans_youtube_parts_and_grows_most_played() {
    const POP: &str = "spotify:track:0000000000000000000pop";
    const DEEP: &str = "spotify:track:000000000000000000deep";
    const PANDORA: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";
    const ROAD_TRIP: &str = "PLaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    let config = |most_played: u32| {
        std::fs::write(
            Path::new(repo).join("config.toml"),
            format!(
                "[spotify]\nclient_id = \"test-client-id\"\n\
                 [youtube]\nmost_played = {most_played}\n"
            ),
        )
        .expect("write config");
    };
    config(1);
    write_export(repo, &[("TR:1", "First", true)]);
    // "Pop Song" played twice, "Deep Cut" once, both from a Topic channel;
    // a pushed playlist holding both, Watch later holding one and the liked
    // list the other.
    let archive = tmp.path().join("takeout");
    let base = "Takeout/YouTube and YouTube Music";
    let play = |id: &str, title: &str, time: &str| {
        format!(
            r#"{{"header": "YouTube Music", "title": "Watched {title}",
               "titleUrl": "https://music.youtube.com/watch?v={id}",
               "subtitles": [{{"name": "The Band - Topic",
                  "url": "https://www.youtube.com/channel/UCcccccccccccccccccccccc"}}],
               "time": "{time}", "products": ["YouTube"]}}"#
        )
    };
    let history = format!(
        "[{}, {}, {}]",
        play("popPOP12345", "Pop Song", "2026-01-03T00:00:00.000Z"),
        play("popPOP12345", "Pop Song", "2026-01-02T00:00:00.000Z"),
        play("deepDEEP123", "Deep Cut", "2026-01-01T00:00:00.000Z"),
    );
    lay_out(
        &archive,
        &[
            (&format!("{base}/history/watch-history.json"), &history),
            (
                &format!("{base}/playlists/playlists.csv"),
                &format!(
                    "Playlist ID,Playlist Title (Original)\n\
                     {ROAD_TRIP},Road Trip\n\
                     PLwwwwwwwwwwwwwwwwwwwwwwwwwwwwwwww,Watch later\n\
                     PLllllllllllllllllllllllllllllllll,Liked videos\n"
                ),
            ),
            (
                &format!("{base}/playlists/Road Trip-videos.csv"),
                "Video ID,Playlist Video Creation Timestamp\n\
                 deepDEEP123,2025-01-02T00:00:00+00:00\n\
                 popPOP12345,2025-01-03T00:00:00+00:00\n",
            ),
            (
                &format!("{base}/playlists/Liked videos-videos.csv"),
                "Video ID,Playlist Video Creation Timestamp\n\
                 deepDEEP123,2025-01-02T00:00:00+00:00\n",
            ),
            (
                &format!("{base}/playlists/Watch later-videos.csv"),
                "Video ID,Playlist Video Creation Timestamp\n\
                 popPOP12345,2025-01-02T00:00:00+00:00\n",
            ),
        ],
    );
    sss(tmp.path())
        .args(["--repo", repo, "--profile", "alice", "youtube", "import"])
        .arg(&archive)
        .assert()
        .code(0);
    for (id, uri) in [
        ("YV:popPOP12345", POP),
        ("YV:deepDEEP123", DEEP),
        ("TR:1", PANDORA),
    ] {
        sss(tmp.path())
            .args([
                "--repo",
                repo,
                "--profile",
                "alice",
                "match",
                "set",
                id,
                uri,
            ])
            .assert()
            .code(0);
    }
    let state_dir = tmp.path().join("streaming-service-sync/alice");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    std::fs::write(
        state_dir.join("spotify-token.json"),
        serde_json::json!({"stored_at": "2026-10-15T00:00:00Z", "token": {
            "access_token": "access-secret", "refresh_token": "refresh-secret",
            "expires_at": "2099-01-01T00:00:00Z",
            "scopes": ["playlist-modify-private", "user-library-modify", "user-library-read",
                "user-follow-read", "playlist-read-private", "playlist-read-collaborative"]}})
        .to_string(),
    )
    .expect("write token");
    let push = |extra: &[&str]| {
        let out = sss(tmp.path())
            .env("SSS_SPOTIFY_API_BASE_URL", server.url("/v1"))
            .args(["--repo", repo, "--profile", "alice", "push", "--json"])
            .args(extra)
            .assert()
            .code(0)
            .get_output()
            .clone();
        json_document(&out.stdout)
    };
    let keys = |part: &serde_json::Value| -> Vec<String> {
        part["playlists"]
            .as_array()
            .expect("playlists")
            .iter()
            .map(|p| p["key"].as_str().expect("key").to_owned())
            .collect()
    };

    let doc = push(&["--dry-run"]);
    let parts = &doc["parts"];
    assert_eq!(keys(&parts["stations"]), ["ST:1"]);
    assert_eq!(
        keys(&parts["playlists"]),
        [format!("YP:{ROAD_TRIP}")],
        "neither Watch later nor the liked list is a playlist"
    );
    assert_eq!(parts["playlists"]["playlists"][0]["name"], "Road Trip");
    assert_eq!(parts["playlists"]["playlists"][0]["to_add"], 2);
    assert_eq!(keys(&parts["most_played"]), ["youtube:most-played"]);
    assert_eq!(parts["most_played"]["playlists"][0]["to_add"], 1);
    assert_eq!(parts["collection"]["to_save"], 0);
    assert_eq!(
        parts["liked"]["to_save"], 2,
        "the Pandora thumbs-up and the liked video"
    );

    let doc = push(&["--dry-run", "--source", "youtube"]);
    assert!(keys(&doc["parts"]["stations"]).is_empty());
    assert_eq!(keys(&doc["parts"]["most_played"]), ["youtube:most-played"]);
    let doc = push(&["--dry-run", "--source", "pandora"]);
    assert!(keys(&doc["parts"]["playlists"]).is_empty());
    assert!(keys(&doc["parts"]["most_played"]).is_empty());

    let create = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/me/playlists")
            .json_body_includes(
                serde_json::json!({"name": "YouTube Music: most played"}).to_string(),
            );
        then.status(201)
            .json_body(serde_json::json!({"id": "top", "snapshot_id": "s0"}));
    });
    let add_pop = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/playlists/top/items")
            .json_body(serde_json::json!({ "uris": [POP] }));
        then.status(201)
            .json_body(serde_json::json!({"snapshot_id": "s1"}));
    });
    let only_most_played = ["--only", "most-played"];
    let doc = push(&only_most_played);
    let made = &doc["parts"]["most_played"]["playlists"][0];
    assert_eq!(
        (&made["action"], &made["added"]),
        (&"create".into(), &1.into())
    );
    assert!(doc["parts"]["playlists"].is_null(), "only the one part ran");
    create.assert_calls(1);
    add_pop.assert_calls(1);

    let mut held = server.mock(|when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/playlists/top/items");
        then.status(200)
            .json_body(serde_json::json!({"items": [{"item": {"uri": POP}}], "next": null}));
    });
    let on_account = on_account_mock(&server, "top");
    let doc = push(&only_most_played);
    assert_eq!(
        doc["parts"]["most_played"]["playlists"][0]["action"],
        "skip"
    );
    add_pop.assert_calls(1);

    // A larger top N appends the new entrant and removes nothing.
    config(2);
    let add_deep = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/playlists/top/items")
            .json_body(serde_json::json!({ "uris": [DEEP] }));
        then.status(201)
            .json_body(serde_json::json!({"snapshot_id": "s2"}));
    });
    let remove = server.mock(|when, then| {
        when.method(httpmock::Method::DELETE);
        then.status(200);
    });
    let doc = push(&only_most_played);
    let grown = &doc["parts"]["most_played"]["playlists"][0];
    assert_eq!(
        (&grown["action"], &grown["added"]),
        (&"update".into(), &1.into())
    );
    add_deep.assert_calls(1);
    remove.assert_calls(0);
    create.assert_calls(1);
    held.assert_calls(2);
    on_account.assert_calls(2);
    held.delete();

    // Off: the part is reported off in [youtube] and plans nothing.
    config(0);
    let out = sss(tmp.path())
        .args([
            "--repo",
            repo,
            "--profile",
            "alice",
            "push",
            "--dry-run",
            "--only",
            "most-played",
        ])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("most-played: off in config.toml [youtube]"),
        "{text}"
    );
}

#[test]
fn report_on_youtube_only_then_mixed_profiles_shows_the_youtube_summary() {
    const SKIPPED: &str = "PLssssssssssssssssssssssssssssssss";
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    std::fs::write(
        Path::new(repo).join("config.toml"),
        format!("[youtube]\nskip_playlists = [\"{SKIPPED}\"]\n"),
    )
    .expect("write config");
    // A Topic song played in January and again in April (a gap of more
    // than 30 days), a removed video, a video of no known kind, and three
    // playlists: one holding a video seen nowhere else, one skipped by id,
    // and two whose titles share one items file.
    let archive = tmp.path().join("takeout");
    let base = "Takeout/YouTube and YouTube Music";
    let history = r#"[
        {"header": "YouTube Music", "title": "Watched Calm Waters",
         "titleUrl": "https://music.youtube.com/watch?v=abcDEF12345",
         "subtitles": [{"name": "The Band - Topic",
                        "url": "https://www.youtube.com/channel/UCcccccccccccccccccccccc"}],
         "time": "2026-04-02T03:04:05.678Z", "products": ["YouTube"]},
        {"header": "YouTube", "title": "Watched Painting a Lake",
         "titleUrl": "https://www.youtube.com/watch?v=paintPAINT1",
         "subtitles": [{"name": "Paint Club",
                        "url": "https://www.youtube.com/channel/UCpppppppppppppppppppppp"}],
         "time": "2026-01-03T03:04:05.678Z", "products": ["YouTube"]},
        {"header": "YouTube Music",
         "title": "Watched https://music.youtube.com/watch?v=goneGONE123",
         "titleUrl": "https://music.youtube.com/watch?v=goneGONE123",
         "time": "2026-01-02T03:04:05.678Z", "products": ["YouTube"]},
        {"header": "YouTube Music", "title": "Watched Calm Waters",
         "titleUrl": "https://music.youtube.com/watch?v=abcDEF12345",
         "subtitles": [{"name": "The Band - Topic",
                        "url": "https://www.youtube.com/channel/UCcccccccccccccccccccccc"}],
         "time": "2026-01-01T03:04:05.678Z", "products": ["YouTube"]}]"#;
    let items = "Video ID,Playlist Video Creation Timestamp\n\
                 xyzXYZ12345,2025-01-02T00:00:00+00:00\n\
                 abcDEF12345,2025-01-03T00:00:00+00:00\n";
    lay_out(
        &archive,
        &[
            (&format!("{base}/history/watch-history.json"), history),
            (
                &format!("{base}/playlists/playlists.csv"),
                &format!(
                    "Playlist ID,Playlist Title (Original)\n\
                     PLaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa,Road Trip\n\
                     {SKIPPED},Converted\n\
                     PLmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm,Mix: 2024\n\
                     PLnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn,Mix/ 2024\n"
                ),
            ),
            (&format!("{base}/playlists/Road Trip-videos.csv"), items),
            (&format!("{base}/playlists/Converted-videos.csv"), items),
            (&format!("{base}/playlists/Mix_ 2024-videos.csv"), items),
        ],
    );
    sss(tmp.path())
        .args(["--repo", repo, "--profile", "alice", "youtube", "import"])
        .arg(&archive)
        .assert()
        .code(0);
    let report = || {
        sss(tmp.path())
            .args(["--repo", repo, "--profile", "alice", "report"])
            .assert()
            .code(0);
        std::fs::read_to_string(Path::new(repo).join("profiles/alice/report.md"))
            .expect("report.md")
    };
    // The removed video, in the top N, as `match` leaves it.
    let matches = Path::new(repo).join("profiles/alice/matches");
    std::fs::create_dir_all(&matches).expect("matches dir");
    std::fs::write(
        matches.join("tracks.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({"item_id": "YV:goneGONE123", "status": "unmatched",
                "reason": "no title", "decided_at": "2026-10-15T00:00:00Z"})
        ),
    )
    .expect("write matches");
    let doc = report();
    assert!(!doc.contains("### Pandora"), "{doc}");
    for expected in [
        "### YouTube\n\n- Latest complete import run: `",
        "- History: 4 records, 4 plays from 2026-01-01 to 2026-04-02\n",
        "  - no plays from 2026-01-03 to 2026-04-02\n",
        "- Plays by app: YouTube Music 3, YouTube 1\n",
        "- 1 removed videos, 1 plays, titles withheld by Takeout\n",
        "- Liked: no likes in this archive\n",
        "#### Not classified\n",
        "| Paint Club | 1 | 1 |\n",
        "#### Typed music by the Music app alone\n",
        "1 removed videos are typed this way too",
        "- Road Trip `PLaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa`: 2 videos, 1 music, 1 with no \
         evidence\n",
        "- Mix: 2024 `PLmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm`: 0 videos, 0 music, 0 with no \
         evidence; items: ambiguous\\_title",
        "## YouTube playlists not pushed\n",
        "- Converted `PLssssssssssssssssssssssssssssssss`: in `[youtube] skip_playlists`\n",
        "- Mix/ 2024 `PLnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn`: its items file is shared",
        "- Road Trip `PLaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa`: no matched music\n",
        "[Watch on YouTube](https://www.youtube.com/watch?v=goneGONE123)",
    ] {
        assert!(doc.contains(expected), "{expected}\n---\n{doc}");
    }

    write_export(repo, &[("TR:1", "First", true)]);
    let doc = report();
    assert!(doc.contains("### Pandora\n"), "{doc}");
    assert!(doc.contains("### YouTube\n"), "{doc}");
    assert!(doc.contains("## Pandora playlists not pushed\n"), "{doc}");
    assert!(doc.contains("## YouTube playlists not pushed\n"), "{doc}");
}

/// Every file under `dir`, read as text, by path.
fn texts_under(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut paths = walk(dir);
    paths.sort();
    paths
        .into_iter()
        .map(|p| {
            let text = std::fs::read_to_string(&p).expect("read");
            (p, text)
        })
        .collect()
}

#[test]
fn youtube_api_key_is_verified_before_it_is_stored_and_never_shown() {
    use httpmock::prelude::*;
    const KEY: &str = "AIza-test-key-0001";
    let server = MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    let refused = server.mock(|when, then| {
        when.method(GET)
            .path("/youtube/v3/videos")
            .query_param("key", "AIza-bad");
        then.status(400).json_body(serde_json::json!({"error": {
            "code": 400, "message": "API key not valid. Please pass a valid API key.",
            "errors": [{"reason": "keyInvalid"}]}}));
    });
    let accepted = server.mock(|when, then| {
        when.method(GET)
            .path("/youtube/v3/videos")
            .query_param("key", KEY)
            .query_param("id", "dQw4w9WgXcQ")
            .query_param("part", "id");
        then.status(200)
            .json_body(serde_json::json!({"items": [{"id": "dQw4w9WgXcQ"}]}));
    });
    let api_key = |stdin: &str| {
        sss(tmp.path())
            .env("SSS_YOUTUBE_API_BASE_URL", server.url("/youtube/v3"))
            .args(["--repo", repo, "--json", "youtube", "api-key"])
            .write_stdin(stdin.to_owned())
            .assert()
            .get_output()
            .clone()
    };
    let key_file = tmp
        .path()
        .join("streaming-service-sync/youtube-api-key.json");

    let out = api_key("AIza-bad\n\n");
    assert_eq!(out.status.code(), Some(1));
    let doc = json_document(&out.stdout);
    assert_eq!(doc["kind"], "input");
    refused.assert_calls(1);
    assert!(!key_file.exists());

    let out = api_key(&format!("{KEY}\n\n"));
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    accepted.assert_calls(1);
    let doc = json_document(&out.stdout);
    assert_eq!(doc["command"], "youtube api-key");
    assert_eq!(doc["key_path"], key_file.to_str().expect("utf-8"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stdout.contains(KEY) && !stderr.contains(KEY), "{out:?}");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&key_file)
        .expect("key file")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(
        std::fs::read_to_string(&key_file)
            .expect("read")
            .contains(KEY)
    );

    // `status` says when it was stored, never the key; no profile appears.
    let out = sss(tmp.path())
        .args(["--repo", repo, "--json", "status"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let doc = json_document(&out.stdout);
    assert_eq!(doc["profiles"], serde_json::json!([]));
    assert!(doc["youtube_api_key"]["stored_at"].is_string(), "{doc}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains(KEY));
}

#[test]
fn youtube_enrich_fetches_in_fifties_stops_on_quota_and_resumes() {
    use httpmock::prelude::*;
    const KEY: &str = "AIza-test-key-0002";
    let server = MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("data");
    let repo = repo.to_str().expect("utf-8 path");
    sss(tmp.path())
        .args(["--repo", repo, "init"])
        .assert()
        .code(0);
    std::fs::write(
        Path::new(repo).join("config.toml"),
        "[spotify]\nclient_id = \"test-client-id\"\n[youtube]\nmost_played = 1\n",
    )
    .expect("write config");
    // 51 videos played once in the Music app with no channel; the first
    // played twice, so it alone is the top 1.
    let id = |n: usize| format!("v{n:010}");
    let mut plays: Vec<serde_json::Value> = (0..51)
        .map(|n| {
            serde_json::json!({"header": "YouTube Music", "title": format!("Watched Track {n}"),
                "titleUrl": format!("https://music.youtube.com/watch?v={}", id(n)),
                "time": format!("2026-01-01T00:{:02}:{:02}.000Z", n / 60, n % 60),
                "products": ["YouTube"]})
        })
        .collect();
    plays.push(
        serde_json::json!({"header": "YouTube Music", "title": "Watched Track 1",
        "titleUrl": format!("https://music.youtube.com/watch?v={}", id(1)),
        "time": "2026-02-01T00:00:00.000Z", "products": ["YouTube"]}),
    );
    let archive = tmp.path().join("takeout");
    lay_out(
        &archive,
        &[(
            "Takeout/YouTube and YouTube Music/history/watch-history.json",
            &serde_json::Value::Array(plays).to_string(),
        )],
    );
    sss(tmp.path())
        .args(["--repo", repo, "--profile", "alice", "youtube", "import"])
        .arg(&archive)
        .assert()
        .code(0);

    let enrich = || {
        sss(tmp.path())
            .env("SSS_YOUTUBE_API_BASE_URL", server.url("/youtube/v3"))
            .args([
                "--repo",
                repo,
                "--profile",
                "alice",
                "--json",
                "youtube",
                "enrich",
            ])
            .assert()
            .get_output()
            .clone()
    };
    let out = enrich();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(json_document(&out.stdout)["kind"], "auth");

    let key_file = tmp
        .path()
        .join("streaming-service-sync/youtube-api-key.json");
    std::fs::create_dir_all(key_file.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &key_file,
        serde_json::json!({"key": KEY, "stored_at": "2026-10-17T00:00:00Z"}).to_string(),
    )
    .expect("write key");

    // The first fifty: all but video 3 found, as Topic songs of 3:20.
    let first: Vec<String> = (0..50).map(id).collect();
    let items: Vec<serde_json::Value> = (0..50)
        .filter(|&n| n != 3)
        .map(|n| {
            serde_json::json!({"id": id(n),
                "snippet": {"title": format!("Track {n}"), "channelId": "UCbbbbbbbbbbbbbbbbbbbbbb",
                            "channelTitle": "The Band - Topic", "categoryId": "10"},
                "contentDetails": {"duration": "PT3M20S"}})
        })
        .collect();
    let first_chunk = server.mock(|when, then| {
        when.method(GET)
            .path("/youtube/v3/videos")
            .query_param("key", KEY)
            .query_param("part", "snippet,contentDetails,topicDetails")
            .query_param("id", first.join(","));
        then.status(200)
            .json_body(serde_json::json!({ "items": items }));
    });
    let mut quota = server.mock(|when, then| {
        when.method(GET)
            .path("/youtube/v3/videos")
            .query_param("id", id(50));
        then.status(403).json_body(serde_json::json!({"error": {
            "code": 403, "message": "quota", "errors": [{"reason": "quotaExceeded"}]}}));
    });

    let out = enrich();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(json_document(&out.stdout)["kind"], "quota");
    first_chunk.assert_calls(1);
    quota.assert_calls(1);
    let youtube = Path::new(repo).join("profiles/alice/youtube");
    let cached = texts_under(&youtube.join("api"));
    let names: Vec<_> = cached.iter().map(|(p, _)| p.file_name()).collect();
    assert_eq!(
        names,
        [
            Some("0001-videos-list.json".as_ref()),
            Some("0002-videos-list.json".as_ref())
        ],
        "the answer and the refusal"
    );
    assert!(
        cached[1].1.contains("\"status\": 403"),
        "the refusal is kept"
    );
    assert!(cached.iter().all(|(_, t)| !t.contains(KEY)));
    // What was fetched before the stop is normalized.
    let videos: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(youtube.join("videos.json")).expect("videos.json"),
    )
    .expect("json");
    assert_eq!(videos[id(1)]["duration_s"], 200);
    assert_eq!(videos[id(1)]["kind"], "song");
    assert_eq!(videos[id(3)]["available"], false);
    assert_eq!(videos[id(50)]["available"], serde_json::Value::Null);

    // The next day: only the rest is asked for.
    quota.delete();
    let rest = server.mock(|when, then| {
        when.method(GET)
            .path("/youtube/v3/videos")
            .query_param("key", KEY)
            .query_param("id", id(50));
        then.status(200)
            .json_body(serde_json::json!({"items": [{"id": id(50),
            "snippet": {"title": "Track 50", "channelTitle": "Someone", "categoryId": "22"}}]}));
    });
    let out = enrich();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let doc = json_document(&out.stdout);
    assert_eq!(
        (
            &doc["requested"],
            &doc["found"],
            &doc["unavailable"],
            &doc["remaining"]
        ),
        (
            &serde_json::json!(1),
            &serde_json::json!(1),
            &serde_json::json!(0),
            &serde_json::json!(0)
        )
    );
    assert_eq!(doc["normalized"]["videos"], 51);
    first_chunk.assert_calls(1);
    rest.assert_calls(1);
    for (path, text) in texts_under(Path::new(repo)) {
        assert!(!text.contains(KEY), "{}", path.display());
    }
    let status = sss(tmp.path())
        .args(["--repo", repo, "status"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("youtube videos: 51 (51 enriched"),
        "{stdout}"
    );

    // With an API duration the Topic song is matched outright.
    let spotify = server.mock(|when, then| {
        when.method(GET).path("/v1/search");
        then.status(200)
            .json_body(search_body(&[("spotify:track:track1", "Track 1", None)]));
    });
    let state_dir = tmp.path().join("streaming-service-sync/alice");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    std::fs::write(
        state_dir.join("spotify-token.json"),
        serde_json::json!({"stored_at": "2026-10-15T00:00:00Z", "token": {
            "access_token": "access-secret", "refresh_token": "refresh-secret",
            "expires_at": "2099-01-01T00:00:00Z", "scopes": ["user-library-read"]}})
        .to_string(),
    )
    .expect("write token");
    let out = sss(tmp.path())
        .env("SSS_SPOTIFY_API_BASE_URL", server.url("/v1"))
        .args([
            "--repo",
            repo,
            "--profile",
            "alice",
            "--json",
            "match",
            "--no-llm",
            "--source",
            "youtube",
        ])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let doc = json_document(&out.stdout);
    assert_eq!(doc["kinds"]["tracks"]["processed"], 1, "{doc}");
    assert_eq!(doc["kinds"]["tracks"]["by_method"]["exact"], 1, "{doc}");
    assert_eq!(doc["kinds"]["tracks"]["by_status"]["matched"], 1, "{doc}");
    assert!(spotify.calls() >= 1);
}

/// The playlist the playlist-write tests change.
const KIDS: &str = "37i9dQZF1DXcBWIGoYBM5M";

/// `GET me` answers Alice's account.
fn me_is_alice(server: &httpmock::MockServer) -> httpmock::Mock<'_> {
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/me");
        then.status(200)
            .json_body(serde_json::json!({"id": "alice-spotify", "display_name": "Alice"}));
    })
}

/// Alice's private "Kids Songs" holding `total` items, as the details read
/// and the listing give it.
fn kids_songs(total: u32) -> serde_json::Value {
    serde_json::json!({"uri": format!("spotify:playlist:{KIDS}"), "name": "Kids Songs",
        "description": "", "public": false, "collaborative": false, "snapshot_id": "k0",
        "owner": {"id": "alice-spotify", "display_name": "Alice"},
        "items": {"href": "https://api.spotify.com/v1/x", "total": total}})
}

/// The details read and the items read of "Kids Songs" holding `uris`.
fn kids_songs_holding<'a>(
    server: &'a httpmock::MockServer,
    uris: &[&str],
) -> (httpmock::Mock<'a>, httpmock::Mock<'a>) {
    let total = u32::try_from(uris.len()).expect("small");
    let details = server.mock(move |when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/playlists/{KIDS}"));
        then.status(200).json_body(kids_songs(total));
    });
    let items: Vec<serde_json::Value> = uris
        .iter()
        .map(|u| serde_json::json!({"item": {"type": "track", "uri": u, "name": "Song"}}))
        .collect();
    let contents = server.mock(move |when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/v1/playlists/{KIDS}/items"));
        then.status(200)
            .json_body(serde_json::json!({"items": items, "next": null}));
    });
    (details, contents)
}

#[test]
fn playlist_create_then_add_by_name_from_a_file() {
    const TRACK_C: &str = "spotify:track:6rqhFgbbKwnb9MLmUQDhG6";
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_spotify_token(tmp.path(), ALL_SCOPES);
    let me = me_is_alice(&server);
    let mut empty = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/me/playlists");
        then.status(200)
            .json_body(serde_json::json!({"items": [], "next": null}));
    });
    let create = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/me/playlists")
            .json_body(serde_json::json!({"name": "Kids Songs", "public": false,
                "description": ""}));
        then.status(201)
            .json_body(serde_json::json!({"id": KIDS, "snapshot_id": "k0"}));
    });
    let (code, doc) = spotify_json(
        tmp.path(),
        &server,
        &repo,
        &["playlist", "create", "Kids Songs"],
    );
    assert_eq!(code, 0, "{doc}");
    me.assert_calls(1);
    empty.assert_calls(1);
    create.assert_calls(1);
    assert_eq!(doc["created"]["id"], format!("spotify:playlist:{KIDS}"));
    empty.delete();

    let listing = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/me/playlists");
        then.status(200)
            .json_body(serde_json::json!({"items": [kids_songs(1)], "next": null}));
    });
    let (details, contents) = kids_songs_holding(&server, &[TRACK_A]);
    let add = server.mock(|when, then| {
        when.method(httpmock::Method::POST)
            .path(format!("/v1/playlists/{KIDS}/items"))
            .json_body(serde_json::json!({"uris": [TRACK_B, TRACK_C]}));
        then.status(201)
            .json_body(serde_json::json!({"snapshot_id": "k1"}));
    });
    let list = tmp.path().join("uris.txt");
    std::fs::write(
        &list,
        format!("# kids\n{TRACK_A}\n{TRACK_B}\n\n{TRACK_C}\n"),
    )
    .expect("list");
    let (code, doc) = spotify_json(
        tmp.path(),
        &server,
        &repo,
        &[
            "playlist",
            "add",
            "Kids Songs",
            "--from-file",
            list.to_str().expect("utf-8 path"),
        ],
    );
    assert_eq!(code, 0, "{doc}");
    listing.assert_calls(1);
    details.assert_calls(1);
    contents.assert_calls(1);
    add.assert_calls(1);
    assert_eq!(doc["to_add"], serde_json::json!([TRACK_B, TRACK_C]));
    assert_eq!(doc["already_present"], serde_json::json!([TRACK_A]));
}

#[test]
fn a_deleted_pushed_playlist_is_left_alone_by_the_next_push() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_spotify_token(tmp.path(), ALL_SCOPES);
    write_export(&repo, &[("TR:1", "First", true)]);
    sss(tmp.path())
        .args([
            "--repo",
            &repo,
            "--profile",
            "alice",
            "match",
            "set",
            "TR:1",
            TRACK_A,
        ])
        .assert()
        .code(0);
    let ledger = Path::new(&repo).join("profiles/alice/spotify/state.json");
    std::fs::create_dir_all(ledger.parent().expect("parent")).expect("spotify dir");
    std::fs::write(
        &ledger,
        serde_json::json!({"playlists": {"ST:1": {"status": "created",
            "spotify_id": KIDS, "items": [TRACK_A]}}})
        .to_string(),
    )
    .expect("write ledger");
    let before = std::fs::read(&ledger).expect("ledger");
    let uri = format!("spotify:playlist:{KIDS}");

    me_is_alice(&server);
    let (details, contents) = kids_songs_holding(&server, &[TRACK_A]);
    let held_uri = uri.clone();
    let mut held = server.mock(move |when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/me/library/contains")
            .query_param("uris", held_uri);
        then.status(200).json_body(serde_json::json!([true]));
    });
    let removed_uri = uri.clone();
    let remove = server.mock(move |when, then| {
        when.method(httpmock::Method::DELETE)
            .path("/v1/me/library")
            .query_param("uris", removed_uri);
        then.status(200).body("");
    });
    let (code, doc) = spotify_json(tmp.path(), &server, &repo, &["playlist", "delete", &uri]);
    assert_eq!(code, 0, "{doc}");
    held.assert_calls(1);
    remove.assert_calls(1);
    details.assert_calls(1);
    assert_eq!(doc["action"], "deleted");
    assert_eq!(doc["pushed_as"], "ST:1");
    assert_eq!(std::fs::read(&ledger).expect("ledger"), before);
    held.delete();

    let gone_uri = uri.clone();
    let gone = server.mock(move |when, then| {
        when.method(httpmock::Method::GET)
            .path("/v1/me/library/contains")
            .query_param("uris", gone_uri);
        then.status(200).json_body(serde_json::json!([false]));
    });
    let (code, doc) = spotify_json(tmp.path(), &server, &repo, &["push", "--only", "stations"]);
    assert_eq!(code, 0, "{doc}");
    gone.assert_calls(1);
    contents.assert_calls(0);
    let pushed = &doc["parts"]["stations"]["playlists"][0];
    assert_eq!(pushed["key"], "ST:1");
    assert_eq!(pushed["action"], "deleted");
    assert_eq!(std::fs::read(&ledger).expect("ledger"), before);
}

#[test]
fn library_remove_without_the_modify_scope_is_kind_auth_with_no_request() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_spotify_token(tmp.path(), &["user-library-read", "user-follow-read"]);
    let any = server.mock(|_, then| {
        then.status(500);
    });
    let (code, doc) = spotify_json(tmp.path(), &server, &repo, &["library", "remove", TRACK_A]);
    assert_eq!(code, 1, "{doc}");
    assert_eq!(doc["kind"], "auth");
    assert!(
        doc["error"]
            .as_str()
            .expect("error")
            .contains("user-library-modify"),
        "{doc}"
    );
    any.assert_calls(0);
}

#[test]
fn a_playlist_add_that_fails_on_its_second_chunk_reports_what_landed() {
    let server = httpmock::MockServer::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_spotify_token(tmp.path(), ALL_SCOPES);
    let uris: Vec<String> = (0..150)
        .map(|n| format!("spotify:track:{n:0>22}"))
        .collect();
    me_is_alice(&server);
    kids_songs_holding(&server, &[]);
    let first = uris[..100].to_vec();
    let landed = server.mock(move |when, then| {
        when.method(httpmock::Method::POST)
            .path(format!("/v1/playlists/{KIDS}/items"))
            .json_body(serde_json::json!({"uris": first}));
        then.status(201)
            .json_body(serde_json::json!({"snapshot_id": "k1"}));
    });
    let rest = uris[100..].to_vec();
    let failed = server.mock(move |when, then| {
        when.method(httpmock::Method::POST)
            .path(format!("/v1/playlists/{KIDS}/items"))
            .json_body(serde_json::json!({"uris": rest}));
        then.status(503).body("");
    });
    let list = tmp.path().join("uris.txt");
    std::fs::write(&list, uris.join("\n")).expect("list");
    let (code, doc) = spotify_json(
        tmp.path(),
        &server,
        &repo,
        &[
            "playlist",
            "add",
            &format!("spotify:playlist:{KIDS}"),
            "--from-file",
            list.to_str().expect("utf-8 path"),
        ],
    );
    assert_eq!(code, 1, "{doc}");
    landed.assert_calls(1);
    failed.assert_calls(1);
    assert_eq!(doc["kind"], "unavailable");
    assert_eq!(doc["details"]["done"], serde_json::json!(uris[..100]));
}
