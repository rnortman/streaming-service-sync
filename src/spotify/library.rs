//! The library reads — saved tracks, saved albums, followed artists — and
//! the library removal they make safe to call. Entries that parse as
//! nothing usable are dropped rather than failing the response.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::spotify::client::{FoundAlbum, FoundArtist, Spotify, WireAlbum, WireArtist};
use crate::spotify::item::Offered;
use crate::spotify::playback::item_of;

/// Saved tracks, saved albums or followed artists per page: the
/// endpoints' maximum.
pub const LIBRARY_PAGE: usize = 50;

/// One entry of the member's library.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Saved {
    /// When it was saved, as Spotify gives it (RFC 3339); `None` for a
    /// followed artist, for which Spotify gives no date.
    pub added_at: Option<String>,
    pub item: Offered,
}

#[derive(Deserialize)]
struct WireSavedTrack {
    added_at: Option<String>,
    #[serde(alias = "item")]
    track: Value,
}

#[derive(Deserialize)]
struct WireSavedAlbum {
    added_at: Option<String>,
    album: WireAlbum,
}

#[derive(Deserialize)]
struct WireFollowed {
    artists: WireCursorPage,
}

#[derive(Deserialize, Default)]
struct WireCursorPage {
    #[serde(default)]
    items: Vec<Value>,
    cursors: Option<WireCursors>,
}

#[derive(Deserialize)]
struct WireCursors {
    after: Option<String>,
}

impl Spotify {
    /// `DELETE me/library` in chunks of
    /// [`LIBRARY_CHUNK`](super::client::LIBRARY_CHUNK): removes saved
    /// tracks and albums and unfollows artists and playlists. `removed` is
    /// called after each chunk lands; its error stops the writes.
    pub fn library_remove(
        &mut self,
        uris: &[String],
        removed: &mut dyn FnMut(&[String]) -> Result<()>,
    ) -> Result<()> {
        self.library_write("DELETE", uris, removed)
    }

    /// `GET me/tracks`, every page or the first `limit` entries: the saved
    /// tracks, most recently saved first.
    pub fn saved_tracks(&mut self, limit: Option<usize>) -> Result<Vec<Saved>> {
        Ok(self
            .get_pages("me/tracks", &[], LIBRARY_PAGE, limit)?
            .into_iter()
            .filter_map(|v| {
                let s: WireSavedTrack = serde_json::from_value(v).ok()?;
                Some(Saved {
                    added_at: s.added_at,
                    item: item_of(s.track)?,
                })
            })
            .collect())
    }

    /// `GET me/albums`, every page or the first `limit` entries: the saved
    /// albums, most recently saved first.
    pub fn saved_albums(&mut self, limit: Option<usize>) -> Result<Vec<Saved>> {
        Ok(self
            .get_pages("me/albums", &[], LIBRARY_PAGE, limit)?
            .into_iter()
            .filter_map(|v| serde_json::from_value::<WireSavedAlbum>(v).ok())
            .map(|s| Saved {
                added_at: s.added_at,
                item: Offered::from(&FoundAlbum::from(s.album)),
            })
            .collect())
    }

