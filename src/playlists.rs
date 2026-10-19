//! `sss playlists` and `sss playlist`: the playlists on the profile's
//! Spotify account — the member's own and those they follow — and the
//! contents of playlists named by URI, link, push ledger key or name; and
//! the resolution of a playlist target every playlist command shares.

use std::borrow::BorrowMut;
use std::io::{self, Write};

use anyhow::{Context as _, Result};
use serde::Serialize;

use crate::error::Kind;
use crate::fail;
use crate::player::folded;
use crate::push::{self, Ledger, PlaylistEntry};
use crate::search::result_line;
use crate::spotify::account::{Playlist, User};
use crate::spotify::auth::PLAYLIST_READ_SCOPES;
use crate::spotify::client::Spotify;
use crate::spotify::ids::{self, ContextKind};
use crate::spotify::item::Offered;
use crate::youtube::select::MOST_PLAYED_KEY;

/// What every refusal of a `sss playlist` target ends with.
const PLAYLIST_FORMS: &str = "`sss playlist` takes a playlist URI (spotify:playlist:<id>), an \
                              https://open.spotify.com/playlist/<id> address, the key of a \
                              playlist `sss push` made (ST:…, PL:…, YP:…), or the name of a \
                              playlist on the account";

/// The line `sss playlist` prints for a playlist whose contents Spotify
/// does not show the member.
const NOT_READABLE: &str =
    "contents not readable: Spotify shows a playlist's items only to its owner and collaborators";

/// Names a refusal of an unknown playlist name lists at most.
const NAMES_LISTED: usize = 20;

/// The prefixes of the ledger keys `sss push` writes for stations, Pandora
/// playlists and YouTube playlists.
const KEY_PREFIXES: [&str; 4] = ["ST:", "station:", "PL:", "YP:"];

/// What `sss playlists` reports: whose account it is, and its playlists in
/// Spotify's order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlaylistsReport {
    pub profile: String,
    pub user: User,
    pub playlists: Vec<Playlist>,
}

/// `sss playlists`: the account, then every page of its playlists.
pub fn playlists<S: BorrowMut<Spotify>>(
    profile: &str,
    connect: impl FnOnce() -> Result<S>,
) -> Result<PlaylistsReport> {
    let mut client = connect()?;
    let c = client.borrow_mut();
    c.require_scopes(profile, PLAYLIST_READ_SCOPES)?;
    let user = c.me().context("reading the Spotify account")?;
    let playlists = c.my_playlists().context("listing the playlists")?;
    Ok(PlaylistsReport {
        profile: profile.to_owned(),
        user,
        playlists,
    })
}

/// One playlist `sss playlist` read.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlaylistContents {
    pub playlist: Playlist,
    /// The ledger key of the push that made the playlist, if one did.
    pub pushed_as: Option<String>,
    /// The contents in playlist order; `None` when Spotify does not show
    /// them to the member.
    pub items: Option<Vec<Offered>>,
    /// The entries `items` does not show (removed or unreadable); `None`
    /// when the contents were not read.
    pub unavailable: Option<u32>,
}

/// What `sss playlist` reports: one entry per target, in the order given.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlaylistReport {
    pub profile: String,
    pub playlists: Vec<PlaylistContents>,
}

/// `a` or `an`, for a Spotify kind.
fn article(kind: &str) -> &'static str {
    match kind.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u') => "an",
        _ => "a",
    }
}

/// The playlist id of `text`, a playlist URI or open.spotify.com playlist
/// address. Anything else is refused with kind `input`.
fn playlist_id(text: &str) -> Result<String> {
    let (segment, id) = ids::split(text, PLAYLIST_FORMS)?;
    match segment.as_deref() {
        Some(kind) if kind == ContextKind::Playlist.as_str() => Ok(id),
        Some(other) => fail!(
            Kind::Input,
            "{text:?} is {} {other}, not a playlist; {PLAYLIST_FORMS}",
            article(other)
        ),
        None => fail!(
            Kind::Input,
            "{text:?} is a bare id, which `sss playlist` does not take; {PLAYLIST_FORMS}"
        ),
    }
}

/// A playlist target parsed without a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistTarget {
    /// A playlist id, from a URI or address, or from the created entry of
    /// ledger key `key`.
    Id { id: String, key: Option<String> },
    /// The name of a playlist on the account, trimmed.
    Name(String),
}

