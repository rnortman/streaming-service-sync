//! The Spotify player endpoints: devices, the player's state and queue,
//! play, pause, skip, queue, shuffle, repeat, volume and seek, plus the
//! track counts a shuffled start needs. Each request is paced and retried by
//! the client's transport; the body of every 2xx to a `PUT` or `POST` here
//! is ignored, whatever it holds (Spotify answers some with a bare snapshot
//! id). A refusal carrying Spotify's `error.reason` is a [`PlayerRefused`].

use std::fmt;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::spotify::client::{Call, FoundTrack, Spotify, WireTrack};
use crate::spotify::item::Offered;

/// A Spotify player the account has open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// Spotify's device type: `Computer`, `Smartphone`, `Speaker`, …
    #[serde(rename = "type")]
    pub kind: String,
    pub is_active: bool,
    /// Spotify accepts no Web API command for it.
    pub is_restricted: bool,
    pub volume_percent: Option<u8>,
}

/// What a playlist, album or artist playback runs in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Context {
    /// `playlist`, `album`, `artist`, …
    #[serde(rename = "type")]
    pub kind: String,
    pub uri: String,
}

/// The player's repeat mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    Off,
    Track,
    Context,
}

impl Repeat {
    /// `off`, `track`, `context`.
    pub fn as_str(self) -> &'static str {
        match self {
            Repeat::Off => "off",
            Repeat::Track => "track",
            Repeat::Context => "context",
        }
    }

    /// The mode Spotify's `repeat_state` names; `off` for one it does not.
    fn from_wire(state: Option<&str>) -> Self {
        match state {
            Some("track") => Repeat::Track,
            Some("context") => Repeat::Context,
            _ => Repeat::Off,
        }
    }
}

/// The player as `GET me/player` reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlayerState {
    pub device: Option<Device>,
    pub is_playing: bool,
    pub item: Option<Offered>,
    pub progress_ms: Option<u64>,
    pub context: Option<Context>,
    pub shuffle: bool,
    pub repeat: Repeat,
}

/// What is playing and what is queued after it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueueState {
    pub currently_playing: Option<Offered>,
    pub queue: Vec<Offered>,
}

/// The body of a `PUT me/player/play`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayBody {
    /// Resume what was playing.
    Resume,
    /// Play these track URIs, in order.
    Uris(Vec<String>),
    /// Play a playlist, album or artist, from track `offset` (0-based)
    /// when given.
    Context { uri: String, offset: Option<u32> },
}

impl PlayBody {
    fn to_json(&self) -> Value {
        match self {
            PlayBody::Resume => json!({}),
            PlayBody::Uris(uris) => json!({ "uris": uris }),
            PlayBody::Context { uri, offset: None } => json!({ "context_uri": uri }),
            PlayBody::Context {
                uri,
                offset: Some(position),
            } => json!({ "context_uri": uri, "offset": { "position": position } }),
        }
    }
}

/// The text Spotify's or `sss`'s refusal gives when no player can take a
/// command and none is open.
pub const NO_PLAYER_TEXT: &str = "no Spotify player is open or active; open the Spotify app, or \
                                  https://open.spotify.com in a browser, then run again";

/// The Spotify player cannot take the command as things stand: Spotify
/// refused it with a player `reason`, or `sss` found no device to send it
/// to. Kind `player`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerRefused {
    /// `<METHOD> <path>` of the refused request, or the command `sss`
    /// refused before sending one.
    pub what: String,
    /// The HTTP status; 0 when `sss` refused before any request.
    pub status: u16,
    /// Spotify's `error.reason` (`NO_ACTIVE_DEVICE`, `PREMIUM_REQUIRED`, …)
    /// or `sss`'s own (`SEVERAL_DEVICES`).
    pub reason: String,
    /// Spotify's `error.message`; empty for `sss`'s own refusals.
    pub message: String,
    /// The open devices, when `sss` read them before refusing.
    pub devices: Vec<Device>,
}

/// The reason of a refusal because no player can take the command: Spotify's
/// own, and `sss`'s when no open device is unrestricted.
pub const NO_ACTIVE_DEVICE: &str = "NO_ACTIVE_DEVICE";
/// `sss`'s reason when several unrestricted players are open and none is
/// active.
pub const SEVERAL_DEVICES: &str = "SEVERAL_DEVICES";
/// Spotify's reason when the account lacks Premium.
const PREMIUM_REQUIRED: &str = "PREMIUM_REQUIRED";

