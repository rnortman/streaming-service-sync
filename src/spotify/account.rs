//! The account reads: the member's profile, play history, top tracks and
//! artists, playlists, and a playlist's details and contents. Each is a
//! `GET`, paced and retried by the client's transport; entries that parse
//! as nothing usable are dropped rather than failing the response.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::spotify::client::{
    FoundArtist, PLAYLIST_CHUNK, Spotify, WireArtist, WirePage, parsed_items,
};
use crate::spotify::item::Offered;
use crate::spotify::playback::{Context, WireContext, context_of, item_of};

/// The most items a history or top-items read returns.
pub const PAGE_MAX: u32 = 50;

/// Playlists per `me/playlists` page: the endpoint's maximum.
pub const PLAYLISTS_CHUNK: usize = 50;

/// The `fields` of a playlist's details read.
const PLAYLIST_FIELDS: &str =
    "uri,name,description,public,collaborative,owner(id,display_name),items(total)";

/// The `fields` of a playlist's items read.
const CONTENTS_FIELDS: &str = "items(item(type,uri,name,artists(name),\
                               album(name,album_type,release_date),duration_ms,explicit,\
                               external_ids)),next";

/// The signed-in Spotify account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct User {
    pub id: String,
    /// The display name, else the id.
    pub name: String,
}

/// One play from the member's history.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Played {
    /// When the play finished, as Spotify gives it (RFC 3339).
    pub played_at: String,
    pub track: Offered,
    /// The playlist, album or artist it was played from.
    pub context: Option<Context>,
}

/// What a top-items read ranks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TopKind {
    Tracks,
    Artists,
}

impl TopKind {
    /// `tracks` or `artists`: the path segment and the serialized name.
    pub fn as_str(self) -> &'static str {
        match self {
            TopKind::Tracks => "tracks",
            TopKind::Artists => "artists",
        }
    }
}

/// The window a top-items ranking covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeRange {
    Short,
    Medium,
    Long,
}

impl TimeRange {
    /// `short`, `medium` or `long`.
    pub fn as_str(self) -> &'static str {
        match self {
            TimeRange::Short => "short",
            TimeRange::Medium => "medium",
            TimeRange::Long => "long",
        }
    }

    /// Spotify's `time_range` value.
    fn wire(self) -> &'static str {
        match self {
            TimeRange::Short => "short_term",
            TimeRange::Medium => "medium_term",
            TimeRange::Long => "long_term",
        }
    }
}

/// A playlist's owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Owner {
    pub id: String,
    /// The display name, else the id.
    pub name: String,
}

/// A playlist on the member's account, or one read by id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Playlist {
    /// The playlist's URI.
    pub id: String,
    pub name: String,
    pub owner: Owner,
    /// Spotify's nullable flag: `None` when it gives none.
    pub public: Option<bool>,
    pub collaborative: bool,
    /// The item count, Spotify's `items.total`; `None` when Spotify does
    /// not expose the playlist's items to the member.
    pub total: Option<u32>,
    /// `None` when absent or empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Deserialize)]
struct WireUser {
    id: String,
    display_name: Option<String>,
}

/// `display_name` when it is set and non-empty, else `id`.
fn name_or_id(display_name: Option<String>, id: &str) -> String {
    display_name
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| id.to_owned())
}

#[derive(Deserialize)]
struct WirePlayed {
    played_at: String,
    track: Value,
    context: Option<WireContext>,
}

#[derive(Deserialize)]
struct WireCount {
    total: Option<u32>,
}

#[derive(Deserialize)]
struct WirePlaylist {
    uri: String,
    name: String,
    description: Option<String>,
    public: Option<bool>,
    #[serde(default)]
    collaborative: bool,
    owner: WireUser,
    items: Option<WireCount>,
}

impl From<WirePlaylist> for Playlist {
    fn from(w: WirePlaylist) -> Self {
        Playlist {
            id: w.uri,
            name: w.name,
            owner: Owner {
                name: name_or_id(w.owner.display_name, &w.owner.id),
                id: w.owner.id,
            },
            public: w.public,
            collaborative: w.collaborative,
            total: w.items.and_then(|i| i.total),
            description: w.description.filter(|d| !d.is_empty()),
        }
    }
}

#[derive(Deserialize)]
struct WireEntry {
    item: Option<Value>,
}

impl Spotify {
    /// `GET me`: the signed-in account.
    pub fn me(&mut self) -> Result<User> {
        let w: WireUser = self.get("me", &[])?;
        Ok(User {
            name: name_or_id(w.display_name, &w.id),
            id: w.id,
        })
    }