/// Whether `text` has the shape of a key `sss push` writes to its ledger:
/// a station, Pandora playlist or YouTube playlist key without
/// whitespace, or one of the fixed keys.
pub fn is_ledger_key(text: &str) -> bool {
    text == push::THUMBS_DOWN_KEY
        || text == MOST_PLAYED_KEY
        || (KEY_PREFIXES.iter().any(|p| text.starts_with(p))
            && !text.chars().any(char::is_whitespace))
}

/// The `sss push --only` part that makes the playlist of ledger key `key`.
pub(crate) fn push_part(key: &str) -> &'static str {
    if key == push::THUMBS_DOWN_KEY {
        "thumbs-down"
    } else if key == MOST_PLAYED_KEY {
        "most-played"
    } else if key.starts_with("PL:") || key.starts_with("YP:") {
        "playlists"
    } else {
        "stations"
    }
}

/// Parses `target`: a URI or address (by its prefix), a refused bare id,
/// a ledger key looked up in `ledger`, or else a name. Makes no request.
pub fn parse_target(target: &str, ledger: &Ledger) -> Result<PlaylistTarget> {
    let text = target.trim();
    if ["spotify:", "http://", "https://"]
        .iter()
        .any(|p| text.starts_with(p))
    {
        return Ok(PlaylistTarget::Id {
            id: playlist_id(text)?,
            key: None,
        });
    }
    if text.len() == 22 && text.chars().all(|c| c.is_ascii_alphanumeric()) {
        fail!(
            Kind::Input,
            "{text:?} is a bare id, which `sss playlist` does not take; {PLAYLIST_FORMS}"
        );
    }
    if is_ledger_key(text) {
        return match ledger.playlists.get(text) {
            Some(PlaylistEntry::Created { spotify_id, .. }) => Ok(PlaylistTarget::Id {
                id: spotify_id.clone(),
                key: Some(text.to_owned()),
            }),
            Some(PlaylistEntry::Pending { .. }) => fail!(
                Kind::Input,
                "{text} has no Spotify playlist yet; run `sss push --only {}`",
                push_part(text)
            ),
            None => fail!(
                Kind::Input,
                "{text} has no Spotify playlist; run `sss push`"
            ),
        };
    }
    if text.is_empty() {
        fail!(Kind::Input, "an empty playlist name; {PLAYLIST_FORMS}");
    }
    Ok(PlaylistTarget::Name(text.to_owned()))
}

/// The ledger key of the playlist push created as `id`, if any.
pub fn pushed_as(ledger: &Ledger, id: &str) -> Option<String> {
    ledger
        .playlists
        .iter()
        .find_map(|(key, entry)| match entry {
            PlaylistEntry::Created { spotify_id, .. } if spotify_id == id => Some(key.clone()),
            _ => None,
        })
}

/// The ledger key of `target`, resolved to `id`: the key it was given as,
/// else that of the created entry whose playlist `id` is.
pub fn target_key(target: &PlaylistTarget, ledger: &Ledger, id: &str) -> Option<String> {
    match target {
        PlaylistTarget::Id { key: Some(key), .. } => Some(key.clone()),
        _ => pushed_as(ledger, id),
    }
}

/// The one playlist in `listing` named `name` (trimmed and lowercased on
/// both sides). None or several is kind `input`.
pub fn by_name<'a>(listing: &'a [Playlist], name: &str) -> Result<&'a Playlist> {
    let wanted = folded(name);
    let named: Vec<&Playlist> = listing
        .iter()
        .filter(|p| folded(&p.name) == wanted)
        .collect();
    match named.as_slice() {
        [one] => Ok(one),
        [] if listing.is_empty() => fail!(
            Kind::Input,
            "no playlist named {name:?} on the account (it has no playlists)"
        ),
        [] => {
            let mut names = listing
                .iter()
                .take(NAMES_LISTED)
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if listing.len() > NAMES_LISTED {
                names.push_str(", …");
            }
            fail!(
                Kind::Input,
                "no playlist named {name:?} on the account (playlists: {names})"
            )
        }
        several => {
            let listed: Vec<String> = several
                .iter()
                .map(|p| format!("{} ({})", p.id, p.owner.name))
                .collect();
            fail!(
                Kind::Input,
                "{} playlists are named {name:?}: {}; give the URI",
                several.len(),
                listed.join(", ")
            )
        }
    }
}

