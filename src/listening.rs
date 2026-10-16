//! `sss history` and `sss top`: what the profile's member has played
//! lately, and Spotify's ranking of what they play most.

use std::borrow::BorrowMut;
use std::io::{self, Write};

use anyhow::Result;
use serde::Serialize;

use crate::search::item_head;
use crate::spotify::account::{Played, TimeRange, TopKind};
use crate::spotify::auth::{RECENTLY_PLAYED_SCOPE, TOP_READ_SCOPE};
use crate::spotify::client::Spotify;
use crate::spotify::item::Offered;

/// What `sss history` reports: the finished plays, newest first.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryReport {
    pub profile: String,
    pub items: Vec<Played>,
}

/// `sss history`: up to `limit` of the member's last finished plays.
pub fn history<S: BorrowMut<Spotify>>(
    profile: &str,
    limit: u32,
    connect: impl FnOnce() -> Result<S>,
) -> Result<HistoryReport> {
    let mut client = connect()?;
    let c = client.borrow_mut();
    c.require_scopes(profile, &[RECENTLY_PLAYED_SCOPE])?;
    Ok(HistoryReport {
        profile: profile.to_owned(),
        items: c.recently_played(limit)?,
    })
}

/// What `sss top` reports: the ranking, first place first.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TopReport {
    pub profile: String,
    pub kind: TopKind,
    pub range: TimeRange,
    pub items: Vec<Offered>,
}

/// `sss top`: up to `limit` of the member's top `kind` over `range`.
pub fn top<S: BorrowMut<Spotify>>(
    profile: &str,
    kind: TopKind,
    range: TimeRange,
    limit: u32,
    connect: impl FnOnce() -> Result<S>,
) -> Result<TopReport> {
    let mut client = connect()?;
    let c = client.borrow_mut();
    c.require_scopes(profile, &[TOP_READ_SCOPE])?;
    Ok(TopReport {
        profile: profile.to_owned(),
        kind,
        range,
        items: c.top(kind, range, limit)?,
    })
}

/// How long a top-items range reaches back, in words.
fn range_words(range: TimeRange) -> &'static str {
    match range {
        TimeRange::Short => "about the last four weeks",
        TimeRange::Medium => "about the last six months",
        TimeRange::Long => "several years",
    }
}

impl HistoryReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        if self.items.is_empty() {
            return writeln!(out, "no plays recorded");
        }
        for played in &self.items {
            write!(out, "{} {}", played.played_at, item_head(&played.track))?;
            match &played.context {
                Some(context) => writeln!(out, " (from {} {})", context.kind, context.uri)?,
                None => writeln!(out)?,
            }
        }
        Ok(())
    }
}

