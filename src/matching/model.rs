//! Match entries and the `matches/*.jsonl` files that hold them.
//!
//! Each file is append-only; the latest entry per `item_id` is the
//! current decision for that item.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::repo::{self, ProfilePaths};
use crate::spotify::ids::IdKind;

/// One value per kind of item matched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ByKind<T> {
    pub tracks: T,
    pub artists: T,
    pub albums: T,
}

impl<T> ByKind<T> {
    pub fn of(&self, kind: IdKind) -> &T {
        match kind {
            IdKind::Track => &self.tracks,
            IdKind::Artist => &self.artists,
            IdKind::Album => &self.albums,
        }
    }

    pub fn of_mut(&mut self, kind: IdKind) -> &mut T {
        match kind {
            IdKind::Track => &mut self.tracks,
            IdKind::Artist => &mut self.artists,
            IdKind::Album => &mut self.albums,
        }
    }
}

/// The match file of `kind`.
pub fn file(paths: &ProfilePaths, kind: IdKind) -> PathBuf {
    match kind {
        IdKind::Track => paths.track_matches(),
        IdKind::Artist => paths.artist_matches(),
        IdKind::Album => paths.album_matches(),
    }
}

/// Where an item stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Matched,
    /// No candidate qualified, or none was found.
    Unmatched,
    /// The deterministic step could not decide and the LLM has not.
    Ambiguous,
    LlmFailed,
    /// Set by hand: the item has no Spotify counterpart.
    Rejected,
}

impl Status {
    /// Every variant, in declaration order.
    pub const ALL: [Status; 5] = [
        Status::Matched,
        Status::Unmatched,
        Status::Ambiguous,
        Status::LlmFailed,
        Status::Rejected,
    ];

    /// The JSON spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Matched => "matched",
            Status::Unmatched => "unmatched",
            Status::Ambiguous => "ambiguous",
            Status::LlmFailed => "llm_failed",
            Status::Rejected => "rejected",
        }
    }
}

/// How a decision was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Isrc,
    Exact,
    Llm,
    Manual,
    /// Decided outside `sss` and submitted with `sss match answer`.
    External,
}

impl Method {
    /// Every variant, in declaration order.
    pub const ALL: [Method; 5] = [
        Method::Isrc,
        Method::Exact,
        Method::Llm,
        Method::Manual,
        Method::External,
    ];

    /// The JSON spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Isrc => "isrc",
            Method::Exact => "exact",
            Method::Llm => "llm",
            Method::Manual => "manual",
            Method::External => "external",
        }
    }
}

/// How sure a decision is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    pub const ALL: [Confidence; 3] = [Confidence::High, Confidence::Medium, Confidence::Low];

    /// `high`, `medium`, `low`: the JSON spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
    }
}

/// One Spotify entity offered for an item, with its score. Tracks carry
/// every field; artists only `uri`, `name` and `score`; albums add
/// `artists`, `album_type` and `release_date`. Fields a kind does not carry
/// are left out of the JSON.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub uri: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artists: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explicit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
    pub score: f64,
}

/// One line of a `matches/*.jsonl` file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchEntry {
    /// The source item id (`TR:…`, `YV:…`, …); files written before YouTube
    /// items existed call it `pandora_id`.
    #[serde(alias = "pandora_id")]
    pub item_id: String,
    pub status: Status,
    #[serde(default)]
    pub spotify_uri: Option<String>,
    #[serde(default)]
    pub method: Option<Method>,
    #[serde(default)]
    pub confidence: Option<Confidence>,
    /// The scored candidates offered, best first, at most ten.
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    /// The search strings run.
    #[serde(default)]
    pub queries: Vec<String>,
    pub decided_at: String,
    /// The ccc session id of an `llm` decision; `None` for an `external` one.
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

impl MatchEntry {
    /// A hand-set match: `matched`, method `manual`, confidence `high`.
    pub fn manual_set(item_id: &str, spotify_uri: &str, decided_at: String) -> Self {
        MatchEntry {
            item_id: item_id.to_owned(),
            status: Status::Matched,
            spotify_uri: Some(spotify_uri.to_owned()),
            method: Some(Method::Manual),
            confidence: Some(Confidence::High),
            candidates: Vec::new(),
            queries: Vec::new(),
            decided_at,
            session: None,
            reason: None,
        }
    }

