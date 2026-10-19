//! The playlist writes beside push's create and add: a details change, a
//! removal by URI, a reorder and a replace. Each answers with the
//! playlist's new snapshot id where Spotify gives one.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};

use crate::spotify::client::{Call, ItemsChanged, PLAYLIST_CHUNK, Spotify, WireSnapshot};

/// The details a [`Spotify::playlist_change`] sets; `None` leaves one as
/// it is and is not sent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PlaylistChanges {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collaborative: Option<bool>,
}

impl Spotify {
    /// `PUT playlists/{id}`: sets the details `changes` names.
    pub fn playlist_change(&mut self, playlist_id: &str, changes: &PlaylistChanges) -> Result<()> {
        let path = format!("playlists/{playlist_id}");
        let body = serde_json::to_value(changes).context("serializing the playlist changes")?;
        self.call(&Call {
            body: Some(&body),
            ..Call::new("PUT", &path)
        })?;
        Ok(())
    }

    /// `DELETE playlists/{id}/items` in chunks of [`PLAYLIST_CHUNK`]: every
    /// occurrence of each of `uris` leaves the playlist. The first chunk
    /// carries `snapshot_id`, each later one the snapshot id the chunk
    /// before it returned. `removed` is called after each chunk lands with
    /// its URIs and the new snapshot id; its error stops the writes. A
    /// removal applied twice is the same playlist, so the request is
    /// resent after no answer or a 5xx.
    pub fn playlist_remove_items(
        &mut self,
        playlist_id: &str,
        uris: &[String],
        snapshot_id: Option<&str>,
        removed: &mut ItemsChanged<'_>,
    ) -> Result<()> {
        let path = format!("playlists/{playlist_id}/items");
        let mut snapshot = snapshot_id.map(str::to_owned);
        for chunk in uris.chunks(PLAYLIST_CHUNK) {
            let items: Vec<Value> = chunk.iter().map(|uri| json!({ "uri": uri })).collect();
            let mut body = json!({ "items": items });
            if let Some(s) = &snapshot {
                body["snapshot_id"] = json!(s);
            }
            let wire: Option<WireSnapshot> = self.json_opt(&Call {
                body: Some(&body),
                ..Call::new("DELETE", &path)
            })?;
            snapshot = wire.and_then(|w| w.snapshot_id);
            removed(chunk, snapshot.as_deref())?;
        }
        Ok(())
    }

    /// `PUT playlists/{id}/items` as a reorder, positions 0-based: the
    /// `range_length` items from `range_start` move before the item at
    /// `insert_before` (the playlist's length for the end). A reorder
    /// applied twice moves whatever then sits at `range_start`, so it is
    /// never resent. The new snapshot id, when Spotify gives one.
    pub fn playlist_reorder(
        &mut self,
        playlist_id: &str,
        range_start: u32,
        insert_before: u32,
        range_length: u32,
        snapshot_id: Option<&str>,
    ) -> Result<Option<String>> {
        let path = format!("playlists/{playlist_id}/items");
        let mut body = json!({
            "range_start": range_start,
            "insert_before": insert_before,
            "range_length": range_length,
        });
        if let Some(s) = snapshot_id {
            body["snapshot_id"] = json!(s);
        }
        let wire: Option<WireSnapshot> = self.json_opt(&Call {
            body: Some(&body),
            resend: false,
            ..Call::new("PUT", &path)
        })?;
        Ok(wire.and_then(|w| w.snapshot_id))
    }

    /// `PUT playlists/{id}/items` as a replace: the playlist holds exactly
    /// `uris` (at most [`PLAYLIST_CHUNK`]; the rest are the caller's to
    /// add), and nothing when `uris` is empty. The new snapshot id, when
    /// Spotify gives one.
    pub fn playlist_replace_items(
        &mut self,
        playlist_id: &str,
        uris: &[String],
    ) -> Result<Option<String>> {
        debug_assert!(uris.len() <= PLAYLIST_CHUNK, "a replace takes at most 100");
        let path = format!("playlists/{playlist_id}/items");
        let body = json!({ "uris": uris });
        let wire: Option<WireSnapshot> = self.json_opt(&Call {
            body: Some(&body),
            ..Call::new("PUT", &path)
        })?;
        Ok(wire.and_then(|w| w.snapshot_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{self, Kind};
    use crate::spotify::client::MAX_TRIES;
    use crate::spotify::test_support::client;
    use anyhow::anyhow;
    use httpmock::prelude::*;

    const PL: &str = "37i9dQZF1DXcBWIGoYBM5M";

    fn uris(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("spotify:track:{i}")).collect()
    }

    fn quiet(_: &str) {}

    #[test]
    fn playlist_change_sends_only_the_fields_set() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let renamed = server.mock(|when, then| {
            when.method(PUT)
                .path(format!("/v1/playlists/{PL}"))
                .json_body(json!({"name": "Kids Songs"}));
            then.status(200);
        });
        let all = server.mock(|when, then| {
            when.method(PUT)
                .path(format!("/v1/playlists/{PL}"))
                .json_body(json!({"name": "n", "description": "d", "public": false,
                                  "collaborative": true}));
            then.status(200);
        });
        let mut c = client(tmp.path(), &server, &[]);
        c.playlist_change(
            PL,
            &PlaylistChanges {
                name: Some("Kids Songs".into()),
                ..PlaylistChanges::default()
            },
        )
        .unwrap();
        renamed.assert();
        c.playlist_change(
            PL,
            &PlaylistChanges {
                name: Some("n".into()),
                description: Some("d".into()),
                public: Some(false),
                collaborative: Some(true),
            },
        )
        .unwrap();
        all.assert();
    }

    #[test]
    fn playlist_remove_items_goes_in_hundreds_passing_each_snapshot_on() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let all = uris(230);
        let mut mocks = Vec::new();
        for (n, chunk) in all.chunks(100).enumerate() {
            let items: Vec<Value> = chunk.iter().map(|u| json!({"uri": u})).collect();
            let sent = if n == 0 {
                "read".to_owned()
            } else {
                format!("s{}", n - 1)
            };
            mocks.push(server.mock(move |when, then| {
                when.method(DELETE)
                    .path(format!("/v1/playlists/{PL}/items"))
                    .json_body(json!({"items": items, "snapshot_id": sent}));
                then.status(200)
                    .json_body(json!({"snapshot_id": format!("s{n}")}));
            }));
        }
        let mut c = client(tmp.path(), &server, &[]);
        let mut seen = Vec::new();
        c.playlist_remove_items(PL, &all, Some("read"), &mut |chunk, snap| {
            seen.push((chunk.len(), snap.map(str::to_owned)));
            Ok(())
        })
        .unwrap();
        for m in &mocks {
            m.assert();
        }
        assert_eq!(
            seen,
            [
                (100, Some("s0".into())),
                (100, Some("s1".into())),
                (30, Some("s2".into()))
            ]
        );
    }