/// The ids of `targets`, in order. Names are looked up in one listing of
/// the account's playlists, read only when a name is among them.
pub fn resolve_ids(c: &mut Spotify, targets: &[PlaylistTarget]) -> Result<Vec<String>> {
    let mut listing: Option<Vec<Playlist>> = None;
    let mut found = Vec::with_capacity(targets.len());
    for target in targets {
        found.push(match target {
            PlaylistTarget::Id { id, .. } => id.clone(),
            PlaylistTarget::Name(name) => {
                let all = match &mut listing {
                    Some(all) => all,
                    none => none.insert(c.my_playlists().context("listing the playlists")?),
                };
                ids::split(&by_name(all, name)?.id, PLAYLIST_FORMS)?.1
            }
        });
    }
    Ok(found)
}

/// The details of playlist `id`, read. A listed playlist is read again
/// all the same: the listing gives `items.total` for a playlist whose items
/// Spotify refuses to show, and only this read leaves it out.
pub fn details(c: &mut Spotify, id: &str) -> Result<Playlist> {
    c.playlist(id)
        .with_context(|| format!("reading playlist {}", ContextKind::Playlist.uri(id)))
}

/// The entries of `playlist` its contents `items` do not show: removed
/// items and those that parse as neither track nor episode.
pub fn unavailable(playlist: &Playlist, items: &[Offered]) -> u32 {
    let shown = u32::try_from(items.len()).unwrap_or(u32::MAX);
    playlist.total.map_or(0, |t| t.saturating_sub(shown))
}

/// `sss playlist`: every target parsed before any request (ledger keys
/// looked up in `ledger`), then names resolved through one listing, then
/// per target the playlist's details and, when Spotify exposes them, its
/// contents. The first failing read ends the command.
pub fn playlist<S: BorrowMut<Spotify>>(
    profile: &str,
    targets: &[String],
    ledger: &Ledger,
    connect: impl FnOnce() -> Result<S>,
) -> Result<PlaylistReport> {
    let parsed = targets
        .iter()
        .map(|t| parse_target(t, ledger))
        .collect::<Result<Vec<_>>>()?;
    let mut client = connect()?;
    let c = client.borrow_mut();
    c.require_scopes(profile, PLAYLIST_READ_SCOPES)?;
    let found = resolve_ids(c, &parsed)?;
    let mut playlists = Vec::with_capacity(found.len());
    for (id, target) in found.iter().zip(&parsed) {
        let id = id.as_str();
        let playlist = details(c, id)?;
        // Spotify leaves `items` out of the details exactly when it would
        // refuse the items read.
        let items = match playlist.total {
            Some(_) => Some(c.playlist_contents(id).with_context(|| {
                format!("reading the items of {}", ContextKind::Playlist.uri(id))
            })?),
            None => None,
        };
        playlists.push(PlaylistContents {
            unavailable: items.as_deref().map(|i| unavailable(&playlist, i)),
            playlist,
            pushed_as: target_key(target, ledger, id),
            items,
        });
    }
    Ok(PlaylistReport {
        profile: profile.to_owned(),
        playlists,
    })
}

/// `spotify:playlist:… "Morning" — 42 items, private, collaborative, by
/// Bob`: the count when Spotify gives one, the visibility, and the owner
/// when `user` is not it.
pub(crate) fn playlist_line(playlist: &Playlist, user: Option<&User>) -> String {
    let mut facts = Vec::new();
    match playlist.total {
        Some(1) => facts.push("1 item".to_owned()),
        Some(n) => facts.push(format!("{n} items")),
        None => {}
    }
    facts.push(
        match playlist.public {
            Some(true) => "public",
            Some(false) => "private",
            None => "unlisted",
        }
        .to_owned(),
    );
    if playlist.collaborative {
        facts.push("collaborative".to_owned());
    }
    if user.is_some_and(|u| u.id != playlist.owner.id) {
        facts.push(format!("by {}", playlist.owner.name));
    }
    format!("{} {:?} — {}", playlist.id, playlist.name, facts.join(", "))
}