/// The devices' names, comma-separated.
pub(crate) fn device_names(devices: &[Device]) -> String {
    devices
        .iter()
        .map(|d| d.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

impl fmt::Display for PlayerRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.what)?;
        match self.reason.as_str() {
            NO_ACTIVE_DEVICE if !self.devices.is_empty() => write!(
                f,
                "no active Spotify player; the open ones ({}) cannot be controlled through the \
                 Web API — start playback on one of them, or open the Spotify app or \
                 https://open.spotify.com, then run again",
                device_names(&self.devices)
            ),
            NO_ACTIVE_DEVICE => f.write_str(NO_PLAYER_TEXT),
            SEVERAL_DEVICES => write!(
                f,
                "several Spotify players are open and none is active ({}); pass --device NAME",
                device_names(&self.devices)
            ),
            PREMIUM_REQUIRED => f.write_str(
                "Spotify Premium is required for playback control and the account signed in to \
                 this profile does not have it",
            ),
            reason if self.message.is_empty() => write!(f, "Spotify refused it ({reason})"),
            reason => write!(f, "{} ({reason})", self.message),
        }
    }
}

impl std::error::Error for PlayerRefused {}

// The wire types are lenient: an entry that does not parse is
// dropped rather than failing the response.

#[derive(Deserialize)]
struct WireDevice {
    id: Option<String>,
    name: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    is_active: bool,
    #[serde(default)]
    is_restricted: bool,
    volume_percent: Option<u8>,
}

impl WireDevice {
    /// The device, unless it lacks an id or a name.
    fn device(self) -> Option<Device> {
        Some(Device {
            id: self.id.filter(|s| !s.is_empty())?,
            name: self.name?,
            kind: self.kind.unwrap_or_default(),
            is_active: self.is_active,
            is_restricted: self.is_restricted,
            volume_percent: self.volume_percent,
        })
    }
}

/// The device in `v`, when it parses and has an id and a name.
fn device_of(v: Value) -> Option<Device> {
    serde_json::from_value::<WireDevice>(v).ok()?.device()
}

#[derive(Deserialize)]
struct WireDevices {
    #[serde(default)]
    devices: Vec<Value>,
}

#[derive(Deserialize)]
struct WireEpisode {
    uri: String,
    name: String,
}

/// A playing or queued item as [`Offered`]: a track with its fields, an
/// episode with its `id` and `name` alone; `None` when it parses as
/// neither.
fn item_of(v: Value) -> Option<Offered> {
    if v.get("type").and_then(Value::as_str) == Some("episode") {
        let e: WireEpisode = serde_json::from_value(v).ok()?;
        return Some(Offered::named(e.uri, e.name));
    }
    let t: WireTrack = serde_json::from_value(v).ok()?;
    Some(Offered::from(&FoundTrack::from(t)))
}

#[derive(Deserialize)]
struct WireContext {
    #[serde(rename = "type")]
    kind: Option<String>,
    uri: Option<String>,
}

#[derive(Deserialize)]
struct WirePlayer {
    device: Option<Value>,
    #[serde(default)]
    is_playing: bool,
    item: Option<Value>,
    progress_ms: Option<u64>,
    context: Option<WireContext>,
    #[serde(default)]
    shuffle_state: bool,
    repeat_state: Option<String>,
}

impl From<WirePlayer> for PlayerState {
    fn from(w: WirePlayer) -> Self {
        PlayerState {
            device: w.device.and_then(device_of),
            is_playing: w.is_playing,
            item: w.item.and_then(item_of),
            progress_ms: w.progress_ms,
            context: w.context.and_then(|c| {
                Some(Context {
                    kind: c.kind.unwrap_or_default(),
                    uri: c.uri?,
                })
            }),
            shuffle: w.shuffle_state,
            repeat: Repeat::from_wire(w.repeat_state.as_deref()),
        }
    }
}

#[derive(Deserialize)]
struct WireQueue {
    currently_playing: Option<Value>,
    #[serde(default)]
    queue: Vec<Value>,
}

