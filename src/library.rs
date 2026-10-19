//! `sss library tracks|albums|artists|add|remove`: the member's saved
//! tracks and albums and followed artists, and the saves, follows,
//! removals and unfollows of tracks, albums, artists and playlists.

use std::borrow::BorrowMut;
use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

use anyhow::{Context as _, Result};
use serde::Serialize;

use crate::repo::{self, ProfilePaths};
use crate::search::result_line;
use crate::spotify::auth::{LIBRARY_READ_SCOPES, LIBRARY_WRITE_SCOPES, PLAYLIST_FOLLOW_SCOPES};
use crate::spotify::client::Spotify;
use crate::spotify::library::Saved;
use crate::targets::{self, ItemKind, Items, LIBRARY_ITEMS, Progress, Resolved};

/// What a library read lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LibraryKind {
    /// The saved tracks.
    Tracks,
    /// The saved albums.
    Albums,
    /// The followed artists.
    Artists,
}

impl LibraryKind {
    /// What the text calls the list when it is empty.
    fn empty_line(self) -> &'static str {
        match self {
            LibraryKind::Tracks => "no saved tracks",
            LibraryKind::Albums => "no saved albums",
            LibraryKind::Artists => "no followed artists",
        }
    }
}

/// What `sss library tracks|albums|artists` reports: the entries in
/// Spotify's order, most recently saved first.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LibraryReport {
    pub profile: String,
    pub kind: LibraryKind,
    pub items: Vec<Saved>,
}

/// `sss library tracks|albums|artists`: every entry of `kind`, or the
/// first `limit`.
pub fn list<S: BorrowMut<Spotify>>(
    profile: &str,
    kind: LibraryKind,
    limit: Option<usize>,
    connect: impl FnOnce() -> Result<S>,
) -> Result<LibraryReport> {
    let mut client = connect()?;
    let c = client.borrow_mut();
    c.require_scopes(profile, LIBRARY_READ_SCOPES)?;
    let items = match kind {
        LibraryKind::Tracks => c.saved_tracks(limit).context("reading the saved tracks")?,
        LibraryKind::Albums => c.saved_albums(limit).context("reading the saved albums")?,
        LibraryKind::Artists => c
            .followed_artists(limit)
            .context("reading the followed artists")?,
    };
    Ok(LibraryReport {
        profile: profile.to_owned(),
        kind,
        items,
    })
}

/// What `sss library add` reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LibraryAddReport {
    pub profile: String,
    pub dry_run: bool,
    /// The URI each source item id given resolved to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<BTreeMap<String, String>>,
    /// Items given more than once, kept at their first occurrence.
    #[serde(skip_serializing_if = "repo::is_zero")]
    pub duplicates_dropped: usize,
    /// What was not yet in the library, in the order given: what the run
    /// saved, or a dry run would.
    pub to_save: Vec<String>,
    pub already_saved: Vec<String>,
}

/// What `sss library remove` reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LibraryRemoveReport {
    pub profile: String,
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "repo::is_zero")]
    pub duplicates_dropped: usize,
    /// What was in the library, in the order given: what the run removed,
    /// or a dry run would.
    pub to_remove: Vec<String>,
    pub not_saved: Vec<String>,
}

/// The direction of a library write.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Save,
    Remove,
}

/// What a library write found and did, before it is shaped as a report.
struct Change {
    resolved: Option<BTreeMap<String, String>>,
    duplicates_dropped: usize,
    /// The URIs the write is for: unsaved ones for a save, saved ones for
    /// a removal.
    change: Vec<String>,
    unchanged: Vec<String>,
}

/// Resolves `items` (before `connect`), checks the scopes, asks which are
/// saved, and unless it is a dry run writes the ones `direction` changes. A
/// failure after a chunk landed is a [`PartialWrite`](crate::error::PartialWrite).
fn change<S: BorrowMut<Spotify>>(
    profile: &str,
    paths: &ProfilePaths,
    items: &Items,
    input: &mut dyn BufRead,
    direction: Direction,
    connect: impl FnOnce() -> Result<S>,
    err: &mut dyn Write,
) -> Result<Change> {
    let given = targets::given(paths, LIBRARY_ITEMS, items, input)?;
    let mut client = connect()?;
    let c = client.borrow_mut();
    c.require_scopes(profile, scopes_for(&given.items))?;
    let held = c
        .library_contains(&given.uris)
        .context("checking the library")?;
    // A save changes what the library does not hold; a removal what it does.
    let wanted = direction == Direction::Remove;
    let mut change = Vec::new();
    let mut unchanged = Vec::new();
    for (uri, held) in given.uris.into_iter().zip(held) {
        if held == wanted {
            change.push(uri);
        } else {
            unchanged.push(uri);
        }
    }
    if !items.dry_run && !change.is_empty() {
        write(c, &change, direction, err)?;
    }
    Ok(Change {
        resolved: given.resolved,
        duplicates_dropped: given.duplicates_dropped,
        change,
        unchanged,
    })
}