impl TopReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        writeln!(
            out,
            "top {}, {} range ({})",
            self.kind.as_str(),
            self.range.as_str(),
            range_words(self.range)
        )?;
        if self.items.is_empty() {
            return writeln!(out, "none yet");
        }
        for (n, item) in self.items.iter().enumerate() {
            writeln!(out, "{}. {}", n + 1, item_head(item))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{self, Kind};
    use crate::spotify::playback::Context;
    use crate::spotify::test_support::client;
    use httpmock::prelude::*;
    use serde_json::json;

    const TRACK: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";
    const PLAYLIST: &str = "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M";

    fn song() -> Offered {
        Offered {
            artists: vec!["Artist".into(), "Other".into()],
            album: Some("Album".into()),
            ..Offered::named(TRACK, "Song")
        }
    }

    fn text(write: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> String {
        let mut out = Vec::new();
        write(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn history_checks_its_scope_before_any_request() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut c = client(tmp.path(), &server, &["user-library-read"]);
        let err = history("alice", 50, || Ok(&mut c)).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Auth);
        assert!(format!("{err:#}").contains("without user-read-recently-played"));
        assert_eq!(c.requests(), 0);
    }

    #[test]
    fn history_reads_the_plays_with_its_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let read = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/player/recently-played")
                .query_param("limit", "10");
            then.status(200).json_body(json!({"items": [
                {"played_at": "2026-10-16T13:02:11Z",
                 "track": {"type": "track", "uri": TRACK, "name": "Song",
                           "artists": [{"name": "Artist"}]},
                 "context": {"type": "playlist", "uri": PLAYLIST}}
            ]}));
        });
        let mut c = client(tmp.path(), &server, &[RECENTLY_PLAYED_SCOPE]);
        let report = history("alice", 10, || Ok(&mut c)).unwrap();
        read.assert();
        assert_eq!(report.profile, "alice");
        assert_eq!(report.items.len(), 1);
        assert_eq!(report.items[0].played_at, "2026-10-16T13:02:11Z");
        let doc = serde_json::to_value(&report).unwrap();
        assert_eq!(doc["items"][0]["track"]["id"], TRACK);
        assert_eq!(doc["items"][0]["context"]["type"], "playlist");
    }

    #[test]
    fn history_text_names_the_context_when_there_is_one() {
        let report = HistoryReport {
            profile: "alice".into(),
            items: vec![
                Played {
                    played_at: "2026-10-16T13:02:11Z".into(),
                    track: song(),
                    context: Some(Context {
                        kind: "playlist".into(),
                        uri: PLAYLIST.into(),
                    }),
                },
                Played {
                    played_at: "2026-10-16T12:58:40Z".into(),
                    track: Offered::named(TRACK, "Song"),
                    context: None,
                },
            ],
        };
        assert_eq!(
            text(|o| report.write_text(o)),
            format!(
                "2026-10-16T13:02:11Z {TRACK} \"Song\" — Artist, Other (from playlist {PLAYLIST})\n\
                 2026-10-16T12:58:40Z {TRACK} \"Song\"\n"
            )
        );
        let empty = HistoryReport {
            profile: "alice".into(),
            items: vec![],
        };
        assert_eq!(text(|o| empty.write_text(o)), "no plays recorded\n");
    }

    #[test]
    fn top_checks_its_scope_before_any_request() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut c = client(tmp.path(), &server, &[RECENTLY_PLAYED_SCOPE]);
        let err = top("alice", TopKind::Tracks, TimeRange::Medium, 50, || {
            Ok(&mut c)
        })
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Auth);
        assert!(format!("{err:#}").contains("without user-top-read"));
        assert_eq!(c.requests(), 0);
    }

    #[test]
    fn top_sends_its_kind_range_and_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let read = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/me/top/artists")
                .query_param("time_range", "short_term")
                .query_param("limit", "5");
            then.status(200).json_body(json!({"items": [
                {"uri": "spotify:artist:0OdUWJ0sBjDrqHygGUXeCF", "name": "Band"}
            ]}));
        });
        let mut c = client(tmp.path(), &server, &[TOP_READ_SCOPE]);
        let report = top("alice", TopKind::Artists, TimeRange::Short, 5, || {
            Ok(&mut c)
        })
        .unwrap();
        read.assert();
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            json!({"profile": "alice", "kind": "artists", "range": "short",
                   "items": [{"id": "spotify:artist:0OdUWJ0sBjDrqHygGUXeCF", "name": "Band"}]})
        );
    }

    #[test]
    fn top_text_heads_the_ranking_with_its_range() {
        for (range, words) in [
            (TimeRange::Short, "short range (about the last four weeks)"),
            (
                TimeRange::Medium,
                "medium range (about the last six months)",
            ),
            (TimeRange::Long, "long range (several years)"),
        ] {
            let report = TopReport {
                profile: "alice".into(),
                kind: TopKind::Tracks,
                range,
                items: vec![song()],
            };
            assert_eq!(
                text(|o| report.write_text(o)),
                format!("top tracks, {words}\n1. {TRACK} \"Song\" — Artist, Other\n")
            );
        }
        let empty = TopReport {
            profile: "alice".into(),
            kind: TopKind::Artists,
            range: TimeRange::Long,
            items: vec![],
        };
        assert_eq!(
            text(|o| empty.write_text(o)),
            "top artists, long range (several years)\nnone yet\n"
        );
    }
}