#[derive(Deserialize)]
struct WireTotal {
    total: u32,
}

#[derive(Deserialize)]
struct WireTotalTracks {
    total_tracks: u32,
}

impl Spotify {
    /// `GET me/player/devices`: the open players; an entry without an id
    /// or a name is dropped.
    pub fn devices(&mut self) -> Result<Vec<Device>> {
        let wire: WireDevices = self.json(&player_call("GET", "me/player/devices", &[], None))?;
        Ok(wire.devices.into_iter().filter_map(device_of).collect())
    }

    /// `GET me/player`: the player's state; `None` when no player is
    /// active (Spotify answers 204).
    pub fn player_state(&mut self) -> Result<Option<PlayerState>> {
        let wire: Option<WirePlayer> = self.json_opt(&player_call(
            "GET",
            "me/player",
            &[("additional_types", "track,episode")],
            None,
        ))?;
        Ok(wire.map(PlayerState::from))
    }

    /// `GET me/player/queue`: what is playing and what is queued.
    pub fn queue_state(&mut self) -> Result<QueueState> {
        let wire: Option<WireQueue> =
            self.json_opt(&player_call("GET", "me/player/queue", &[], None))?;
        let Some(wire) = wire else {
            return Ok(QueueState {
                currently_playing: None,
                queue: Vec::new(),
            });
        };
        Ok(QueueState {
            currently_playing: wire.currently_playing.and_then(item_of),
            queue: wire.queue.into_iter().filter_map(item_of).collect(),
        })
    }

    /// `PUT me/player/play` on `device`, or the active player when `None`.
    pub fn play(&mut self, device: Option<&str>, body: &PlayBody) -> Result<()> {
        let body = body.to_json();
        self.control("PUT", "me/player/play", device, &[], Some(&body))
    }

    /// `PUT me/player/pause`.
    pub fn pause(&mut self, device: Option<&str>) -> Result<()> {
        self.control("PUT", "me/player/pause", device, &[], None)
    }

    /// `POST me/player/next`; not resent after a 5xx or no answer.
    pub fn next(&mut self, device: Option<&str>) -> Result<()> {
        self.control("POST", "me/player/next", device, &[], None)
    }

    /// `POST me/player/previous`; not resent after a 5xx or no answer.
    pub fn previous(&mut self, device: Option<&str>) -> Result<()> {
        self.control("POST", "me/player/previous", device, &[], None)
    }

    /// `POST me/player/queue`: one item after what is queued; not resent
    /// after a 5xx or no answer.
    pub fn add_to_queue(&mut self, device: Option<&str>, uri: &str) -> Result<()> {
        self.control("POST", "me/player/queue", device, &[("uri", uri)], None)
    }

    /// `PUT me/player/shuffle`.
    pub fn set_shuffle(&mut self, device: Option<&str>, on: bool) -> Result<()> {
        let state = if on { "true" } else { "false" };
        self.control(
            "PUT",
            "me/player/shuffle",
            device,
            &[("state", state)],
            None,
        )
    }

    /// `PUT me/player/repeat`.
    pub fn set_repeat(&mut self, device: Option<&str>, mode: Repeat) -> Result<()> {
        self.control(
            "PUT",
            "me/player/repeat",
            device,
            &[("state", mode.as_str())],
            None,
        )
    }

    /// `PUT me/player/volume`, 0–100.
    pub fn set_volume(&mut self, device: Option<&str>, percent: u8) -> Result<()> {
        let percent = percent.to_string();
        self.control(
            "PUT",
            "me/player/volume",
            device,
            &[("volume_percent", &percent)],
            None,
        )
    }

    /// `PUT me/player/seek` to `position_ms` into the playing item.
    pub fn seek(&mut self, device: Option<&str>, position_ms: u64) -> Result<()> {
        let position = position_ms.to_string();
        self.control(
            "PUT",
            "me/player/seek",
            device,
            &[("position_ms", &position)],
            None,
        )
    }

    /// `GET playlists/{id}/items`: the playlist's track count, from a
    /// one-item page. Not a player endpoint: its refusals route as any
    /// other request's.
    pub fn playlist_track_count(&mut self, playlist_id: &str) -> Result<u32> {
        let path = format!("playlists/{playlist_id}/items");
        let wire: WireTotal = self.json(&Call {
            method: "GET",
            path: &path,
            query: &[("fields", "total"), ("limit", "1")],
            body: None,
            player: false,
        })?;
        Ok(wire.total)
    }