/// The scopes a library write of `items` checks: those of a follow or
/// unfollow of a playlist when one is among them.
fn scopes_for(items: &[Resolved]) -> &'static [&'static str] {
    if items.iter().any(|r| r.kind == ItemKind::Playlist) {
        PLAYLIST_FOLLOW_SCOPES
    } else {
        LIBRARY_WRITE_SCOPES
    }
}

/// Saves or removes `uris` in chunks, a progress line on `err` per chunk
/// landed.
fn write(
    c: &mut Spotify,
    uris: &[String],
    direction: Direction,
    err: &mut dyn Write,
) -> Result<()> {
    let what = match direction {
        Direction::Save => "saved",
        Direction::Remove => "removed",
    };
    let mut progress = Progress::new(what, uris.len(), err);
    let mut landed = |chunk: &[String]| -> Result<()> {
        progress.landed(chunk);
        Ok(())
    };
    let result = match direction {
        Direction::Save => c.library_add(uris, &mut landed),
        Direction::Remove => c.library_remove(uris, &mut landed),
    };
    result.map_err(|e| progress.failed(e))
}

/// `sss library add`: saves the tracks and albums and follows the artists
/// and playlists of `items` that the library does not hold yet.
pub fn add<S: BorrowMut<Spotify>>(
    profile: &str,
    paths: &ProfilePaths,
    items: &Items,
    input: &mut dyn BufRead,
    connect: impl FnOnce() -> Result<S>,
    err: &mut dyn Write,
) -> Result<LibraryAddReport> {
    let found = change(profile, paths, items, input, Direction::Save, connect, err)?;
    Ok(LibraryAddReport {
        profile: profile.to_owned(),
        dry_run: items.dry_run,
        resolved: found.resolved,
        duplicates_dropped: found.duplicates_dropped,
        to_save: found.change,
        already_saved: found.unchanged,
    })
}

/// `sss library remove`: removes the tracks and albums and unfollows the
/// artists and playlists of `items` that the library holds.
pub fn remove<S: BorrowMut<Spotify>>(
    profile: &str,
    paths: &ProfilePaths,
    items: &Items,
    input: &mut dyn BufRead,
    connect: impl FnOnce() -> Result<S>,
    err: &mut dyn Write,
) -> Result<LibraryRemoveReport> {
    let found = change(
        profile,
        paths,
        items,
        input,
        Direction::Remove,
        connect,
        err,
    )?;
    Ok(LibraryRemoveReport {
        profile: profile.to_owned(),
        dry_run: items.dry_run,
        resolved: found.resolved,
        duplicates_dropped: found.duplicates_dropped,
        to_remove: found.change,
        not_saved: found.unchanged,
    })
}

impl LibraryReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        if self.items.is_empty() {
            return writeln!(out, "{}", self.kind.empty_line());
        }
        for saved in &self.items {
            match &saved.added_at {
                Some(at) => writeln!(out, "{at} {}", result_line(&saved.item))?,
                None => writeln!(out, "{}", result_line(&saved.item))?,
            }
        }
        Ok(())
    }
}

/// The lines of a library write: the summary, then each URI changed and
/// each source item id's resolution.
fn write_change(
    out: &mut dyn Write,
    summary: String,
    change: &[String],
    resolved: Option<&BTreeMap<String, String>>,
) -> io::Result<()> {
    writeln!(out, "{summary}")?;
    for uri in change {
        writeln!(out, "  {uri}")?;
    }
    targets::write_resolved(out, resolved)
}

impl LibraryAddReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        let verb = if self.dry_run { "would save" } else { "saved" };
        write_change(
            out,
            format!(
                "{verb} {} ({} already saved)",
                self.to_save.len(),
                self.already_saved.len()
            ),
            &self.to_save,
            self.resolved.as_ref(),
        )
    }
}

