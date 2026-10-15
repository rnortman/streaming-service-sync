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
    assert!(
        stderr.starts_with("error: no normalized export in ")
            && stderr.ends_with("; run `sss pandora export`\n"),
        "{stderr}"
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
        "no profiles yet; run `sss pandora auth --profile NAME` first\n"
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
    assert_eq!(doc["items"][0]["pandora_id"], "TR:3");
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
    assert_eq!(last["pandora_id"], "TR:3");
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
fn write_fake_ccc(dir: &Path, name: &str, which_exit: i32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    let d = dir.display();
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{d}/argv'\n\
             printf '%s\\n' \"$CCC_AGENTS_CONFIG\" >> '{d}/env'\n\
             if [ \"$1\" = which ]; then echo 'no definition for agent type' >&2; exit {which_exit}; fi\n\
             n=$(( $(cat '{d}/count' 2>/dev/null || echo 0) + 1 ))\n\
             echo $n > '{d}/count'\n\
             cat \"{d}/out$n\" 2>/dev/null\n\
             exit $(cat \"{d}/exit$n\" 2>/dev/null || echo 0)\n"
        ),
    )
    .expect("write fake ccc");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
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
            .map(|i| i["pandora_id"].as_str().expect("id").to_owned())
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
        "[spotify]\nclient_id = \"test-client-id\"\n",
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
            "scopes": ["playlist-modify-private", "user-library-modify"]}})
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

    let doc = push(&[]);
    assert_eq!(doc["dry_run"], false);
    let pushed = &doc["parts"]["stations"]["playlists"][0];
    assert_eq!(pushed["action"], "create");
    assert_eq!(pushed["added"], 1);
    assert_eq!(pushed["spotify_id"], "pl1");
    create.assert_calls(1);
    add.assert_calls(1);
    items.assert_calls(0);
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
    assert!(
        report.starts_with("# Pandora to Spotify: alice\n"),
        "{report}"
    );
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