    /// A hand rejection: `rejected`, method `manual`.
    pub fn manual_reject(item_id: &str, decided_at: String) -> Self {
        MatchEntry {
            item_id: item_id.to_owned(),
            status: Status::Rejected,
            spotify_uri: None,
            method: Some(Method::Manual),
            confidence: None,
            candidates: Vec::new(),
            queries: Vec::new(),
            decided_at,
            session: None,
            reason: None,
        }
    }

    /// Whether a `match` run leaves this item alone: it is decided
    /// (`matched`, `unmatched`, `rejected`) or was decided by hand.
    pub fn is_settled(&self) -> bool {
        self.method == Some(Method::Manual)
            || matches!(
                self.status,
                Status::Matched | Status::Unmatched | Status::Rejected
            )
    }

    /// Whether the item goes back to the LLM from its stored candidates.
    pub fn awaits_llm(&self) -> bool {
        !self.is_settled() && matches!(self.status, Status::Ambiguous | Status::LlmFailed)
    }

    /// The Spotify URI this entry settles on, if it is a match.
    pub fn matched_uri(&self) -> Option<&str> {
        match self.status {
            Status::Matched => self.spotify_uri.as_deref(),
            _ => None,
        }
    }
}

/// The latest entry per `item_id` in the match file at `path`; an absent
/// file holds none.
pub fn read_latest(path: &Path) -> Result<BTreeMap<String, MatchEntry>> {
    repo::read_jsonl_latest(path, |e: &MatchEntry| e.item_id.clone())
}

/// The latest entry per `item_id` of every kind's match file.
pub fn read_all_latest(paths: &ProfilePaths) -> Result<ByKind<BTreeMap<String, MatchEntry>>> {
    let mut all = ByKind::default();
    for kind in IdKind::ALL {
        *all.of_mut(kind) = read_latest(&file(paths, kind))?;
    }
    Ok(all)
}

