//! Spotify ids: a URI (`spotify:track:<id>`), an open.spotify.com address
//! or a bare id parsed into a typed id or a playback target, and the
//! agreement between a Pandora id's prefix and the Spotify type it may be
//! matched to.

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

/// The guidance [`parse`] and [`parse_target`] end their refusals with.
const FORMS: &str = "give a Spotify URI (spotify:track:<id>), an https://open.spotify.com/track/<id> \
                     address or a bare 22-character id";

/// Splits `text`, a URI, an open.spotify.com address (an `intl-xx` path
/// segment and any query allowed) or a bare id, into its type segment
/// (none for a bare id) and its base-62 id. Every refusal ends with
/// `forms`, the guidance on what the caller takes.
pub fn split(text: &str, forms: &str) -> Result<(Option<String>, String)> {
    let text = text.trim();
    let (segment, id) = if let Some(rest) = text.strip_prefix("spotify:") {
        let Some((kind, id)) = rest.split_once(':') else {
            fail!(Kind::Input, "{text:?} is not a Spotify URI; {forms}");
        };
        (Some(kind.to_owned()), id.to_owned())
    } else if text.contains('/') {
        let Ok(url) = Url::parse(text) else {
            fail!(Kind::Input, "{text:?} is not a web address; {forms}");
        };
        if url.host_str() != Some("open.spotify.com") {
            fail!(
                Kind::Input,
                "{text:?} is not an open.spotify.com address; {forms}"
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
                "{text:?} is not the address of a Spotify item; {forms}"
            );
        };
        (Some((*kind).to_owned()), (*id).to_owned())
    } else {
        (None, text.to_owned())
    };
    if !is_base62_id(&id) {
        fail!(Kind::Input, "{text:?} does not carry a Spotify id; {forms}");
    }
    Ok((segment, id))
}

/// Parses `text` as a URI, an open.spotify.com address (an `intl-xx` path
/// segment and any query allowed) or a bare id of a track, artist or
/// album. A bare id has no kind.
pub fn parse(text: &str) -> Result<(Option<IdKind>, String)> {
    let (segment, id) = split(text, FORMS)?;
    let text = text.trim();
    let kind = match segment {
        None => None,
        Some(segment) => match IdKind::from_segment(&segment) {
            Some(k) => Some(k),
            None => fail!(
                Kind::Input,
                "{text:?} names a Spotify {segment}, not a track, artist or album"
            ),
        },
    };
    Ok((kind, id))
}

/// The kinds of Spotify context playback can start: the type segment of
/// their URIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextKind {
    Album,
    Artist,
    Playlist,
}

impl ContextKind {
    /// `album`, `artist`, `playlist`.
    pub fn as_str(self) -> &'static str {
        match self {
            ContextKind::Album => "album",
            ContextKind::Artist => "artist",
            ContextKind::Playlist => "playlist",
        }
    }

    /// `spotify:<kind>:<id>`.
    pub fn uri(self, id: &str) -> String {
        format!("spotify:{self}:{id}")
    }
}

impl fmt::Display for ContextKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Something to play or queue: a track, or a context playback starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Track(String),
    Context { kind: ContextKind, id: String },
}

impl Target {
    /// `spotify:<segment>:<id>`.
    pub fn uri(&self) -> String {
        match self {
            Target::Track(id) => format!("spotify:track:{id}"),
            Target::Context { kind, id } => kind.uri(id),
        }
    }
}

/// Parses `text` (any form [`parse`] takes) as a track, album, artist or
/// playlist; a bare id is a track.
pub fn parse_target(text: &str) -> Result<Target> {
    let (segment, id) = split(text, FORMS)?;
    let context = |kind| {
        Ok(Target::Context {
            kind,
            id: id.clone(),
        })
    };
    match segment.as_deref() {
        None | Some("track") => Ok(Target::Track(id)),
        Some("album") => context(ContextKind::Album),
        Some("artist") => context(ContextKind::Artist),
        Some("playlist") => context(ContextKind::Playlist),
        Some(other) => fail!(
            Kind::Input,
            "{:?} names a Spotify {other}; only tracks, albums, artists and playlists can be \
             played",
            text.trim()
        ),
    }
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
    fn split_ends_every_refusal_with_the_forms_given() {
        for text in [
            "spotify:track",
            "https://example.com/track/4uLU6hMCjMI75M1A2tKUQC",
            "https://open.spotify.com/track",
            "https://open.spotify.com/track/short",
            "://open.spotify.com/x",
            "not an id",
        ] {
            let err = split(text, "take a widget").unwrap_err();
            assert_eq!(crate::error::kind(&err), Kind::Input, "{text}");
            assert!(
                format!("{err:#}").ends_with("; take a widget"),
                "{text}: {err:#}"
            );
        }
        assert_eq!(
            split(&format!("spotify:playlist:{ID}"), "x").unwrap(),
            (Some("playlist".to_owned()), ID.to_owned())
        );
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
    fn targets_parse_from_every_form() {
        for text in [
            format!("spotify:track:{ID}"),
            format!("https://open.spotify.com/track/{ID}?si=abc"),
            format!(" {ID} "),
        ] {
            let target = parse_target(&text).unwrap();
            assert_eq!(target, Target::Track(ID.into()), "{text}");
            assert_eq!(target.uri(), format!("spotify:track:{ID}"));
        }
        for (text, kind) in [
            (
                format!("https://open.spotify.com/playlist/{ID}"),
                ContextKind::Playlist,
            ),
            (format!("spotify:playlist:{ID}"), ContextKind::Playlist),
            (
                format!("https://open.spotify.com/intl-de/album/{ID}"),
                ContextKind::Album,
            ),
            (format!("spotify:album:{ID}"), ContextKind::Album),
            (
                format!("https://open.spotify.com/artist/{ID}"),
                ContextKind::Artist,
            ),
            (format!("spotify:artist:{ID}"), ContextKind::Artist),
        ] {
            let target = parse_target(&text).unwrap();
            assert_eq!(
                target,
                Target::Context {
                    kind,
                    id: ID.into()
                },
                "{text}"
            );
            assert_eq!(target.uri(), format!("spotify:{kind}:{ID}"));
        }
    }

    #[test]
    fn targets_that_cannot_be_played_are_input() {
        for (text, needle) in [
            (
                format!("spotify:show:{ID}"),
                "names a Spotify show; only tracks",
            ),
            (
                format!("https://open.spotify.com/episode/{ID}"),
                "names a Spotify episode",
            ),
            (format!("spotify:user:{ID}"), "names a Spotify user"),
            ("spotify:track:x".to_owned(), "does not carry a Spotify id"),
            (
                format!("https://example.com/track/{ID}"),
                "open.spotify.com",
            ),
        ] {
            let err = parse_target(&text).unwrap_err();
            assert_eq!(crate::error::kind(&err), Kind::Input, "{text}");
            assert!(format!("{err:#}").contains(needle), "{text}: {err:#}");
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
