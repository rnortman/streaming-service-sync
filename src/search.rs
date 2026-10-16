//! `sss search`: free-text Spotify searches, several per invocation, each
//! result in the [`Offered`] shape so its `id` goes straight to `sss match
//! set`, `sss play` or `sss queue`.

use std::borrow::BorrowMut;
use std::io::{self, Write};

use anyhow::{Context as _, Result};
use serde::Serialize;

use crate::error::Kind;
use crate::fail;
use crate::player::clock;
use crate::spotify::client::Spotify;
use crate::spotify::ids::IdKind;
use crate::spotify::item::Offered;

/// What one query found, each kind in Spotify's order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchResult {
    pub query: String,
    pub tracks: Vec<Offered>,
    pub artists: Vec<Offered>,
    pub albums: Vec<Offered>,
}

/// What `sss search` reports: one result per query, in the order given.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchReport {
    pub results: Vec<SearchResult>,
}

/// `sss search`: one request per query for `kinds` (every kind when
/// `None`), at most `limit` results of each. A blank query is refused before
/// anything is sent; a failing request ends the command with its error.
pub fn search<S: BorrowMut<Spotify>>(
    queries: &[String],
    only: Option<IdKind>,
    limit: u32,
    connect: impl FnOnce() -> Result<S>,
) -> Result<SearchReport> {
    if let Some(n) = queries.iter().position(|q| q.trim().is_empty()) {
        fail!(Kind::Input, "search query {} is empty", n + 1);
    }
    let kinds: &[IdKind] = match &only {
        Some(kind) => std::slice::from_ref(kind),
        None => &IdKind::ALL,
    };
    let mut client = connect()?;
    let c = client.borrow_mut();
    let mut results = Vec::with_capacity(queries.len());
    for query in queries {
        let query = query.trim();
        let found = c
            .search(query, kinds, limit)
            .with_context(|| format!("searching for {query:?}"))?;
        results.push(SearchResult {
            query: query.to_owned(),
            tracks: found.tracks.iter().map(Offered::from).collect(),
            artists: found.artists.iter().map(Offered::from).collect(),
            albums: found.albums.iter().map(Offered::from).collect(),
        });
    }
    Ok(SearchReport { results })
}

/// `spotify:track:… "Song" — Artist, Other`: the id, the quoted name and
/// the artists when the item names any.
pub(crate) fn item_head(item: &Offered) -> String {
    let mut line = format!("{} {:?}", item.id, item.name);
    if !item.artists.is_empty() {
        line.push_str(&format!(" — {}", item.artists.join(", ")));
    }
    line
}

/// `spotify:track:… "Song" — Artist — Album (3:45)`: [`item_head`], then
/// whichever of album and duration the item carries.
pub(crate) fn result_line(item: &Offered) -> String {
    let mut line = item_head(item);
    if let Some(album) = &item.album {
        line.push_str(&format!(" — {album}"));
    }
    if let Some(ms) = item.duration_ms {
        line.push_str(&format!(" ({})", clock(ms)));
    }
    line
}

