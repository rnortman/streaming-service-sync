//! Spotify ids: a URI (`spotify:track:<id>`), an open.spotify.com address
//! or a bare id parsed into a typed id, and the agreement between a Pandora
//! id's prefix and the Spotify type it may be matched to.

use std::fmt;

use anyhow::Result;
use serde::Serialize;
use url::Url;

use crate::error::Kind;
use crate::fail;
use crate::pandora;

/// The kinds of Spotify entity a Pandora item is matched to, in the order
/// a matching run takes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdKind {
    Track,
    Artist,
    Album,
}

impl IdKind {
    pub const ALL: [IdKind; 3] = [IdKind::Track, IdKind::Artist, IdKind::Album];

    /// `tracks`, `artists`, `albums`.
    pub fn plural(self) -> &'static str {
        match self {
            IdKind::Track => "tracks",
            IdKind::Artist => "artists",
            IdKind::Album => "albums",
        }
    }

    /// The type segment of a URI or address: `track`, `artist`, `album`.
    pub fn as_str(self) -> &'static str {
        match self {
            IdKind::Track => "track",
            IdKind::Artist => "artist",
            IdKind::Album => "album",
        }
    }

    fn from_segment(segment: &str) -> Option<Self> {
        match segment {
            "track" => Some(IdKind::Track),
            "artist" => Some(IdKind::Artist),
            "album" => Some(IdKind::Album),
            _ => None,
        }
    }

    /// The kind a Pandora id is matched to, from its prefix: `TR:` a track,
    /// `AR:` and `CO:` (a composer) an artist, `AL:` an album.
    pub fn of_pandora_id(pandora_id: &str) -> Option<Self> {
        match pandora::model::id_type(pandora_id)? {
            "TR" => Some(IdKind::Track),
            "AR" | "CO" => Some(IdKind::Artist),
            "AL" => Some(IdKind::Album),
            _ => None,
        }
    }
}

impl fmt::Display for IdKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A typed Spotify id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SpotifyId {
    pub kind: IdKind,
    /// The base-62 id.
    pub id: String,
}

impl SpotifyId {
    /// `spotify:<kind>:<id>`.
    pub fn uri(&self) -> String {
        format!("spotify:{}:{}", self.kind, self.id)
    }

    /// `https://open.spotify.com/<kind>/<id>`.
    pub fn open_url(&self) -> String {
        format!("{OPEN}/{}/{}", self.kind, self.id)
    }

    /// The id of `uri`, a `spotify:<kind>:<id>` URI of a track, artist or
    /// album; none for anything else.
    pub fn of_uri(uri: &str) -> Option<Self> {
        if !uri.starts_with("spotify:") {
            return None;
        }
        match parse(uri).ok()? {
            (Some(kind), id) => Some(SpotifyId { kind, id }),
            (None, _) => None,
        }
    }
}

const OPEN: &str = "https://open.spotify.com";

/// `https://open.spotify.com/playlist/<id>`.
pub fn playlist_url(id: &str) -> String {
    format!("{OPEN}/playlist/{id}")
}