impl PlaylistsReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        writeln!(out, "account {} ({})", self.user.id, self.user.name)?;
        if self.playlists.is_empty() {
            return writeln!(out, "no playlists");
        }
        for playlist in &self.playlists {
            writeln!(out, "{}", playlist_line(playlist, Some(&self.user)))?;
        }
        Ok(())
    }
}

impl PlaylistReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        for entry in &self.playlists {
            writeln!(out, "{}", playlist_line(&entry.playlist, None))?;
            match &entry.items {
                None => writeln!(out, "  {NOT_READABLE}")?,
                Some(items) if items.is_empty() => writeln!(out, "  no items")?,
                Some(items) => {
                    for (n, item) in items.iter().enumerate() {
                        writeln!(out, "  {}. {}", n + 1, result_line(item))?;
                    }
                }
            }
            match entry.unavailable {
                Some(1) => writeln!(out, "  (1 unavailable item not shown)")?,
                Some(n) if n > 0 => writeln!(out, "  ({n} unavailable items not shown)")?,
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error;
    use crate::spotify::account::Owner;
    use crate::spotify::test_support::client;
    use httpmock::prelude::*;
    use serde_json::{Value, json};

    const ID: &str = "37i9dQZF1DXcBWIGoYBM5M";
    const OTHER: &str = "4uLU6hMCjMI75M1A2tKUQC";
    const TRACK: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";

    fn targets(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| s.to_string()).collect()
    }

    fn text(write: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> String {
        let mut out = Vec::new();
        write(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn wire_playlist(id: &str, owner: &str, items: Option<u32>) -> Value {
        let mut p = json!({"uri": format!("spotify:playlist:{id}"), "name": "Morning",
                           "description": "", "public": false, "collaborative": false,
                           "owner": {"id": owner, "display_name": "Bob"}});
        if let Some(total) = items {
            p["items"] = json!({"href": "https://api.spotify.com/v1/x", "total": total});
        }
        p
    }

    fn listed(id: &str, owner: &str, public: Option<bool>, collaborative: bool) -> Playlist {
        Playlist {
            id: format!("spotify:playlist:{id}"),
            name: "Morning".into(),
            owner: Owner {
                id: owner.into(),
                name: "Bob".into(),
            },
            public,
            collaborative,
            total: Some(42),
            description: None,
            snapshot_id: None,
        }
    }

    fn alice() -> User {
        User {
            id: "alice-spotify".into(),
            name: "Alice".into(),
        }
    }

    #[test]
    fn the_playlist_commands_check_both_scopes_before_any_request() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut c = client(tmp.path(), &server, &["playlist-read-private"]);
        let err = playlists("alice", || Ok(&mut c)).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Auth);
        assert!(format!("{err:#}").contains("without playlist-read-collaborative"));
        let target = targets(&[&format!("spotify:playlist:{ID}")]);
        let err = playlist("alice", &target, &Ledger::default(), || Ok(&mut c)).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Auth);
        assert_eq!(c.requests(), 0);
    }