impl SearchReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        for result in &self.results {
            writeln!(out, "{:?}:", result.query)?;
            let items = result
                .tracks
                .iter()
                .chain(&result.artists)
                .chain(&result.albums);
            let mut any = false;
            for item in items {
                any = true;
                writeln!(out, "  {}", result_line(item))?;
            }
            if !any {
                writeln!(out, "  no results")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error;
    use crate::spotify::test_support::client;
    use httpmock::prelude::*;
    use serde_json::json;

    fn track(id: &str, name: &str) -> serde_json::Value {
        json!({"uri": format!("spotify:track:{id}"), "name": name,
               "artists": [{"name": "Panjabi MC"}], "album": {"name": "Legalised"},
               "duration_ms": 225_000})
    }

    fn queries(q: &[&str]) -> Vec<String> {
        q.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn every_query_is_one_request_of_every_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let first = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/search")
                .query_param("q", "Mundian to Bach Ke")
                .query_param("type", "track,artist,album")
                .query_param("limit", "3");
            then.status(200).json_body(json!({
                "tracks": {"items": [track("b", "Mundian To Bach Ke"), track("a", "Mundian")]},
                "artists": {"items": [{"uri": "spotify:artist:p", "name": "Panjabi MC"}]},
                "albums": {"items": [{"uri": "spotify:album:l", "name": "Legalised",
                                      "artists": [{"name": "Panjabi MC"}],
                                      "album_type": "album", "release_date": "1998"}]}
            }));
        });
        let second = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/search")
                .query_param("q", "nothing at all");
            then.status(200).json_body(json!({}));
        });
        let report = search(
            &queries(&[" Mundian to Bach Ke ", "nothing at all"]),
            None,
            3,
            || Ok(client(tmp.path(), &server, &[])),
        )
        .unwrap();
        first.assert();
        second.assert();
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            json!({"results": [
                {"query": "Mundian to Bach Ke",
                 "tracks": [
                    {"id": "spotify:track:b", "name": "Mundian To Bach Ke",
                     "artists": ["Panjabi MC"], "album": "Legalised", "duration_ms": 225000},
                    {"id": "spotify:track:a", "name": "Mundian",
                     "artists": ["Panjabi MC"], "album": "Legalised", "duration_ms": 225000}],
                 "artists": [{"id": "spotify:artist:p", "name": "Panjabi MC"}],
                 "albums": [{"id": "spotify:album:l", "name": "Legalised",
                             "artists": ["Panjabi MC"], "album_type": "album",
                             "release_date": "1998"}]},
                {"query": "nothing at all", "tracks": [], "artists": [], "albums": []}]})
        );
        let mut text = Vec::new();
        report.write_text(&mut text).unwrap();
        assert_eq!(
            String::from_utf8(text).unwrap(),
            "\"Mundian to Bach Ke\":\n  \
             spotify:track:b \"Mundian To Bach Ke\" — Panjabi MC — Legalised (3:45)\n  \
             spotify:track:a \"Mundian\" — Panjabi MC — Legalised (3:45)\n  \
             spotify:artist:p \"Panjabi MC\"\n  \
             spotify:album:l \"Legalised\" — Panjabi MC\n\
             \"nothing at all\":\n  no results\n"
        );
    }

    #[test]
    fn only_one_kind_is_searched_when_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/search")
                .query_param("type", "album")
                .query_param("limit", "10");
            then.status(200).json_body(json!({"albums": {"items": []}}));
        });
        let report = search(&queries(&["x"]), Some(IdKind::Album), 10, || {
            Ok(client(tmp.path(), &server, &[]))
        })
        .unwrap();
        mock.assert();
        assert_eq!(report.results.len(), 1);
    }

    #[test]
    fn a_failing_query_ends_the_command() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/search").query_param("q", "a");
            then.status(200).json_body(json!({}));
        });
        let refused = server.mock(|when, then| {
            when.method(GET).path("/v1/search").query_param("q", "b");
            then.status(400)
                .json_body(json!({"error": {"status": 400, "message": "Bad query"}}));
        });
        let never = server.mock(|when, then| {
            when.method(GET).path("/v1/search").query_param("q", "c");
            then.status(200).json_body(json!({}));
        });
        let err = search(&queries(&["a", "b", "c"]), None, 10, || {
            Ok(client(tmp.path(), &server, &[]))
        })
        .unwrap_err();
        refused.assert();
        never.assert_calls(0);
        assert_eq!(error::kind(&err), Kind::Other);
        let text = format!("{err:#}");
        assert!(text.starts_with("searching for \"b\": "), "{text}");
        assert!(text.contains("Bad query"), "{text}");
    }

    #[test]
    fn a_blank_query_is_refused_before_signing_in() {
        let err = search(
            &queries(&["fine", "  "]),
            None,
            10,
            || -> Result<Spotify> { panic!("connected") },
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert_eq!(format!("{err:#}"), "search query 2 is empty");
    }
}