    /// `GET me/player/recently-played`: up to `limit` finished plays,
    /// newest first.
    pub fn recently_played(&mut self, limit: u32) -> Result<Vec<Played>> {
        let limit = limit.to_string();
        let page: WirePage = self.get("me/player/recently-played", &[("limit", &limit)])?;
        Ok(parsed_items::<WirePlayed>(page)
            .into_iter()
            .filter_map(|p| {
                Some(Played {
                    track: item_of(p.track)?,
                    played_at: p.played_at,
                    context: p.context.and_then(context_of),
                })
            })
            .collect())
    }

    /// `GET me/top/{kind}`: up to `limit` items over `range`, in ranked
    /// order; an artist as its `id` and `name`.
    pub fn top(&mut self, kind: TopKind, range: TimeRange, limit: u32) -> Result<Vec<Offered>> {
        let path = format!("me/top/{}", kind.as_str());
        let limit = limit.to_string();
        let page: WirePage = self.get(&path, &[("time_range", range.wire()), ("limit", &limit)])?;
        Ok(match kind {
            TopKind::Tracks => page.items.into_iter().filter_map(item_of).collect(),
            TopKind::Artists => parsed_items::<WireArtist>(page)
                .into_iter()
                .map(|a| Offered::from(&FoundArtist::from(a)))
                .collect(),
        })
    }

    /// `GET me/playlists`, every page: the member's playlists, their own
    /// and those they follow, in Spotify's order.
    pub fn my_playlists(&mut self) -> Result<Vec<Playlist>> {
        Ok(self
            .get_all("me/playlists", &[], PLAYLISTS_CHUNK)?
            .into_iter()
            .filter_map(|v| serde_json::from_value::<WirePlaylist>(v).ok())
            .map(Playlist::from)
            .collect())
    }

    /// `GET playlists/{id}`: the playlist's details. An id Spotify does
    /// not know is an [`ApiError`](super::client::ApiError) with status
    /// 404.
    pub fn playlist(&mut self, playlist_id: &str) -> Result<Playlist> {
        let path = format!("playlists/{playlist_id}");
        let w: WirePlaylist = self.get(&path, &[("fields", PLAYLIST_FIELDS)])?;
        Ok(Playlist::from(w))
    }