    #[test]
    fn playlists_reads_the_account_then_every_page() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let me = server.mock(|when, then| {
            when.method(GET).path("/v1/me");
            then.status(200)
                .json_body(json!({"id": "alice-spotify", "display_name": "Alice"}));
        });
        let page = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/playlists")
                .query_param("limit", "50")
                .query_param("offset", "0");
            then.status(200).json_body(json!({
                "items": [wire_playlist(ID, "alice-spotify", Some(3)),
                          wire_playlist(OTHER, "bob-spotify", Some(9))],
                "next": null}));
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_READ_SCOPES);
        let report = playlists("alice", || Ok(&mut c)).unwrap();
        me.assert();
        page.assert();
        assert_eq!(report.user, alice());
        assert_eq!(report.playlists.len(), 2);
        assert_eq!(report.playlists[1].owner.id, "bob-spotify");
        let doc = serde_json::to_value(&report).unwrap();
        assert_eq!(doc["user"], json!({"id": "alice-spotify", "name": "Alice"}));
        assert_eq!(doc["playlists"][0]["total"], 3);
    }

    #[test]
    fn a_failing_account_read_ends_playlists_before_the_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/me");
            then.status(404)
                .json_body(json!({"error": {"status": 404, "message": "nope"}}));
        });
        let listing = server.mock(|when, then| {
            when.method(GET).path("/v1/me/playlists");
            then.status(200)
                .json_body(json!({"items": [], "next": null}));
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_READ_SCOPES);
        let err = playlists("alice", || Ok(&mut c)).unwrap_err();
        listing.assert_calls(0);
        assert_eq!(error::kind(&err), Kind::Other);
        let msg = format!("{err:#}");
        assert!(msg.contains("reading the Spotify account"), "{msg}");
        assert!(msg.contains("nope"), "{msg}");
    }

    #[test]
    fn the_listing_text_marks_visibility_collaboration_and_other_owners() {
        let unlisted = Playlist {
            total: Some(1),
            ..listed(OTHER, "bob-spotify", None, true)
        };
        let report = PlaylistsReport {
            profile: "alice".into(),
            user: alice(),
            playlists: vec![
                listed(ID, "alice-spotify", Some(false), false),
                listed(ID, "alice-spotify", Some(true), false),
                unlisted,
            ],
        };
        assert_eq!(
            text(|o| report.write_text(o)),
            format!(
                "account alice-spotify (Alice)\n\
                 spotify:playlist:{ID} \"Morning\" — 42 items, private\n\
                 spotify:playlist:{ID} \"Morning\" — 42 items, public\n\
                 spotify:playlist:{OTHER} \"Morning\" — 1 item, unlisted, collaborative, by Bob\n"
            )
        );
        let empty = PlaylistsReport {
            profile: "alice".into(),
            user: alice(),
            playlists: vec![],
        };
        assert_eq!(
            text(|o| empty.write_text(o)),
            "account alice-spotify (Alice)\nno playlists\n"
        );
    }

    #[test]
    fn a_target_that_is_not_a_playlist_address_is_refused_before_any_request() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut c = client(tmp.path(), &server, PLAYLIST_READ_SCOPES);
        for (target, needle) in [
            (TRACK.to_owned(), "is a track, not a playlist"),
            (
                format!("https://open.spotify.com/album/{OTHER}"),
                "is an album, not a playlist",
            ),
            (ID.to_owned(), "is a bare id"),
            (
                "spotify:playlist:bad".to_owned(),
                "does not carry a Spotify id",
            ),
        ] {
            let all = targets(&[&format!("spotify:playlist:{ID}"), &target]);
            let err = playlist("alice", &all, &Ledger::default(), || Ok(&mut c)).unwrap_err();
            let message = format!("{err:#}");
            assert_eq!(error::kind(&err), Kind::Input, "{target}");
            assert!(message.contains(needle), "{target}: {message}");
            assert!(message.contains("spotify:playlist:<id>"), "{message}");
            assert!(!message.contains("bare 22-character id"), "{message}");
        }
        assert_eq!(c.requests(), 0);
    }

    #[test]
    fn playlist_uris_and_links_are_accepted() {
        assert_eq!(playlist_id(&format!("spotify:playlist:{ID}")).unwrap(), ID);
        assert_eq!(
            playlist_id(&format!(
                "https://open.spotify.com/intl-de/playlist/{ID}?si=x"
            ))
            .unwrap(),
            ID
        );
    }

    fn details<'a>(server: &'a MockServer, id: &str, items: Option<u32>) -> httpmock::Mock<'a> {
        let body = wire_playlist(id, "alice-spotify", items);
        let path = format!("/v1/playlists/{id}");
        server.mock(move |when, then| {
            when.method(GET).path(path).query_param_exists("fields");
            then.status(200).json_body(body);
        })
    }

    fn contents<'a>(server: &'a MockServer, id: &str) -> httpmock::Mock<'a> {
        let path = format!("/v1/playlists/{id}/items");
        server.mock(move |when, then| {
            when.method(GET).path(path).query_param("offset", "0");
            then.status(200).json_body(json!({
                "items": [{"item": {"type": "track", "uri": TRACK, "name": "Song",
                                    "artists": [{"name": "Artist"}],
                                    "album": {"name": "Album"}, "duration_ms": 225000}},
                          {"item": null}],
                "next": null}));
        })
    }

    #[test]
    fn playlist_reads_details_then_contents_per_target_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let first = details(&server, ID, Some(1));
        let first_items = contents(&server, ID);
        let second = details(&server, OTHER, None);
        let second_items = contents(&server, OTHER);
        let mut c = client(tmp.path(), &server, PLAYLIST_READ_SCOPES);
        let all = targets(&[
            &format!("spotify:playlist:{ID}"),
            &format!("https://open.spotify.com/playlist/{OTHER}"),
        ]);
        let report = playlist("alice", &all, &Ledger::default(), || Ok(&mut c)).unwrap();
        first.assert();
        first_items.assert();
        second.assert();
        second_items.assert_calls(0);
        assert_eq!(report.playlists.len(), 2);
        let items = report.playlists[0].items.as_ref().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, TRACK);
        assert_eq!(report.playlists[1].playlist.total, None);
        assert_eq!(report.playlists[1].items, None);
        let doc = serde_json::to_value(&report).unwrap();
        assert_eq!(doc["playlists"][1]["items"], Value::Null);
        assert_eq!(doc["playlists"][1]["playlist"]["total"], Value::Null);
        assert_eq!(
            text(|o| report.write_text(o)),
            format!(
                "spotify:playlist:{ID} \"Morning\" — 1 item, private\n\
                 \x20 1. {TRACK} \"Song\" — Artist — Album (3:45)\n\
                 spotify:playlist:{OTHER} \"Morning\" — private\n\
                 \x20 {NOT_READABLE}\n"
            )
        );
    }

    #[test]
    fn an_unknown_playlist_ends_the_command() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let missing = server.mock(|when, then| {
            when.method(GET).path(format!("/v1/playlists/{ID}"));
            then.status(404)
                .json_body(json!({"error": {"status": 404, "message": "Resource not found"}}));
        });
        let next = details(&server, OTHER, Some(0));
        let mut c = client(tmp.path(), &server, PLAYLIST_READ_SCOPES);
        let all = targets(&[
            &format!("spotify:playlist:{ID}"),
            &format!("spotify:playlist:{OTHER}"),
        ]);
        let err = playlist("alice", &all, &Ledger::default(), || Ok(&mut c)).unwrap_err();
        missing.assert();
        next.assert_calls(0);
        assert_eq!(error::kind(&err), Kind::Other);
        assert!(format!("{err:#}").contains("Resource not found"), "{err:#}");
    }

    #[test]
    fn a_refused_items_read_ends_the_command() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let first = details(&server, ID, Some(3));
        let items = server.mock(|when, then| {
            when.method(GET).path(format!("/v1/playlists/{ID}/items"));
            then.status(403)
                .json_body(json!({"error": {"status": 403, "message": "Forbidden"}}));
        });
        let next = details(&server, OTHER, Some(0));
        let mut c = client(tmp.path(), &server, PLAYLIST_READ_SCOPES);
        let all = targets(&[
            &format!("spotify:playlist:{ID}"),
            &format!("spotify:playlist:{OTHER}"),
        ]);
        let err = playlist("alice", &all, &Ledger::default(), || Ok(&mut c)).unwrap_err();
        first.assert();
        items.assert();
        next.assert_calls(0);
        assert_eq!(error::kind(&err), Kind::Config);
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&format!("reading the items of spotify:playlist:{ID}")),
            "{msg}"
        );
    }

    fn ledger(entries: Value) -> Ledger {
        serde_json::from_value(json!({ "playlists": entries })).unwrap()
    }

    fn named(id: &str, name: &str, owner: &str) -> Value {
        json!({"uri": format!("spotify:playlist:{id}"), "name": name, "public": false,
               "collaborative": false, "owner": {"id": owner, "display_name": owner},
               "items": {"href": "https://api.spotify.com/v1/x", "total": 1}})
    }

    fn listing<'a>(server: &'a MockServer, playlists: Vec<Value>) -> httpmock::Mock<'a> {
        server.mock(move |when, then| {
            when.method(GET).path("/v1/me/playlists");
            then.status(200)
                .json_body(json!({"items": playlists, "next": null}));
        })
    }

    #[test]
    fn targets_parse_by_prefix_key_shape_then_name() {
        let led = ledger(json!({
            "ST:1": {"status": "created", "spotify_id": ID},
            "ST:2": {"status": "pending", "name": "Jazz", "at": "2026-10-19T00:00:00Z"},
            "thumbs-down": {"status": "pending", "name": "x", "at": "2026-10-19T00:00:00Z"},
            "PL:7": {"status": "pending", "name": "x", "at": "2026-10-19T00:00:00Z"}
        }));
        assert_eq!(
            parse_target(&format!(" spotify:playlist:{ID} "), &led).unwrap(),
            PlaylistTarget::Id {
                id: ID.into(),
                key: None
            }
        );
        assert_eq!(
            parse_target(
                &format!("https://open.spotify.com/intl-de/playlist/{ID}?si=x"),
                &led
            )
            .unwrap(),
            PlaylistTarget::Id {
                id: ID.into(),
                key: None
            }
        );
        assert_eq!(
            parse_target("ST:1", &led).unwrap(),
            PlaylistTarget::Id {
                id: ID.into(),
                key: Some("ST:1".into())
            }
        );
        for (text, needle) in [
            (
                "ST:2",
                "ST:2 has no Spotify playlist yet; run `sss push --only stations`",
            ),
            (
                "thumbs-down",
                "thumbs-down has no Spotify playlist yet; run `sss push --only thumbs-down`",
            ),
            (
                "PL:7",
                "PL:7 has no Spotify playlist yet; run `sss push --only playlists`",
            ),
        ] {
            let err = parse_target(text, &led).unwrap_err();
            assert_eq!(error::kind(&err), Kind::Input);
            assert_eq!(format!("{err:#}"), needle);
        }
        for key in ["ST:9", "station:41", "PL:x", "YP:x", "youtube:most-played"] {
            let err = parse_target(key, &Ledger::default()).unwrap_err();
            assert_eq!(error::kind(&err), Kind::Input);
            assert_eq!(
                format!("{err:#}"),
                format!("{key} has no Spotify playlist; run `sss push`")
            );
        }
        for name in ["Kids Songs", "PL: my list", "Rock / Pop", "create", "ST:"] {
            let want = if name == "ST:" {
                None
            } else {
                Some(PlaylistTarget::Name(name.into()))
            };
            assert_eq!(parse_target(name, &led).ok(), want, "{name}");
        }
        let err = parse_target("   ", &led).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
    }

    #[test]
    fn pushed_as_finds_the_created_entry_of_an_id() {
        let led = ledger(json!({
            "ST:1": {"status": "pending", "name": "x", "at": "t"},
            "ST:2": {"status": "created", "spotify_id": ID}
        }));
        assert_eq!(pushed_as(&led, ID).as_deref(), Some("ST:2"));
        assert_eq!(pushed_as(&led, OTHER), None);
    }

    #[test]
    fn a_name_resolves_through_one_listing_case_insensitively() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let list = listing(
            &server,
            vec![
                named(OTHER, "Road trip", "bob-spotify"),
                named(ID, "Rock / Pop", "alice-spotify"),
            ],
        );
        let first = details(&server, ID, Some(1));
        let first_items = contents(&server, ID);
        let mut c = client(tmp.path(), &server, PLAYLIST_READ_SCOPES);
        let all = targets(&["  rock / POP ", "Rock / Pop"]);
        let report = playlist("alice", &all, &Ledger::default(), || Ok(&mut c)).unwrap();
        list.assert_calls(1);
        first.assert_calls(2);
        first_items.assert_calls(2);
        assert_eq!(report.playlists.len(), 2);
        assert_eq!(
            report.playlists[0].playlist.id,
            format!("spotify:playlist:{ID}")
        );
        assert_eq!(report.playlists[0].pushed_as, None);
        assert_eq!(report.playlists[0].unavailable, Some(0));
    }

    #[test]
    fn a_followed_playlist_named_is_read_and_its_items_left_unread() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        // The listing counts the items of a playlist the account only
        // follows; the details read does not.
        listing(&server, vec![named(OTHER, "Road trip", "bob-spotify")]);
        let read = details(&server, OTHER, None);
        let items = contents(&server, OTHER);
        let mut c = client(tmp.path(), &server, PLAYLIST_READ_SCOPES);
        let report = playlist(
            "alice",
            &targets(&["Road trip"]),
            &Ledger::default(),
            || Ok(&mut c),
        )
        .unwrap();
        read.assert();
        items.assert_calls(0);
        assert_eq!(report.playlists[0].playlist.total, None);
        assert_eq!(report.playlists[0].items, None);
    }

    #[test]
    fn a_uri_or_key_target_makes_no_listing_request_and_reports_the_key() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let list = listing(&server, vec![]);
        let first = details(&server, ID, Some(1));
        contents(&server, ID);
        let mut c = client(tmp.path(), &server, PLAYLIST_READ_SCOPES);
        let led = ledger(json!({"ST:1": {"status": "created", "spotify_id": ID}}));
        let all = targets(&["ST:1", &format!("spotify:playlist:{ID}")]);
        let report = playlist("alice", &all, &led, || Ok(&mut c)).unwrap();
        list.assert_calls(0);
        first.assert_calls(2);
        assert_eq!(report.playlists[0].pushed_as.as_deref(), Some("ST:1"));
        assert_eq!(report.playlists[1].pushed_as.as_deref(), Some("ST:1"));
        let doc = serde_json::to_value(&report).unwrap();
        assert_eq!(doc["playlists"][0]["pushed_as"], "ST:1");
        assert_eq!(doc["playlists"][0]["unavailable"], 0);
    }

    #[test]
    fn a_key_absent_from_the_ledger_is_refused_before_any_request() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut c = client(tmp.path(), &server, PLAYLIST_READ_SCOPES);
        let err = playlist("alice", &targets(&["ST:9"]), &Ledger::default(), || {
            Ok(&mut c)
        })
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert_eq!(c.requests(), 0);
    }

    #[test]
    fn several_playlists_of_a_name_are_refused_with_their_uris_and_owners() {
        let listing = vec![
            listed(ID, "alice-spotify", Some(false), false),
            Playlist {
                owner: Owner {
                    id: "bob-spotify".into(),
                    name: "Bob".into(),
                },
                ..listed(OTHER, "bob-spotify", Some(true), false)
            },
        ];
        let err = by_name(&listing, "morning").unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert_eq!(
            format!("{err:#}"),
            format!(
                "2 playlists are named \"morning\": spotify:playlist:{ID} (Bob), \
                 spotify:playlist:{OTHER} (Bob); give the URI"
            )
        );
    }

    #[test]
    fn an_unknown_name_lists_at_most_twenty_names() {
        let listing: Vec<Playlist> = (1..=21)
            .map(|n| Playlist {
                name: format!("List {n}"),
                ..listed(ID, "alice-spotify", None, false)
            })
            .collect();
        let err = by_name(&listing, "Kids Songz").unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        let msg = format!("{err:#}");
        assert!(
            msg.starts_with("no playlist named \"Kids Songz\" on the account (playlists: List 1, "),
            "{msg}"
        );
        assert!(msg.ends_with("List 20, …)"), "{msg}");
        assert!(!msg.contains("List 21"), "{msg}");
        let err = by_name(&[], "Kids Songz").unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "no playlist named \"Kids Songz\" on the account (it has no playlists)"
        );
    }

    #[test]
    fn entries_the_contents_cannot_show_are_counted_as_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        details(&server, ID, Some(5));
        server.mock(|when, then| {
            when.method(GET).path(format!("/v1/playlists/{ID}/items"));
            then.status(200).json_body(json!({
                "items": [
                    {"item": {"type": "track", "uri": TRACK, "name": "Song",
                              "artists": [{"name": "Artist"}], "album": {"name": "Album"},
                              "duration_ms": 225000}},
                    {"item": null},
                    {"item": {"type": "track", "name": "no uri"}},
                    {"item": {"type": "episode", "uri": "spotify:episode:e", "name": "Talk"}},
                    {"item": {"type": "track", "uri": "spotify:local:A:B:Song:180",
                              "name": "Home"}}],
                "next": null}));
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_READ_SCOPES);
        let all = targets(&[&format!("spotify:playlist:{ID}")]);
        let report = playlist("alice", &all, &Ledger::default(), || Ok(&mut c)).unwrap();
        let entry = &report.playlists[0];
        assert_eq!(entry.items.as_ref().unwrap().len(), 3);
        assert_eq!(entry.unavailable, Some(2));
        let text = text(|o| report.write_text(o));
        assert!(
            text.ends_with("  (2 unavailable items not shown)\n"),
            "{text}"
        );
    }
}