/// Whether `id` looks like a Spotify id: 22 ASCII letters and digits.
fn is_base62_id(id: &str) -> bool {
    id.len() == 22 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

const FORMS: &str = "give a Spotify URI (spotify:track:<id>), an https://open.spotify.com/track/<id> \
                     address or a bare 22-character id";

/// Parses `text` as a URI, an open.spotify.com address (an `intl-xx` path
/// segment and any query allowed) or a bare id. A bare id has no kind.
pub fn parse(text: &str) -> Result<(Option<IdKind>, String)> {
    let text = text.trim();
    let (kind, id) = if let Some(rest) = text.strip_prefix("spotify:") {
        let Some((kind, id)) = rest.split_once(':') else {
            fail!(Kind::Input, "{text:?} is not a Spotify URI; {FORMS}");
        };
        (Some(kind.to_owned()), id.to_owned())
    } else if text.contains('/') {
        let Ok(url) = Url::parse(text) else {
            fail!(Kind::Input, "{text:?} is not a web address; {FORMS}");
        };
        if url.host_str() != Some("open.spotify.com") {
            fail!(
                Kind::Input,
                "{text:?} is not an open.spotify.com address; {FORMS}"
            );
        }
        let segments: Vec<&str> = url
            .path_segments()
            .map(|s| s.filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        let segments = match segments.split_first() {
            Some((first, rest)) if first.starts_with("intl-") => rest,
            _ => &segments[..],
        };
        let [kind, id] = segments else {
            fail!(
                Kind::Input,
                "{text:?} is not the address of a track, artist or album; {FORMS}"
            );
        };
        (Some((*kind).to_owned()), (*id).to_owned())
    } else {
        (None, text.to_owned())
    };
    let kind = match kind {
        None => None,
        Some(segment) => match IdKind::from_segment(&segment) {
            Some(k) => Some(k),
            None => fail!(
                Kind::Input,
                "{text:?} names a Spotify {segment}, not a track, artist or album"
            ),
        },
    };
    if !is_base62_id(&id) {
        fail!(Kind::Input, "{text:?} does not carry a Spotify id; {FORMS}");
    }
    Ok((kind, id))
}

/// Parses `text` (as [`parse`]) as the Spotify counterpart of `pandora_id`:
/// its kind must be the one the Pandora prefix calls for.
pub fn parse_for(pandora_id: &str, text: &str) -> Result<SpotifyId> {
    let Some(wanted) = IdKind::of_pandora_id(pandora_id) else {
        fail!(
            Kind::Input,
            "{pandora_id:?} is not a Pandora track (TR:), artist (AR:, CO:) or album (AL:) id"
        );
    };
    let (kind, id) = parse(text)?;
    match kind {
        Some(kind) if kind != wanted => fail!(
            Kind::Input,
            "{pandora_id} is a Pandora {wanted} but {text:?} is a Spotify {kind}"
        ),
        _ => Ok(SpotifyId { kind: wanted, id }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "4uLU6hMCjMI75M1A2tKUQC";

    #[test]
    fn uris_addresses_and_bare_ids_parse() {
        for (text, kind) in [
            (format!("spotify:track:{ID}"), Some(IdKind::Track)),
            (format!("spotify:artist:{ID}"), Some(IdKind::Artist)),
            (
                format!("https://open.spotify.com/album/{ID}?si=abc123"),
                Some(IdKind::Album),
            ),
            (
                format!("https://open.spotify.com/intl-de/track/{ID}"),
                Some(IdKind::Track),
            ),
            (format!("  {ID}\n"), None),
        ] {
            assert_eq!(parse(&text).unwrap(), (kind, ID.to_owned()), "{text}");
        }
    }

    #[test]
    fn malformed_input_is_refused_with_kind_input() {
        for (text, needle) in [
            ("spotify:track", "not a Spotify URI"),
            (
                "spotify:playlist:4uLU6hMCjMI75M1A2tKUQC",
                "Spotify playlist",
            ),
            (
                "https://example.com/track/4uLU6hMCjMI75M1A2tKUQC",
                "open.spotify.com",
            ),
            ("https://open.spotify.com/track", "not the address"),
            ("https://open.spotify.com/track/short", "does not carry"),
            ("://open.spotify.com/x", "not a web address"),
            ("not an id", "does not carry"),
            ("", "does not carry"),
        ] {
            let err = parse(text).unwrap_err();
            assert_eq!(crate::error::kind(&err), Kind::Input, "{text}");
            assert!(format!("{err:#}").contains(needle), "{text}: {err:#}");
        }
    }

    #[test]
    fn the_pandora_prefix_fixes_the_kind() {
        assert_eq!(
            parse_for("TR:1", &format!("https://open.spotify.com/track/{ID}?si=x")).unwrap(),
            SpotifyId {
                kind: IdKind::Track,
                id: ID.into()
            }
        );
        assert_eq!(parse_for("CO:9", ID).unwrap().kind, IdKind::Artist);
        assert_eq!(
            parse_for("AL:2", ID).unwrap().uri(),
            format!("spotify:album:{ID}")
        );
        let err = parse_for("TR:1", &format!("spotify:album:{ID}")).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Input);
        assert!(
            format!("{err:#}").contains("TR:1 is a Pandora track but"),
            "{err:#}"
        );
        for not_an_item in ["ST:1", "TR:", "tr:1", "TR1"] {
            let err = parse_for(not_an_item, ID).unwrap_err();
            assert_eq!(crate::error::kind(&err), Kind::Input, "{not_an_item}");
        }
    }

    #[test]
    fn uris_read_back_and_render_as_addresses() {
        let id = SpotifyId::of_uri(&format!("spotify:album:{ID}")).unwrap();
        assert_eq!(id.kind, IdKind::Album);
        assert_eq!(
            id.open_url(),
            format!("https://open.spotify.com/album/{ID}")
        );
        for not_an_item_uri in [
            format!("spotify:playlist:{ID}"),
            format!("https://open.spotify.com/track/{ID}"),
            ID.to_owned(),
            "spotify:track:short".to_owned(),
        ] {
            assert_eq!(
                SpotifyId::of_uri(&not_an_item_uri),
                None,
                "{not_an_item_uri}"
            );
        }
        assert_eq!(
            playlist_url(ID),
            format!("https://open.spotify.com/playlist/{ID}")
        );
    }
}