    /// `GET playlists/{id}/items`, every page: each item in playlist order;
    /// a removed item, or one that parses as neither track nor episode, is
    /// dropped.
    pub fn playlist_contents(&mut self, playlist_id: &str) -> Result<Vec<Offered>> {
        let path = format!("playlists/{playlist_id}/items");
        Ok(self
            .get_all(&path, &[("fields", CONTENTS_FIELDS)], PLAYLIST_CHUNK)?
            .into_iter()
            .filter_map(|v| serde_json::from_value::<WireEntry>(v).ok())
            .filter_map(|e| e.item.and_then(item_of))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spotify::test_support::client;
    use httpmock::prelude::*;
    use serde_json::json;

    const TRACK_A: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";
    const PLAYLIST: &str = "37i9dQZF1DXcBWIGoYBM5M";

    fn track_a() -> Value {
        json!({"type": "track", "uri": TRACK_A, "name": "Song",
               "artists": [{"name": "Artist"}],
               "album": {"name": "Album", "album_type": "album", "release_date": "2001"},
               "duration_ms": 225000, "explicit": false,
               "external_ids": {"isrc": "USAAA0100001"}})
    }

    fn offered_a() -> Offered {
        Offered {
            id: TRACK_A.into(),
            name: "Song".into(),
            artists: vec!["Artist".into()],
            album: Some("Album".into()),
            album_type: Some("album".into()),
            release_date: Some("2001".into()),
            duration_ms: Some(225000),
            explicit: Some(false),
            isrc: Some("USAAA0100001".into()),
        }
    }

    #[test]
    fn me_reads_the_id_and_the_display_name_else_the_id() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut c = client(tmp.path(), &server, &[]);
        let mut named = server.mock(|when, then| {
            when.method(GET).path("/v1/me");
            then.status(200)
                .json_body(json!({"id": "alice-spotify", "display_name": "Alice"}));
        });
        assert_eq!(
            c.me().unwrap(),
            User {
                id: "alice-spotify".into(),
                name: "Alice".into()
            }
        );
        named.delete();
        server.mock(|when, then| {
            when.method(GET).path("/v1/me");
            then.status(200)
                .json_body(json!({"id": "alice-spotify", "display_name": null}));
        });
        assert_eq!(c.me().unwrap().name, "alice-spotify");
    }

    #[test]
    fn recently_played_sends_the_limit_and_parses_each_play() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut c = client(tmp.path(), &server, &[]);
        let history = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/player/recently-played")
                .query_param("limit", "10");
            then.status(200).json_body(json!({"items": [
                {"played_at": "2026-10-16T13:02:11.123Z", "track": track_a(),
                 "context": {"type": "playlist", "uri": "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M",
                             "href": "https://api.spotify.com/v1/playlists/x"}},
                {"played_at": "2026-10-16T12:58:00.000Z", "track": track_a(), "context": null},
                {"played_at": "2026-10-16T12:50:00.000Z", "track": {"type": "track"}},
            ], "next": null, "cursors": {"after": "1", "before": "2"}}));
        });
        let plays = c.recently_played(10).unwrap();
        history.assert_calls(1);
        assert_eq!(
            plays,
            [
                Played {
                    played_at: "2026-10-16T13:02:11.123Z".into(),
                    track: offered_a(),
                    context: Some(Context {
                        kind: "playlist".into(),
                        uri: "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M".into(),
                    }),
                },
                Played {
                    played_at: "2026-10-16T12:58:00.000Z".into(),
                    track: offered_a(),
                    context: None,
                },
            ]
        );
        let doc = serde_json::to_value(&plays[1]).unwrap();
        assert_eq!(doc["context"], Value::Null);
        assert_eq!(doc["track"]["id"], TRACK_A);
    }

    #[test]
    fn top_sends_the_kind_range_and_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut c = client(tmp.path(), &server, &[]);
        let mut sent = Vec::new();
        for (range, wire) in [
            (TimeRange::Short, "short_term"),
            (TimeRange::Medium, "medium_term"),
            (TimeRange::Long, "long_term"),
        ] {
            sent.push(server.mock(|when, then| {
                when.method(GET)
                    .path("/v1/me/top/tracks")
                    .query_param("time_range", wire)
                    .query_param("limit", "5");
                then.status(200)
                    .json_body(json!({"items": [track_a(), {"name": "no uri"}], "total": 2}));
            }));
            assert_eq!(
                c.top(TopKind::Tracks, range, 5).unwrap(),
                [offered_a()],
                "{wire}"
            );
        }
        for mock in &sent {
            mock.assert_calls(1);
        }
        let artists = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/top/artists")
                .query_param("time_range", "medium_term")
                .query_param("limit", "50");
            then.status(200).json_body(json!({"items": [
                {"type": "artist", "uri": "spotify:artist:0OdUWJ0sBjDrqHygGUXeCF",
                 "name": "Band of Horses", "genres": ["indie"], "popularity": 60},
                {"type": "artist", "name": "no uri"},
            ]}));
        });
        assert_eq!(
            c.top(TopKind::Artists, TimeRange::Medium, 50).unwrap(),
            [Offered::named(
                "spotify:artist:0OdUWJ0sBjDrqHygGUXeCF",
                "Band of Horses"
            )]
        );
        artists.assert_calls(1);
    }

    #[test]
    fn the_top_and_range_names_serialize_as_documented() {
        assert_eq!(serde_json::to_value(TopKind::Artists).unwrap(), "artists");
        assert_eq!(serde_json::to_value(TimeRange::Short).unwrap(), "short");
        assert_eq!(TimeRange::Long.as_str(), "long");
        assert_eq!(TopKind::Tracks.as_str(), "tracks");
    }

    /// A listing entry for playlist number `n`, owned by alice.
    fn listed(n: usize) -> Value {
        json!({"uri": format!("spotify:playlist:{n:022}"), "name": format!("List {n}"),
               "description": "", "public": true, "collaborative": false,
               "owner": {"id": "alice-spotify", "display_name": "Alice"},
               "items": {"href": "https://api.spotify.com/v1/playlists/x/items", "total": n}})
    }

    #[test]
    fn my_playlists_pages_by_offset_until_next_is_null() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut c = client(tmp.path(), &server, &[]);
        let first: Vec<Value> = (0..50).map(listed).collect();
        let page1 = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/playlists")
                .query_param("limit", "50")
                .query_param("offset", "0");
            then.status(200).json_body(json!({"items": first,
                "next": "https://api.spotify.com/v1/me/playlists?offset=50&limit=50"}));
        });
        let page2 = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/playlists")
                .query_param("limit", "50")
                .query_param("offset", "50");
            then.status(200).json_body(json!({"items": [
                {"uri": "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M", "name": "Shared",
                 "description": "Songs from Bob", "public": null, "collaborative": true,
                 "owner": {"id": "bob-spotify", "display_name": null},
                 "items": {"href": "https://api.spotify.com/v1/playlists/y/items", "total": 7}},
                {"name": "no uri", "owner": {"id": "x"}},
                listed(51),
            ], "next": null}));
        });
        let lists = c.my_playlists().unwrap();
        page1.assert_calls(1);
        page2.assert_calls(1);
        assert_eq!(lists.len(), 52);
        assert_eq!(lists[0].description, None, "an empty description");
        assert_eq!(lists[0].public, Some(true));
        assert_eq!(lists[3].total, Some(3));
        assert_eq!(
            lists[50],
            Playlist {
                id: "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M".into(),
                name: "Shared".into(),
                owner: Owner {
                    id: "bob-spotify".into(),
                    name: "bob-spotify".into()
                },
                public: None,
                collaborative: true,
                total: Some(7),
                description: Some("Songs from Bob".into()),
            }
        );
        assert_eq!(lists[51].name, "List 51");
    }

    #[test]
    fn playlist_sends_its_fields_and_reads_the_total_when_spotify_gives_it() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut c = client(tmp.path(), &server, &[]);
        let owned = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/playlists/{PLAYLIST}"))
                .query_param("fields", PLAYLIST_FIELDS);
            then.status(200).json_body(json!({
                "uri": format!("spotify:playlist:{PLAYLIST}"), "name": "Morning",
                "description": "sss:alice/morning", "public": false, "collaborative": false,
                "owner": {"id": "alice-spotify", "display_name": "Alice"},
                "items": {"href": "https://api.spotify.com/v1/playlists/x/items", "total": 42}}));
        });
        let list = c.playlist(PLAYLIST).unwrap();
        owned.assert_calls(1);
        assert_eq!(list.total, Some(42));
        assert_eq!(list.public, Some(false));
        assert_eq!(list.owner.name, "Alice");
        assert_eq!(list.description.as_deref(), Some("sss:alice/morning"));
        let doc = serde_json::to_value(&list).unwrap();
        assert_eq!(doc["id"], format!("spotify:playlist:{PLAYLIST}"));
        assert_eq!(
            doc["owner"],
            json!({"id": "alice-spotify", "name": "Alice"})
        );

        let other = "0000000000000000000002";
        server.mock(|when, then| {
            when.method(GET).path(format!("/v1/playlists/{other}"));
            then.status(200).json_body(json!({
                "uri": format!("spotify:playlist:{other}"), "name": "Bob's",
                "description": null, "public": true, "collaborative": false,
                "owner": {"id": "bob-spotify", "display_name": "Bob"}}));
        });
        let list = c.playlist(other).unwrap();
        assert_eq!(list.total, None);
        let doc = serde_json::to_value(&list).unwrap();
        assert_eq!(doc["total"], Value::Null, "{doc}");
        assert!(doc.get("description").is_none(), "{doc}");
    }

    #[test]
    fn an_unknown_playlist_is_spotify_s_404() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut c = client(tmp.path(), &server, &[]);
        server.mock(|when, then| {
            when.method(GET).path(format!("/v1/playlists/{PLAYLIST}"));
            then.status(404)
                .json_body(json!({"error": {"status": 404, "message": "Resource not found"}}));
        });
        let err = c.playlist(PLAYLIST).unwrap_err();
        assert_eq!(crate::error::kind(&err), crate::error::Kind::Other);
        assert!(format!("{err:#}").contains("Resource not found"), "{err:#}");
    }

    #[test]
    fn playlist_contents_pages_and_parses_tracks_episodes_and_local_files() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut c = client(tmp.path(), &server, &[]);
        let path = format!("/v1/playlists/{PLAYLIST}/items");
        let filler: Vec<Value> = (0..99).map(|_| json!({"item": track_a()})).collect();
        let mut first = vec![json!({"item": null})];
        first.extend(filler);
        let page1 = server.mock(|when, then| {
            when.method(GET)
                .path(&path)
                .query_param("fields", CONTENTS_FIELDS)
                .query_param("limit", "100")
                .query_param("offset", "0");
            then.status(200)
                .json_body(json!({"items": first, "next": "https://api.spotify.com/next"}));
        });
        let page2 = server.mock(|when, then| {
            when.method(GET)
                .path(&path)
                .query_param("fields", CONTENTS_FIELDS)
                .query_param("limit", "100")
                .query_param("offset", "100");
            then.status(200).json_body(json!({"items": [
                {"item": {"type": "episode", "uri": "spotify:episode:512ojhOuo1ktJprKbVcKyQ",
                          "name": "An Episode"}},
                {"item": {"type": "track", "uri": "spotify:local:Artist:Album:Home+Made:200",
                          "name": "Home Made", "artists": [{"name": "Artist"}]}},
                null,
            ], "next": null}));
        });
        let items = c.playlist_contents(PLAYLIST).unwrap();
        page1.assert_calls(1);
        page2.assert_calls(1);
        assert_eq!(items.len(), 101);
        assert_eq!(items[0], offered_a());
        assert_eq!(
            items[99],
            Offered::named("spotify:episode:512ojhOuo1ktJprKbVcKyQ", "An Episode")
        );
        assert_eq!(items[100].id, "spotify:local:Artist:Album:Home+Made:200");
        assert_eq!(items[100].name, "Home Made");
        assert_eq!(items[100].artists, ["Artist"]);
    }
}
