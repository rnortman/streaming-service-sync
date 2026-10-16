//! [`Offered`]: the one shape every document gives a Spotify track, artist
//! or album — a stored candidate, a search result, the playing item, a
//! queue entry.

use serde::Serialize;

use crate::matching::model::Candidate;
use crate::spotify::client::{FoundAlbum, FoundArtist, FoundTrack};

/// A Spotify track, artist or album: its URI as `id`, its name, and the
/// fields its kind carries; no score. Fields a kind does not carry are left
/// out of the JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Offered {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artists: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explicit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
}

impl Offered {
    /// Only `id` and `name`, every other field empty: an artist, or an
    /// item (an episode) whose other fields this shape does not carry.
    pub fn named(id: impl Into<String>, name: impl Into<String>) -> Self {
        Offered {
            id: id.into(),
            name: name.into(),
            artists: Vec::new(),
            album: None,
            album_type: None,
            release_date: None,
            duration_ms: None,
            explicit: None,
            isrc: None,
        }
    }
}

impl From<&Candidate> for Offered {
    fn from(c: &Candidate) -> Self {
        // Exhaustive, so a new candidate field has to be offered or dropped here.
        let Candidate {
            uri,
            name,
            artists,
            album,
            album_type,
            release_date,
            duration_ms,
            explicit,
            isrc,
            score: _,
        } = c;
        Offered {
            id: uri.clone(),
            name: name.clone(),
            artists: artists.clone(),
            album: album.clone(),
            album_type: album_type.clone(),
            release_date: release_date.clone(),
            duration_ms: *duration_ms,
            explicit: *explicit,
            isrc: isrc.clone(),
        }
    }
}

impl From<&FoundTrack> for Offered {
    fn from(t: &FoundTrack) -> Self {
        // Exhaustive, so a new track field has to be offered or dropped here.
        let FoundTrack {
            uri,
            name,
            artists,
            album,
            album_type,
            release_date,
            duration_ms,
            explicit,
            isrc,
        } = t;
        Offered {
            id: uri.clone(),
            name: name.clone(),
            artists: artists.clone(),
            album: album.clone(),
            album_type: album_type.clone(),
            release_date: release_date.clone(),
            duration_ms: *duration_ms,
            explicit: *explicit,
            isrc: isrc.clone(),
        }
    }
}

impl From<&FoundArtist> for Offered {
    fn from(a: &FoundArtist) -> Self {
        Offered::named(a.uri.clone(), a.name.clone())
    }
}

impl From<&FoundAlbum> for Offered {
    fn from(a: &FoundAlbum) -> Self {
        let FoundAlbum {
            uri,
            name,
            artists,
            album_type,
            release_date,
        } = a;
        Offered {
            artists: artists.clone(),
            album_type: album_type.clone(),
            release_date: release_date.clone(),
            ..Offered::named(uri.clone(), name.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn track() -> FoundTrack {
        FoundTrack {
            uri: "spotify:track:4uLU6hMCjMI75M1A2tKUQC".into(),
            name: "Song".into(),
            artists: vec!["Band".into()],
            album: Some("LP".into()),
            album_type: Some("album".into()),
            release_date: Some("2004".into()),
            duration_ms: Some(217_000),
            explicit: Some(false),
            isrc: Some("USRC17607839".into()),
        }
    }

    #[test]
    fn a_candidate_is_offered_without_its_score() {
        let c = Candidate {
            uri: "spotify:track:a".into(),
            name: "Song".into(),
            artists: vec!["Band".into()],
            duration_ms: Some(1000),
            score: 0.93,
            ..Candidate::default()
        };
        assert_eq!(
            serde_json::to_value(Offered::from(&c)).unwrap(),
            json!({"id": "spotify:track:a", "name": "Song", "artists": ["Band"],
                   "duration_ms": 1000})
        );
    }

    #[test]
    fn a_found_track_keeps_every_field() {
        assert_eq!(
            serde_json::to_value(Offered::from(&track())).unwrap(),
            json!({"id": "spotify:track:4uLU6hMCjMI75M1A2tKUQC", "name": "Song",
                   "artists": ["Band"], "album": "LP", "album_type": "album",
                   "release_date": "2004", "duration_ms": 217000, "explicit": false,
                   "isrc": "USRC17607839"})
        );
    }

    #[test]
    fn found_artists_and_albums_carry_their_own_fields() {
        let artist = FoundArtist {
            uri: "spotify:artist:a".into(),
            name: "Band".into(),
        };
        assert_eq!(
            serde_json::to_value(Offered::from(&artist)).unwrap(),
            json!({"id": "spotify:artist:a", "name": "Band"})
        );
        let album = FoundAlbum {
            uri: "spotify:album:b".into(),
            name: "LP".into(),
            artists: vec!["Band".into()],
            album_type: Some("compilation".into()),
            release_date: None,
        };
        assert_eq!(
            serde_json::to_value(Offered::from(&album)).unwrap(),
            json!({"id": "spotify:album:b", "name": "LP", "artists": ["Band"],
                   "album_type": "compilation"})
        );
    }
}