impl LibraryRemoveReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        let verb = if self.dry_run {
            "would remove"
        } else {
            "removed"
        };
        write_change(
            out,
            format!(
                "{verb} {} ({} not saved)",
                self.to_remove.len(),
                self.not_saved.len()
            ),
            &self.to_remove,
            self.resolved.as_ref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{self, Kind, PartialWrite};
    use crate::matching::model::{self, MatchEntry, Status};
    use crate::repo::Repo;
    use crate::spotify::ids::IdKind;
    use crate::spotify::test_support::client;
    use httpmock::prelude::*;
    use serde_json::json;
    use std::path::Path;

    const TRACK: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";
    const ALBUM: &str = "spotify:album:6akEvsycLGftJxYudPjmqK";
    const PLAYLIST: &str = "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M";

    fn paths(tmp: &Path) -> ProfilePaths {
        Repo::at(tmp.join("data")).profile("alice").unwrap()
    }

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn text(write: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> String {
        let mut out = Vec::new();
        write(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn no_client() -> Result<Spotify> {
        panic!("connected before the items were resolved")
    }

    fn contains<'a>(
        server: &'a MockServer,
        uris: &str,
        answer: serde_json::Value,
    ) -> httpmock::Mock<'a> {
        let uris = uris.to_owned();
        server.mock(move |when, then| {
            when.method(GET)
                .path("/v1/me/library/contains")
                .query_param("uris", uris);
            then.status(200).json_body(answer);
        })
    }

    fn library<'a>(
        server: &'a MockServer,
        method: httpmock::Method,
        uris: &str,
    ) -> httpmock::Mock<'a> {
        let uris = uris.to_owned();
        server.mock(move |when, then| {
            when.method(method)
                .path("/v1/me/library")
                .query_param("uris", uris);
            then.status(200);
        })
    }

    fn track(n: usize) -> String {
        format!("spotify:track:{n:022}")
    }

    #[test]
    fn reads_check_their_scopes_before_any_request() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let any = server.mock(|when, then| {
            when.any_request();
            then.status(200);
        });
        let err = list("alice", LibraryKind::Tracks, None, || {
            Ok(client(tmp.path(), &server, &["user-library-read"]))
        })
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Auth);
        assert!(format!("{err:#}").contains("user-follow-read"), "{err:#}");
        any.assert_calls(0);
    }

    #[test]
    fn reads_list_each_kind_and_render_it() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/me/tracks");
            then.status(200).json_body(json!({"items": [
                {"added_at": "2026-10-16T13:02:11Z",
                 "track": {"type": "track", "uri": TRACK, "name": "Song",
                           "artists": [{"name": "Artist"}], "album": {"name": "Album"},
                           "duration_ms": 225000}}], "next": null}));
        });
        server.mock(|when, then| {
            when.method(GET).path("/v1/me/albums");
            then.status(200)
                .json_body(json!({"items": [], "next": null}));
        });
        server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/following")
                .query_param("limit", "5");
            then.status(200).json_body(json!({"artists": {
                "items": [{"type": "artist", "uri": "spotify:artist:1", "name": "Band"}],
                "cursors": {"after": null}}}));
        });
        let scopes = LIBRARY_READ_SCOPES;
        let mut c = client(tmp.path(), &server, scopes);
        let tracks = list("alice", LibraryKind::Tracks, None, || Ok(&mut c)).unwrap();
        assert_eq!(
            text(|o| tracks.write_text(o)),
            format!("2026-10-16T13:02:11Z {TRACK} \"Song\" — Artist — Album (3:45)\n")
        );
        let doc = serde_json::to_value(&tracks).unwrap();
        assert_eq!(doc["kind"], "tracks");
        assert_eq!(doc["items"][0]["added_at"], "2026-10-16T13:02:11Z");
        assert_eq!(doc["items"][0]["item"]["id"], TRACK);

        let albums = list("alice", LibraryKind::Albums, None, || Ok(&mut c)).unwrap();
        assert_eq!(text(|o| albums.write_text(o)), "no saved albums\n");

        let artists = list("alice", LibraryKind::Artists, Some(5), || Ok(&mut c)).unwrap();
        assert_eq!(
            text(|o| artists.write_text(o)),
            "spotify:artist:1 \"Band\"\n"
        );
        assert_eq!(serde_json::to_value(&artists).unwrap()["kind"], "artists");
    }

    #[test]
    fn add_checks_the_library_first_and_saves_only_what_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let check = contains(&server, &format!("{TRACK},{ALBUM}"), json!([true, false]));
        let save = library(&server, PUT, ALBUM);
        let mut c = client(tmp.path(), &server, LIBRARY_WRITE_SCOPES);
        let given = args(&[TRACK, ALBUM, TRACK]);
        let items = Items {
            args: &given,
            from_file: None,
            dry_run: false,
        };
        let mut err = Vec::new();
        let report = add(
            "alice",
            &paths(tmp.path()),
            &items,
            &mut io::empty(),
            || Ok(&mut c),
            &mut err,
        )
        .unwrap();
        check.assert();
        save.assert();
        assert_eq!(report.to_save, [ALBUM]);
        assert_eq!(report.already_saved, [TRACK]);
        assert_eq!(report.duplicates_dropped, 1);
        assert_eq!(String::from_utf8(err).unwrap(), "saved 1 of 1\n");
        let doc = serde_json::to_value(&report).unwrap();
        assert_eq!(doc["dry_run"], false);
        assert_eq!(doc["duplicates_dropped"], 1);
        assert!(doc.get("resolved").is_none(), "{doc}");
        assert_eq!(
            text(|o| report.write_text(o)),
            format!("saved 1 (1 already saved)\n  {ALBUM}\n")
        );
    }

    #[test]
    fn remove_removes_only_what_is_saved() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let check = contains(&server, &format!("{TRACK},{ALBUM}"), json!([true, false]));
        let gone = library(&server, DELETE, TRACK);
        let mut c = client(tmp.path(), &server, LIBRARY_WRITE_SCOPES);
        let given = args(&[TRACK, ALBUM]);
        let items = Items {
            args: &given,
            from_file: None,
            dry_run: false,
        };
        let report = remove(
            "alice",
            &paths(tmp.path()),
            &items,
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap();
        check.assert();
        gone.assert();
        assert_eq!(report.to_remove, [TRACK]);
        assert_eq!(report.not_saved, [ALBUM]);
        let doc = serde_json::to_value(&report).unwrap();
        assert!(doc.get("duplicates_dropped").is_none(), "{doc}");
        assert_eq!(
            text(|o| report.write_text(o)),
            format!("removed 1 (1 not saved)\n  {TRACK}\n")
        );
    }

    #[test]
    fn a_dry_run_checks_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let check = contains(&server, TRACK, json!([true]));
        let write = server.mock(|when, then| {
            when.path("/v1/me/library");
            then.status(200);
        });
        let mut c = client(tmp.path(), &server, LIBRARY_WRITE_SCOPES);
        let given = args(&[TRACK]);
        let items = Items {
            args: &given,
            from_file: None,
            dry_run: true,
        };
        let report = remove(
            "alice",
            &paths(tmp.path()),
            &items,
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap();
        check.assert();
        write.assert_calls(0);
        assert_eq!(report.to_remove, [TRACK]);
        assert_eq!(
            text(|o| report.write_text(o)),
            format!("would remove 1 (0 not saved)\n  {TRACK}\n")
        );
        assert_eq!(serde_json::to_value(&report).unwrap()["dry_run"], true);
    }

    #[test]
    fn a_playlist_uri_needs_the_playlist_follow_scopes() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let any = server.mock(|when, then| {
            when.any_request();
            then.status(200).json_body(json!([false]));
        });
        let given = args(&[PLAYLIST]);
        let items = Items {
            args: &given,
            from_file: None,
            dry_run: false,
        };
        let err = add(
            "alice",
            &paths(tmp.path()),
            &items,
            &mut io::empty(),
            || Ok(client(tmp.path(), &server, LIBRARY_WRITE_SCOPES)),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Auth);
        assert!(
            format!("{err:#}").contains("playlist-modify-private"),
            "{err:#}"
        );
        any.assert_calls(0);

        // A list without a playlist needs the library scopes alone.
        let given = args(&[TRACK]);
        let items = Items {
            args: &given,
            from_file: None,
            dry_run: true,
        };
        add(
            "alice",
            &paths(tmp.path()),
            &items,
            &mut io::empty(),
            || Ok(client(tmp.path(), &server, LIBRARY_WRITE_SCOPES)),
            &mut Vec::new(),
        )
        .unwrap();
        any.assert_calls(1);
    }

    #[test]
    fn a_pushed_playlist_is_followed_without_reading_or_writing_the_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        contains(&server, PLAYLIST, json!([false]));
        let follow = library(&server, PUT, PLAYLIST);
        let paths = paths(tmp.path());
        let given = args(&[PLAYLIST]);
        let items = Items {
            args: &given,
            from_file: None,
            dry_run: false,
        };
        let mut c = client(tmp.path(), &server, PLAYLIST_FOLLOW_SCOPES);
        let report = add(
            "alice",
            &paths,
            &items,
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap();
        follow.assert();
        assert_eq!(report.to_save, [PLAYLIST]);
        assert!(!paths.spotify_state().exists());
    }

    #[test]
    fn items_are_resolved_before_connecting() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let missing = tmp.path().join("missing.txt");
        let none: Vec<String> = Vec::new();
        let items = Items {
            args: &none,
            from_file: Some(&missing),
            dry_run: false,
        };
        let err = remove(
            "alice",
            &paths,
            &items,
            &mut io::empty(),
            no_client,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Other);

        let unmatched = args(&["TR:1"]);
        let items = Items {
            args: &unmatched,
            from_file: None,
            dry_run: false,
        };
        let err = add(
            "alice",
            &paths,
            &items,
            &mut io::empty(),
            no_client,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert!(format!("{err:#}").contains("has no match entry"), "{err:#}");

        let items = Items {
            args: &none,
            from_file: Some(Path::new("-")),
            dry_run: false,
        };
        let err = add(
            "alice",
            &paths,
            &items,
            &mut io::Cursor::new("# nothing\n\n"),
            no_client,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert_eq!(format!("{err:#}"), "no items given");
    }

    #[test]
    fn item_ids_resolve_and_are_echoed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        model::append(
            &model::file(&paths, IdKind::Track),
            &MatchEntry {
                status: Status::Matched,
                spotify_uri: Some(TRACK.into()),
                ..MatchEntry::manual_reject("TR:1", "2026-10-19T00:00:00Z".into())
            },
        )
        .unwrap();
        let server = MockServer::start();
        contains(&server, TRACK, json!([false]));
        let mut c = client(tmp.path(), &server, LIBRARY_WRITE_SCOPES);
        let given = args(&["TR:1"]);
        let items = Items {
            args: &given,
            from_file: None,
            dry_run: true,
        };
        let report = add(
            "alice",
            &paths,
            &items,
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap();
        let doc = serde_json::to_value(&report).unwrap();
        assert_eq!(doc["resolved"], json!({"TR:1": TRACK}));
        assert_eq!(
            text(|o| report.write_text(o)),
            format!("would save 1 (0 already saved)\n  {TRACK}\n  (TR:1 is {TRACK})\n")
        );
    }

    #[test]
    fn a_failure_after_a_chunk_landed_reports_what_landed() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let all: Vec<String> = (0..90).map(track).collect();
        for chunk in all.chunks(40) {
            let joined = chunk.join(",");
            let answer = vec![false; chunk.len()];
            server.mock(move |when, then| {
                when.method(GET)
                    .path("/v1/me/library/contains")
                    .query_param("uris", joined);
                then.status(200).json_body(json!(answer));
            });
        }
        let chunks: Vec<String> = all.chunks(40).map(|c| c.join(",")).collect();
        let first = library(&server, PUT, &chunks[0]);
        let second = server.mock(|when, then| {
            when.method(PUT)
                .path("/v1/me/library")
                .query_param("uris", chunks[1].clone());
            then.status(400)
                .json_body(json!({"error": {"status": 400, "message": "Invalid id"}}));
        });
        let third = library(&server, PUT, &chunks[2]);
        let mut c = client(tmp.path(), &server, LIBRARY_WRITE_SCOPES);
        let items = Items {
            args: &all,
            from_file: None,
            dry_run: false,
        };
        let err = add(
            "alice",
            &paths(tmp.path()),
            &items,
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap_err();
        first.assert();
        second.assert();
        third.assert_calls(0);
        let partial = err.downcast_ref::<PartialWrite>().expect("a partial write");
        assert_eq!(partial.done, all[..40]);
        assert_eq!(partial.total, 90);
        assert_eq!(error::kind(&err), Kind::Other);
        assert!(
            format!("{err:#}").starts_with("saved 40 of 90 ("),
            "{err:#}"
        );
    }
}