    /// `GET me/following?type=artist`, following `cursors.after` until it
    /// is absent, or the first `limit` entries: the followed artists.
    pub fn followed_artists(&mut self, limit: Option<usize>) -> Result<Vec<Saved>> {
        let mut all: Vec<Value> = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let size = limit
                .map_or(LIBRARY_PAGE, |l| {
                    l.saturating_sub(all.len()).clamp(1, LIBRARY_PAGE)
                })
                .to_string();
            let mut query = vec![("type", "artist"), ("limit", size.as_str())];
            if let Some(a) = &after {
                query.push(("after", a));
            }
            let page: WireFollowed = self.get("me/following", &query)?;
            let got = page.artists.items.len();
            all.extend(page.artists.items);
            after = page.artists.cursors.and_then(|c| c.after);
            if limit.is_some_and(|l| all.len() >= l) || after.is_none() || got == 0 {
                break;
            }
        }
        all.truncate(limit.unwrap_or(usize::MAX));
        Ok(all
            .into_iter()
            .filter_map(|v| serde_json::from_value::<WireArtist>(v).ok())
            .map(|a| Saved {
                added_at: None,
                item: Offered::from(&FoundArtist::from(a)),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spotify::test_support::client;
    use httpmock::prelude::*;
    use serde_json::json;

    const ARTIST: &str = "spotify:artist:0OdUWJ0sBjDrqHygGUXeCF";
    const ALBUM: &str = "spotify:album:4aawyAB9vmqN3uQ7FjRGTy";

    fn track(n: usize) -> Value {
        json!({"type": "track", "uri": format!("spotify:track:{n:022}"), "name": format!("Song {n}"),
               "artists": [{"name": "Artist"}], "album": {"name": "Album"},
               "duration_ms": 225000})
    }

    fn saved(n: usize) -> Value {
        json!({"added_at": format!("2026-10-{:02}T13:02:11Z", 1 + n % 28), "track": track(n)})
    }

    #[test]
    fn library_remove_deletes_in_forties_with_the_uris_query() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let all: Vec<String> = (0..50).map(|n| format!("spotify:track:{n}")).collect();
        let mut mocks = Vec::new();
        for chunk in all.chunks(40) {
            let joined = chunk.join(",");
            mocks.push(server.mock(move |when, then| {
                when.method(DELETE)
                    .path("/v1/me/library")
                    .query_param("uris", joined)
                    .json_body(json!({}));
                then.status(200);
            }));
        }
        let mut c = client(tmp.path(), &server, &[]);
        let mut chunks = Vec::new();
        c.library_remove(&all, &mut |chunk| {
            chunks.push(chunk.len());
            Ok(())
        })
        .unwrap();
        for m in &mocks {
            m.assert();
        }
        assert_eq!(chunks, [40, 10]);
    }

    #[test]
    fn saved_tracks_pages_by_offset_until_next_is_null_and_drops_unparseable_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let first: Vec<Value> = (0..50).map(saved).collect();
        let page1 = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/tracks")
                .query_param("limit", "50")
                .query_param("offset", "0");
            then.status(200)
                .json_body(json!({"items": first, "next": "https://api.spotify.com/next"}));
        });
        let page2 = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/tracks")
                .query_param("limit", "50")
                .query_param("offset", "50");
            then.status(200).json_body(json!({"items": [
                saved(50),
                {"added_at": "2026-10-01T00:00:00Z", "track": {"type": "track"}},
                null,
                {"added_at": "2026-10-02T00:00:00Z", "item": track(51)},
            ], "next": null}));
        });
        let mut c = client(tmp.path(), &server, &[]);
        let tracks = c.saved_tracks(None).unwrap();
        page1.assert();
        page2.assert();
        assert_eq!(tracks.len(), 52);
        assert_eq!(tracks[0].added_at.as_deref(), Some("2026-10-01T13:02:11Z"));
        assert_eq!(tracks[0].item.id, format!("spotify:track:{:022}", 0));
        assert_eq!(tracks[0].item.artists, ["Artist"]);
        assert_eq!(tracks[51].item.name, "Song 51");
        let doc = serde_json::to_value(&tracks[0]).unwrap();
        assert_eq!(doc["added_at"], "2026-10-01T13:02:11Z");
        assert_eq!(doc["item"]["name"], "Song 0");
    }

    #[test]
    fn saved_tracks_stop_at_the_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let five: Vec<Value> = (0..5).map(saved).collect();
        let page = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/tracks")
                .query_param("limit", "5")
                .query_param("offset", "0");
            then.status(200)
                .json_body(json!({"items": five, "next": "https://api.spotify.com/next"}));
        });
        let mut c = client(tmp.path(), &server, &[]);
        assert_eq!(c.saved_tracks(Some(5)).unwrap().len(), 5);
        page.assert_calls(1);

        let first: Vec<Value> = (0..50).map(saved).collect();
        let full = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/tracks")
                .query_param("limit", "50")
                .query_param("offset", "0");
            then.status(200)
                .json_body(json!({"items": first, "next": "https://api.spotify.com/next"}));
        });
        let rest: Vec<Value> = (50..60).map(saved).collect();
        let tail = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/tracks")
                .query_param("limit", "10")
                .query_param("offset", "50");
            then.status(200)
                .json_body(json!({"items": rest, "next": "https://api.spotify.com/next"}));
        });
        assert_eq!(c.saved_tracks(Some(60)).unwrap().len(), 60);
        full.assert_calls(1);
        tail.assert_calls(1);
    }

    #[test]
    fn saved_albums_read_the_album_of_each_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let page = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/albums")
                .query_param("limit", "50")
                .query_param("offset", "0");
            then.status(200).json_body(json!({"items": [
                {"added_at": "2026-10-16T13:02:11Z",
                 "album": {"type": "album", "uri": ALBUM, "name": "An Album",
                           "artists": [{"name": "Band"}], "album_type": "album",
                           "release_date": "2001-05-01"}},
                {"added_at": "2026-10-16T13:02:11Z", "album": {"name": "no uri"}},
            ], "next": null}));
        });
        let mut c = client(tmp.path(), &server, &[]);
        let albums = c.saved_albums(None).unwrap();
        page.assert();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].item.id, ALBUM);
        assert_eq!(albums[0].item.artists, ["Band"]);
        assert_eq!(albums[0].item.release_date.as_deref(), Some("2001-05-01"));
    }

    #[test]
    fn followed_artists_follow_the_cursor_until_it_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let first = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/following")
                .query_param("type", "artist")
                .query_param("limit", "50")
                .query_param_missing("after");
            then.status(200).json_body(json!({"artists": {
                "items": [{"type": "artist", "uri": ARTIST, "name": "Band of Horses"},
                          {"type": "artist", "name": "no uri"}],
                "next": "https://api.spotify.com/next",
                "cursors": {"after": "0OdUWJ0sBjDrqHygGUXeCF"}}}));
        });
        let second = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/following")
                .query_param("type", "artist")
                .query_param("after", "0OdUWJ0sBjDrqHygGUXeCF");
            then.status(200).json_body(json!({"artists": {
                "items": [{"type": "artist", "uri": "spotify:artist:1", "name": "Second"}],
                "next": null, "cursors": {"after": null}}}));
        });
        let mut c = client(tmp.path(), &server, &[]);
        let artists = c.followed_artists(None).unwrap();
        first.assert();
        second.assert();
        assert_eq!(
            artists,
            [
                Saved {
                    added_at: None,
                    item: Offered::named(ARTIST, "Band of Horses"),
                },
                Saved {
                    added_at: None,
                    item: Offered::named("spotify:artist:1", "Second"),
                },
            ]
        );
        let doc = serde_json::to_value(&artists[0]).unwrap();
        assert_eq!(doc["added_at"], Value::Null);

        let one = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/following")
                .query_param("limit", "1");
            then.status(200).json_body(json!({"artists": {
                "items": [{"type": "artist", "uri": ARTIST, "name": "Band of Horses"}],
                "cursors": {"after": "0OdUWJ0sBjDrqHygGUXeCF"}}}));
        });
        assert_eq!(c.followed_artists(Some(1)).unwrap().len(), 1);
        one.assert_calls(1);
        second.assert_calls(1);
    }
}