/// Appends `entry` to the match file at `path`.
pub fn append(path: &Path, entry: &MatchEntry) -> Result<()> {
    repo::append_jsonl(path, entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: &str = "2026-10-15T12:00:00Z";

    fn entry(id: &str, status: Status, method: Option<Method>) -> MatchEntry {
        MatchEntry {
            item_id: id.into(),
            status,
            spotify_uri: None,
            method,
            confidence: None,
            candidates: Vec::new(),
            queries: Vec::new(),
            decided_at: AT.into(),
            session: None,
            reason: None,
        }
    }

    #[test]
    fn an_entry_serializes_in_the_documented_shape() {
        let e = MatchEntry {
            item_id: "TR:1".into(),
            status: Status::Matched,
            spotify_uri: Some("spotify:track:a".into()),
            method: Some(Method::Isrc),
            confidence: Some(Confidence::High),
            candidates: vec![Candidate {
                uri: "spotify:track:a".into(),
                name: "Song".into(),
                artists: vec!["Artist".into()],
                album: Some("Album".into()),
                album_type: Some("album".into()),
                release_date: Some("2004-03-01".into()),
                duration_ms: Some(217_000),
                explicit: Some(false),
                isrc: Some("USAAA0400001".into()),
                score: 0.91,
            }],
            queries: vec!["isrc:USAAA0400001".into()],
            decided_at: AT.into(),
            session: None,
            reason: None,
        };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "item_id": "TR:1", "status": "matched",
                "spotify_uri": "spotify:track:a", "method": "isrc",
                "confidence": "high",
                "candidates": [{"uri": "spotify:track:a", "name": "Song",
                    "artists": ["Artist"], "album": "Album", "album_type": "album",
                    "release_date": "2004-03-01", "duration_ms": 217000,
                    "explicit": false, "isrc": "USAAA0400001", "score": 0.91}],
                "queries": ["isrc:USAAA0400001"], "decided_at": AT,
                "session": null, "reason": null
            })
        );
        let back: MatchEntry = serde_json::from_value(json).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn an_artist_candidate_carries_only_uri_name_and_score() {
        let c = Candidate {
            uri: "spotify:artist:a".into(),
            name: "Artist".into(),
            score: 1.0,
            ..Candidate::default()
        };
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            serde_json::json!({"uri": "spotify:artist:a", "name": "Artist", "score": 1.0})
        );
    }

    #[test]
    fn status_and_method_use_snake_case() {
        assert_eq!(
            serde_json::to_value(Status::LlmFailed).unwrap(),
            serde_json::json!("llm_failed")
        );
        assert_eq!(
            serde_json::to_value(Method::Manual).unwrap(),
            serde_json::json!("manual")
        );
        assert_eq!(
            serde_json::to_value(Method::External).unwrap(),
            serde_json::json!("external")
        );
        assert!(Method::ALL.contains(&Method::External));
        assert!(serde_json::from_value::<Status>(serde_json::json!("pending")).is_err());
    }

    #[test]
    fn spellings_are_the_json_ones() {
        for s in Status::ALL {
            assert_eq!(
                serde_json::to_value(s).unwrap(),
                serde_json::json!(s.as_str())
            );
        }
        for m in Method::ALL {
            assert_eq!(
                serde_json::to_value(m).unwrap(),
                serde_json::json!(m.as_str())
            );
        }
        for c in Confidence::ALL {
            assert_eq!(
                serde_json::to_value(c).unwrap(),
                serde_json::json!(c.as_str())
            );
        }
    }

    #[test]
    fn by_kind_reaches_the_field_of_each_kind() {
        let mut all = ByKind::<Vec<IdKind>>::default();
        for kind in IdKind::ALL {
            all.of_mut(kind).push(kind);
        }
        assert_eq!(all.tracks, [IdKind::Track]);
        assert_eq!(all.artists, [IdKind::Artist]);
        assert_eq!(all.albums, [IdKind::Album]);
        for kind in IdKind::ALL {
            assert_eq!(all.of(kind), &[kind]);
        }
    }

    #[test]
    fn settled_and_llm_states() {
        let cases = [
            (Status::Matched, Some(Method::Exact), true, false),
            (Status::Unmatched, None, true, false),
            (Status::Rejected, Some(Method::Manual), true, false),
            (Status::Ambiguous, None, false, true),
            (Status::LlmFailed, None, false, true),
            (Status::Ambiguous, Some(Method::Manual), true, false),
            (Status::Unmatched, Some(Method::External), true, false),
            (Status::Matched, Some(Method::External), true, false),
        ];
        for (status, method, settled, llm) in cases {
            let e = entry("TR:1", status, method);
            assert_eq!(e.is_settled(), settled, "{status:?} {method:?}");
            assert_eq!(e.awaits_llm(), llm, "{status:?} {method:?}");
        }
    }

    #[test]
    fn manual_entries() {
        let set = MatchEntry::manual_set("TR:1", "spotify:track:a", AT.into());
        assert_eq!(set.status, Status::Matched);
        assert_eq!(set.method, Some(Method::Manual));
        assert_eq!(set.confidence, Some(Confidence::High));
        assert_eq!(set.matched_uri(), Some("spotify:track:a"));
        let reject = MatchEntry::manual_reject("TR:1", AT.into());
        assert_eq!(reject.status, Status::Rejected);
        assert_eq!(reject.method, Some(Method::Manual));
        assert_eq!(reject.matched_uri(), None);
    }

    #[test]
    fn the_latest_entry_per_id_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("matches").join("tracks.jsonl");
        assert!(read_latest(&path).unwrap().is_empty());
        append(&path, &entry("TR:1", Status::Ambiguous, None)).unwrap();
        append(&path, &entry("TR:2", Status::Unmatched, None)).unwrap();
        append(
            &path,
            &MatchEntry::manual_set("TR:1", "spotify:track:a", AT.into()),
        )
        .unwrap();
        let latest = read_latest(&path).unwrap();
        assert_eq!(latest.len(), 2);
        assert_eq!(latest["TR:1"].matched_uri(), Some("spotify:track:a"));
        assert_eq!(latest["TR:2"].status, Status::Unmatched);
    }

    #[test]
    fn an_entry_written_without_optional_fields_reads() {
        let e: MatchEntry = serde_json::from_str(
            r#"{"pandora_id":"AR:1","status":"rejected","decided_at":"2026-10-15T12:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(e.status, Status::Rejected);
        assert_eq!(e.item_id, "AR:1");
        assert!(e.candidates.is_empty());
    }

    #[test]
    fn a_file_mixing_both_keys_reads_latest_per_item() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracks.jsonl");
        std::fs::write(
            &path,
            "{\"pandora_id\":\"TR:1\",\"status\":\"ambiguous\",\"decided_at\":\"x\"}\n",
        )
        .unwrap();
        append(&path, &entry("TR:1", Status::Unmatched, None)).unwrap();
        append(&path, &entry("YV:dQw4w9WgXcQ", Status::Ambiguous, None)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("\"item_id\"").count(), 2, "{text}");
        let latest = read_latest(&path).unwrap();
        assert_eq!(latest.len(), 2);
        assert_eq!(latest["TR:1"].status, Status::Unmatched);
        assert_eq!(latest["YV:dQw4w9WgXcQ"].status, Status::Ambiguous);
    }
}