    #[test]
    fn playlist_remove_items_without_a_snapshot_sends_none_and_stops_on_the_callback() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let delete = server.mock(|when, then| {
            when.method(DELETE)
                .path(format!("/v1/playlists/{PL}/items"))
                .json_body(json!({"items": [{"uri": "spotify:track:0"}]}));
            then.status(200);
        });
        let mut c = client(tmp.path(), &server, &[]);
        let mut snaps = Vec::new();
        c.playlist_remove_items(PL, &uris(1), None, &mut |_, snap| {
            snaps.push(snap.map(str::to_owned));
            Ok(())
        })
        .unwrap();
        delete.assert();
        assert_eq!(snaps, [None], "an empty answer has no snapshot");

        let err = c
            .playlist_remove_items(PL, &uris(1), None, &mut |_, _| Err(anyhow!("stop")))
            .unwrap_err();
        assert_eq!(err.to_string(), "stop");
    }

    #[test]
    fn a_removal_is_resent_after_a_server_error() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let busy = server.mock(|when, then| {
            when.method(DELETE)
                .path(format!("/v1/playlists/{PL}/items"));
            then.status(503).body("busy");
        });
        let mut c = client(tmp.path(), &server, &[]);
        c.warn = quiet;
        let err = c
            .playlist_remove_items(PL, &uris(1), None, &mut |_, _| Ok(()))
            .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Unavailable);
        busy.assert_calls(MAX_TRIES as usize);
    }

    #[test]
    fn playlist_reorder_sends_the_range_and_snapshot_and_is_never_resent() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut reorder = server.mock(|when, then| {
            when.method(PUT)
                .path(format!("/v1/playlists/{PL}/items"))
                .json_body(
                    json!({"range_start": 0, "insert_before": 5, "range_length": 2,
                                  "snapshot_id": "read"}),
                );
            then.status(200).json_body(json!({"snapshot_id": "after"}));
        });
        let mut c = client(tmp.path(), &server, &[]);
        assert_eq!(
            c.playlist_reorder(PL, 0, 5, 2, Some("read")).unwrap(),
            Some("after".into())
        );
        reorder.assert();
        reorder.delete();

        let busy = server.mock(|when, then| {
            when.method(PUT).path(format!("/v1/playlists/{PL}/items"));
            then.status(503).body("busy");
        });
        let err = c.playlist_reorder(PL, 3, 0, 1, None).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Unavailable);
        assert!(
            format!("{err:#}").ends_with("not resent, since Spotify may have applied it; re-run"),
            "{err:#}"
        );
        busy.assert_calls(1);
    }

    #[test]
    fn playlist_replace_items_puts_the_uris_and_an_empty_list_empties() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let replace = server.mock(|when, then| {
            when.method(PUT)
                .path(format!("/v1/playlists/{PL}/items"))
                .json_body(json!({"uris": ["spotify:track:0", "spotify:track:1"]}));
            then.status(200).json_body(json!({"snapshot_id": "r"}));
        });
        let empty = server.mock(|when, then| {
            when.method(PUT)
                .path(format!("/v1/playlists/{PL}/items"))
                .json_body(json!({"uris": []}));
            then.status(201);
        });
        let mut c = client(tmp.path(), &server, &[]);
        assert_eq!(
            c.playlist_replace_items(PL, &uris(2)).unwrap(),
            Some("r".into())
        );
        replace.assert();
        assert_eq!(c.playlist_replace_items(PL, &[]).unwrap(), None);
        empty.assert();
    }
}