    /// `GET albums/{id}`: the album's track count. Not a player endpoint:
    /// its refusals route as any other request's.
    pub fn album_track_count(&mut self, album_id: &str) -> Result<u32> {
        let path = format!("albums/{album_id}");
        let wire: WireTotalTracks = self.json(&Call {
            method: "GET",
            path: &path,
            query: &[],
            body: None,
            player: false,
        })?;
        Ok(wire.total_tracks)
    }

    /// Sends a player command with `query`, plus `device_id` when `device`
    /// is given; the 2xx body is ignored.
    fn control(
        &mut self,
        method: &str,
        path: &str,
        device: Option<&str>,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<()> {
        let mut query = query.to_vec();
        if let Some(id) = device {
            query.push(("device_id", id));
        }
        self.call(&player_call(method, path, &query, body))?;
        Ok(())
    }
}

/// A request to a player endpoint.
fn player_call<'a>(
    method: &'a str,
    path: &'a str,
    query: &'a [(&'a str, &'a str)],
    body: Option<&'a Value>,
) -> Call<'a> {
    Call {
        method,
        path,
        query,
        body,
        player: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{self, Kind};
    use crate::spotify::auth::{PLAYBACK_MODIFY_SCOPE, PLAYBACK_READ_SCOPE};
    use crate::spotify::client::http_status;
    use crate::spotify::test_support::client;
    use httpmock::prelude::*;

    const BOTH: &[&str] = &[PLAYBACK_READ_SCOPE, PLAYBACK_MODIFY_SCOPE];

    const TRACK: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";

    fn device(id: &str, name: &str, active: bool) -> Value {
        json!({"id": id, "name": name, "type": "Computer", "is_active": active,
               "is_restricted": false, "is_private_session": false,
               "volume_percent": 80, "supports_volume": true})
    }

    fn wire_track() -> Value {
        json!({"type": "track", "uri": TRACK, "name": "Song",
               "artists": [{"name": "Band"}], "album": {"name": "LP"},
               "duration_ms": 245000, "explicit": false})
    }

    #[test]
    fn devices_drops_entries_without_an_id_or_a_name() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/player/devices")
                .header("Authorization", "Bearer access-secret");
            then.status(200).json_body(json!({"devices": [
                device("d1", "Firefox", false),
                {"id": null, "name": "Kitchen", "is_restricted": true},
                {"id": "d3"},
                "junk"
            ]}));
        });
        let mut c = client(tmp.path(), &server, BOTH);
        assert_eq!(
            c.devices().unwrap(),
            [Device {
                id: "d1".into(),
                name: "Firefox".into(),
                kind: "Computer".into(),
                is_active: false,
                is_restricted: false,
                volume_percent: Some(80),
            }]
        );
        mock.assert();
    }

    #[test]
    fn player_state_parses_the_documented_object_and_none_on_204() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut state = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/player")
                .query_param("additional_types", "track,episode");
            then.status(200).json_body(json!({
                "device": device("d1", "Firefox", true),
                "repeat_state": "context", "shuffle_state": true,
                "context": {"type": "playlist", "uri": "spotify:playlist:p",
                            "href": "x", "external_urls": {}},
                "timestamp": 1, "progress_ms": 83000, "is_playing": true,
                "item": wire_track(), "currently_playing_type": "track",
                "actions": {"disallows": {}}
            }));
        });
        let mut c = client(tmp.path(), &server, BOTH);
        let got = c.player_state().unwrap().unwrap();
        assert!(got.is_playing && got.shuffle);
        assert_eq!(got.repeat, Repeat::Context);
        assert_eq!(got.progress_ms, Some(83_000));
        assert_eq!(got.device.as_ref().unwrap().name, "Firefox");
        assert_eq!(
            got.context,
            Some(Context {
                kind: "playlist".into(),
                uri: "spotify:playlist:p".into()
            })
        );
        assert_eq!(
            serde_json::to_value(&got).unwrap()["item"],
            json!({"id": TRACK, "name": "Song", "artists": ["Band"], "album": "LP",
                   "duration_ms": 245000, "explicit": false})
        );
        state.assert();
        state.delete();

        let mut episode = server.mock(|when, then| {
            when.method(GET).path("/v1/me/player");
            then.status(200).json_body(json!({
                "is_playing": false, "repeat_state": "off",
                "item": {"type": "episode", "uri": "spotify:episode:e", "name": "Ep 1",
                         "duration_ms": 1, "show": {"name": "Show"}}
            }));
        });
        let got = c.player_state().unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&got.item).unwrap(),
            json!({"id": "spotify:episode:e", "name": "Ep 1"})
        );
        assert_eq!(got.device, None);
        assert_eq!(got.repeat, Repeat::Off);
        episode.delete();

        server.mock(|when, then| {
            when.method(GET).path("/v1/me/player");
            then.status(204);
        });
        assert_eq!(c.player_state().unwrap(), None);
    }

    #[test]
    fn queue_state_offers_the_playing_item_and_the_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/me/player/queue");
            then.status(200).json_body(json!({
                "currently_playing": wire_track(),
                "queue": [wire_track(), {"no": "uri"},
                          {"type": "episode", "uri": "spotify:episode:e", "name": "Ep"}]
            }));
        });
        let mut c = client(tmp.path(), &server, BOTH);
        let q = c.queue_state().unwrap();
        assert_eq!(q.currently_playing.as_ref().unwrap().id, TRACK);
        assert_eq!(q.queue.len(), 2);
        assert_eq!(q.queue[0].artists, ["Band"]);
        assert_eq!(q.queue[1], Offered::named("spotify:episode:e", "Ep"));
    }

    #[test]
    fn play_sends_each_body_and_the_device_only_when_given() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let resume = server.mock(|when, then| {
            when.method(PUT)
                .path("/v1/me/player/play")
                .query_param_missing("device_id")
                .json_body(json!({}));
            then.status(204);
        });
        let uris = server.mock(|when, then| {
            when.method(PUT)
                .path("/v1/me/player/play")
                .query_param("device_id", "d1")
                .json_body(json!({"uris": [TRACK, TRACK]}));
            then.status(202).body("snapshot123");
        });
        let context = server.mock(|when, then| {
            when.method(PUT)
                .path("/v1/me/player/play")
                .json_body(json!({"context_uri": "spotify:playlist:p"}));
            then.status(204);
        });
        let offset = server.mock(|when, then| {
            when.method(PUT)
                .path("/v1/me/player/play")
                .json_body(json!({"context_uri": "spotify:album:a",
                                  "offset": {"position": 37}}));
            then.status(204);
        });
        let mut c = client(tmp.path(), &server, BOTH);
        c.play(None, &PlayBody::Resume).unwrap();
        c.play(
            Some("d1"),
            &PlayBody::Uris(vec![TRACK.into(), TRACK.into()]),
        )
        .unwrap();
        c.play(
            None,
            &PlayBody::Context {
                uri: "spotify:playlist:p".into(),
                offset: None,
            },
        )
        .unwrap();
        c.play(
            None,
            &PlayBody::Context {
                uri: "spotify:album:a".into(),
                offset: Some(37),
            },
        )
        .unwrap();
        for m in [&resume, &uris, &context, &offset] {
            m.assert();
        }
    }

    #[test]
    fn the_controls_send_their_paths_and_queries() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mocks = [
            server.mock(|when, then| {
                when.method(PUT)
                    .path("/v1/me/player/pause")
                    .query_param("device_id", "d1")
                    .header("content-length", "0");
                then.status(204);
            }),
            server.mock(|when, then| {
                when.method(POST)
                    .path("/v1/me/player/next")
                    .query_param_missing("device_id")
                    .header("content-length", "0");
                then.status(204);
            }),
            server.mock(|when, then| {
                when.method(POST).path("/v1/me/player/previous");
                then.status(204);
            }),
            server.mock(|when, then| {
                when.method(POST)
                    .path("/v1/me/player/queue")
                    .query_param("uri", TRACK)
                    .query_param("device_id", "d1");
                then.status(200).body("snapshot123");
            }),
            server.mock(|when, then| {
                when.method(PUT)
                    .path("/v1/me/player/shuffle")
                    .query_param("state", "true");
                then.status(200).body("snapshot123");
            }),
            server.mock(|when, then| {
                when.method(PUT)
                    .path("/v1/me/player/shuffle")
                    .query_param("state", "false");
                then.status(204);
            }),
            server.mock(|when, then| {
                when.method(PUT)
                    .path("/v1/me/player/repeat")
                    .query_param("state", "track");
                then.status(204);
            }),
            server.mock(|when, then| {
                when.method(PUT)
                    .path("/v1/me/player/volume")
                    .query_param("volume_percent", "50");
                then.status(204);
            }),
            server.mock(|when, then| {
                when.method(PUT)
                    .path("/v1/me/player/seek")
                    .query_param("position_ms", "30000");
                then.status(204);
            }),
        ];
        let mut c = client(tmp.path(), &server, BOTH);
        c.pause(Some("d1")).unwrap();
        c.next(None).unwrap();
        c.previous(None).unwrap();
        c.add_to_queue(Some("d1"), TRACK).unwrap();
        c.set_shuffle(None, true).unwrap();
        c.set_shuffle(None, false).unwrap();
        c.set_repeat(None, Repeat::Track).unwrap();
        c.set_volume(None, 50).unwrap();
        c.seek(None, 30_000).unwrap();
        for m in &mocks {
            m.assert();
        }
    }

    #[test]
    fn track_counts_read_one_field() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let playlist = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/playlists/p1/items")
                .query_param("fields", "total")
                .query_param("limit", "1");
            then.status(200).json_body(json!({"total": 40}));
        });
        let album = server.mock(|when, then| {
            when.method(GET).path("/v1/albums/a1");
            then.status(200)
                .json_body(json!({"name": "LP", "total_tracks": 12}));
        });
        let mut c = client(tmp.path(), &server, BOTH);
        assert_eq!(c.playlist_track_count("p1").unwrap(), 40);
        assert_eq!(c.album_track_count("a1").unwrap(), 12);
        playlist.assert();
        album.assert();
    }

    #[test]
    fn a_skip_or_a_queue_add_is_not_resent_after_a_server_error() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let next = server.mock(|when, then| {
            when.method(POST).path("/v1/me/player/next");
            then.status(503);
        });
        let queue = server.mock(|when, then| {
            when.method(POST).path("/v1/me/player/queue");
            then.status(502);
        });
        let mut c = client(tmp.path(), &server, BOTH);
        let err = c.next(None).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Unavailable);
        assert_eq!(
            format!("{err:#}"),
            "POST me/player/next: HTTP 503; not resent, since Spotify may have applied it; \
             run `sss player` to see whether it did before running the command again"
        );
        let err = c.add_to_queue(None, TRACK).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Unavailable);
        assert!(format!("{err:#}").contains("run `sss player` to see whether it did"));
        next.assert_calls(1);
        queue.assert_calls(1);
    }

    fn refusal(status: u16, reason: &str, message: &str) -> Value {
        json!({"error": {"status": status, "message": message, "reason": reason}})
    }

    #[test]
    fn player_refusals_carry_their_reason_and_text() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(PUT).path("/v1/me/player/play");
            then.status(404).json_body(refusal(
                404,
                "NO_ACTIVE_DEVICE",
                "Player command failed: No active device found",
            ));
        });
        server.mock(|when, then| {
            when.method(PUT).path("/v1/me/player/pause");
            then.status(403).json_body(refusal(
                403,
                "PREMIUM_REQUIRED",
                "Player command failed: Premium required",
            ));
        });
        server.mock(|when, then| {
            when.method(POST).path("/v1/me/player/next");
            then.status(403).json_body(refusal(
                403,
                "RESTRICTION_VIOLATED",
                "Player command failed: Restriction violated",
            ));
        });
        let mut c = client(tmp.path(), &server, BOTH);

        let err = c.play(None, &PlayBody::Resume).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Player);
        let refused = err.downcast_ref::<PlayerRefused>().unwrap();
        assert_eq!(
            (refused.status, refused.reason.as_str()),
            (404, "NO_ACTIVE_DEVICE")
        );
        assert_eq!(
            format!("{err:#}"),
            format!("PUT me/player/play: {NO_PLAYER_TEXT}")
        );

        let err = c.pause(None).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Player);
        assert_eq!(
            format!("{err:#}"),
            "PUT me/player/pause: Spotify Premium is required for playback control and the \
             account signed in to this profile does not have it"
        );

        let err = c.next(None).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Player);
        assert_eq!(
            format!("{err:#}"),
            "POST me/player/next: Player command failed: Restriction violated \
             (RESTRICTION_VIOLATED)"
        );
    }

    #[test]
    fn refusals_without_a_reason_and_429s_route_as_before() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(PUT).path("/v1/me/player/pause");
            then.status(403)
                .json_body(json!({"error": {"status": 403, "message": "Forbidden"}}));
        });
        server.mock(|when, then| {
            when.method(PUT).path("/v1/me/player/play");
            then.status(404)
                .json_body(json!({"error": {"status": 404, "message": "Not found"}}));
        });
        server.mock(|when, then| {
            when.method(PUT).path("/v1/me/player/seek");
            then.status(429)
                .header("Retry-After", "3600")
                .json_body(refusal(429, "RATE_LIMITED", "Too many requests"));
        });
        let mut c = client(tmp.path(), &server, BOTH);
        assert_eq!(error::kind(&c.pause(None).unwrap_err()), Kind::Config);
        let err = c.play(None, &PlayBody::Resume).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Other);
        assert_eq!(http_status(&err), Some(404));
        assert_eq!(error::kind(&c.seek(None, 1).unwrap_err()), Kind::Quota);
    }

    #[test]
    fn a_count_read_is_not_a_player_call() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/playlists/p1/items");
            then.status(403)
                .json_body(refusal(403, "SOME_REASON", "Forbidden"));
        });
        server.mock(|when, then| {
            when.method(GET).path("/v1/albums/a1");
            then.status(404)
                .json_body(refusal(404, "SOME_REASON", "Not found"));
        });
        let mut c = client(tmp.path(), &server, BOTH);
        assert_eq!(
            error::kind(&c.playlist_track_count("p1").unwrap_err()),
            Kind::Config
        );
        let err = c.album_track_count("a1").unwrap_err();
        assert_eq!(error::kind(&err), Kind::Other);
        assert_eq!(http_status(&err), Some(404));
    }

    #[test]
    fn sss_own_refusals_name_the_devices() {
        let firefox = Device {
            id: "d1".into(),
            name: "Firefox".into(),
            kind: "Computer".into(),
            is_active: false,
            is_restricted: false,
            volume_percent: None,
        };
        let kitchen = Device {
            id: "d2".into(),
            name: "Kitchen".into(),
            kind: "Speaker".into(),
            is_restricted: true,
            ..firefox.clone()
        };
        let refused = |reason: &str, devices: Vec<Device>| PlayerRefused {
            what: "play".into(),
            status: 0,
            reason: reason.into(),
            message: String::new(),
            devices,
        };
        assert_eq!(
            refused("NO_ACTIVE_DEVICE", vec![]).to_string(),
            format!("play: {NO_PLAYER_TEXT}")
        );
        assert_eq!(
            refused("NO_ACTIVE_DEVICE", vec![kitchen.clone()]).to_string(),
            "play: no active Spotify player; the open ones (Kitchen) cannot be controlled \
             through the Web API — start playback on one of them, or open the Spotify app or \
             https://open.spotify.com, then run again"
        );
        assert_eq!(
            refused("SEVERAL_DEVICES", vec![firefox, kitchen]).to_string(),
            "play: several Spotify players are open and none is active (Firefox, Kitchen); \
             pass --device NAME"
        );
        assert_eq!(
            refused("DEVICE_NOT_CONTROLLABLE", vec![]).to_string(),
            "play: Spotify refused it (DEVICE_NOT_CONTROLLABLE)"
        );
    }

    #[test]
    fn a_device_serializes_its_kind_as_type() {
        let d = device_of(device("d1", "Firefox", true)).unwrap();
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!({"id": "d1", "name": "Firefox", "type": "Computer", "is_active": true,
                   "is_restricted": false, "volume_percent": 80})
        );
        assert_eq!(
            serde_json::to_value(Repeat::Context).unwrap(),
            json!("context")
        );
    }
}
