//! The Spotify Web API client: a bearer token kept fresh through the token
//! endpoint and saved before any request uses it, paced requests, retries
//! on 429, 5xx and transport failures, a quota stop, failures classified
//! into kinds, and the six requests matching, push and `sss search` use,
//! each parsed into a lenient type of its own. The player endpoints are in
//! [`super::playback`].

use std::fmt;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::{self, Kind, WithKind};
use crate::fail;
use crate::http::{self, Pacer, Reply};
use crate::secrets::StateDir;
use crate::spotify::auth::{self, SETUP_DOC, Token};
use crate::spotify::ids::IdKind;
use crate::spotify::playback::PlayerRefused;

/// The live Web API base.
pub const DEFAULT_API_BASE_URL: &str = "https://api.spotify.com/v1";

/// Environment variable that replaces [`DEFAULT_API_BASE_URL`] (used to
/// point the binary at a test server).
pub const API_BASE_URL_ENV: &str = "SSS_SPOTIFY_API_BASE_URL";

/// Attempts per request, the first included, for 429s and for outages
/// (5xx or no response), counted separately.
pub const MAX_TRIES: u32 = 5;

/// The longest `Retry-After` waited out; a longer one is a quota stop.
pub const MAX_RETRY_AFTER_S: u64 = 60;

/// An access token this close to its expiry, or past it, is renewed
/// before the next request.
pub const REFRESH_MARGIN_S: i64 = 60;

/// Results asked of one search by matching, and the most `sss search`
/// may ask: the development-mode API refuses a larger `limit`.
pub const SEARCH_LIMIT: u32 = 10;

/// URIs per `me/library` request.
pub const LIBRARY_CHUNK: usize = 40;

/// URIs per playlist-items write, and items per page read.
pub const PLAYLIST_CHUNK: usize = 100;

/// Request pacing and retry waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Minimum time between the starts of two requests.
    pub min_interval: Duration,
    /// Wait before the second attempt after an outage; doubled before
    /// each further one.
    pub first_backoff: Duration,
    /// How long one second of `Retry-After` is.
    pub retry_after_unit: Duration,
    /// Wait after a player command before reading the player's state, which
    /// can still be the old one right after the write.
    pub settle: Duration,
}

impl Timing {
    /// At most 2 requests/s; outage backoff 1 s, 2 s, 4 s, 8 s;
    /// `Retry-After` taken at its word; 1 s for the player to settle.
    pub const LIVE: Timing = Timing {
        min_interval: Duration::from_millis(500),
        first_backoff: Duration::from_secs(1),
        retry_after_unit: Duration::from_secs(1),
        settle: Duration::from_secs(1),
    };
}

/// `$SSS_SPOTIFY_API_BASE_URL` when set and non-empty, else the live API;
/// without a trailing `/`.
pub fn api_base_from_env() -> String {
    http::base_url_from_env(API_BASE_URL_ENV, DEFAULT_API_BASE_URL)
}

/// A request Spotify answered with a 4xx this client has no other kind
/// for. Kind `other`; [`http_status`] finds it in a chain, so a caller can
/// tell a playlist id Spotify does not know (404) from any other refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    /// `<METHOD> <path>: HTTP <status>…` with Spotify's message.
    pub detail: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for ApiError {}

/// The HTTP status of the [`ApiError`] in `err`'s chain, if any.
pub fn http_status(err: &anyhow::Error) -> Option<u16> {
    err.chain()
        .find_map(|e| e.downcast_ref::<ApiError>())
        .map(|e| e.status)
}

/// A track a search returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundTrack {
    pub uri: String,
    pub name: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub album_type: Option<String>,
    pub release_date: Option<String>,
    pub duration_ms: Option<u64>,
    pub explicit: Option<bool>,
    /// `external_ids.isrc`; `None` when the response carried none.
    pub isrc: Option<String>,
}

/// An artist a search returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundArtist {
    pub uri: String,
    pub name: String,
}

/// An album a search returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundAlbum {
    pub uri: String,
    pub name: String,
    pub artists: Vec<String>,
    pub album_type: Option<String>,
    pub release_date: Option<String>,
}

/// One search's results: the list for the type searched is filled, the
/// others are empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResults {
    pub tracks: Vec<FoundTrack>,
    pub artists: Vec<FoundArtist>,
    pub albums: Vec<FoundAlbum>,
}

/// What [`Spotify::playlist_add_items`] calls after each chunk lands: its
/// URIs and the playlist's new snapshot id.
pub type ItemsAdded<'a> = dyn FnMut(&[String], Option<&str>) -> Result<()> + 'a;

/// A playlist [`Spotify::playlist_create`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedPlaylist {
    pub id: String,
    pub snapshot_id: Option<String>,
}

// The wire types: every field a caller does not require is optional or
// defaulted, unknown fields are ignored, and a list entry that does not
// parse is dropped rather than failing the response.

#[derive(Deserialize, Default)]
struct Named {
    name: Option<String>,
}

fn names(list: Vec<Named>) -> Vec<String> {
    list.into_iter().filter_map(|a| a.name).collect()
}

#[derive(Deserialize)]
struct WireAlbumRef {
    name: Option<String>,
    album_type: Option<String>,
    release_date: Option<String>,
}

#[derive(Deserialize)]
struct WireExternalIds {
    isrc: Option<String>,
}

/// A track as search, the player and the queue send it.
#[derive(Deserialize)]
pub(super) struct WireTrack {
    uri: String,
    name: String,
    #[serde(default)]
    artists: Vec<Named>,
    album: Option<WireAlbumRef>,
    duration_ms: Option<u64>,
    explicit: Option<bool>,
    external_ids: Option<WireExternalIds>,
}

impl From<WireTrack> for FoundTrack {
    fn from(t: WireTrack) -> Self {
        let album = t.album;
        FoundTrack {
            uri: t.uri,
            name: t.name,
            artists: names(t.artists),
            album_type: album.as_ref().and_then(|a| a.album_type.clone()),
            release_date: album.as_ref().and_then(|a| a.release_date.clone()),
            album: album.and_then(|a| a.name),
            duration_ms: t.duration_ms,
            explicit: t.explicit,
            isrc: t.external_ids.and_then(|e| e.isrc),
        }
    }
}

#[derive(Deserialize)]
struct WireArtist {
    uri: String,
    name: String,
}

#[derive(Deserialize)]
struct WireAlbum {
    uri: String,
    name: String,
    #[serde(default)]
    artists: Vec<Named>,
    album_type: Option<String>,
    release_date: Option<String>,
}

#[derive(Deserialize, Default)]
struct WirePage {
    #[serde(default)]
    items: Vec<Value>,
}

#[derive(Deserialize)]
struct WireSearch {
    #[serde(default)]
    tracks: WirePage,
    #[serde(default)]
    artists: WirePage,
    #[serde(default)]
    albums: WirePage,
}

/// The entries of `page` that parse as `T`.
fn parsed_items<T: DeserializeOwned>(page: WirePage) -> Vec<T> {
    page.items
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect()
}

#[derive(Deserialize)]
struct WireCreated {
    id: String,
    snapshot_id: Option<String>,
}

#[derive(Deserialize)]
struct WireSnapshot {
    snapshot_id: Option<String>,
}

#[derive(Deserialize)]
struct WireItemRef {
    uri: Option<String>,
}

#[derive(Deserialize)]
struct WirePlaylistItem {
    item: Option<WireItemRef>,
}

#[derive(Deserialize)]
struct WireItemsPage {
    #[serde(default)]
    items: Vec<Option<WirePlaylistItem>>,
    next: Option<String>,
}

/// `HTTP <status>` with Spotify's `error.message` (and `error.reason`), or
/// the body quoted when it is not Spotify's error object.
fn describe(status: u16, body: &str) -> String {
    let error = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").cloned());
    let text = |k: &str| error.as_ref()?.get(k)?.as_str().map(str::to_owned);
    match (text("message"), text("reason")) {
        (Some(m), Some(r)) => format!("HTTP {status}: {m} ({r})"),
        (Some(m), None) => format!("HTTP {status}: {m}"),
        (None, Some(r)) => format!("HTTP {status}: {r}"),
        _ if body.trim().is_empty() => format!("HTTP {status}"),
        _ => format!("HTTP {status}: {:?}", body.trim()),
    }
}

/// Whether a 429 body's `error.message` or `error.reason` speaks of a quota.
fn mentions_quota(body: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    ["message", "reason"].iter().any(|k| {
        v.pointer(&format!("/error/{k}"))
            .and_then(Value::as_str)
            .is_some_and(|s| s.to_ascii_lowercase().contains("quota"))
    })
}

/// The refusal a player endpoint answered with: Spotify's player error
/// object carries `error.reason`; the plain error object does not, and
/// gives `None`.
fn player_refusal(what: &str, status: u16, body: &str) -> Option<PlayerRefused> {
    let v = serde_json::from_str::<Value>(body).ok()?;
    let text = |k: &str| {
        v.pointer(&format!("/error/{k}"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    Some(PlayerRefused {
        what: what.to_owned(),
        status,
        reason: text("reason")?,
        message: text("message").unwrap_or_default(),
        devices: Vec::new(),
    })
}

/// The outage retries of one request — no response, a 5xx, or a token
/// renewal that got no answer — over [`MAX_TRIES`] with doubling backoff.
struct Outages {
    tries: u32,
    backoff: Duration,
}

impl Outages {
    /// Counts one outage: `false` once [`MAX_TRIES`] are spent; otherwise
    /// warns that `what` failed with `detail`, waits the backoff, doubles
    /// it and is `true`.
    fn retry(&mut self, warn: fn(&str), what: &str, detail: &str) -> bool {
        self.tries += 1;
        if self.tries >= MAX_TRIES {
            return false;
        }
        warn(&format!(
            "warning: {what}: {detail} on attempt {}/{MAX_TRIES}; retrying",
            self.tries
        ));
        std::thread::sleep(self.backoff);
        self.backoff = self.backoff.saturating_mul(2);
        true
    }
}

/// A request to send.
pub(super) struct Call<'a> {
    pub(super) method: &'a str,
    pub(super) path: &'a str,
    pub(super) query: &'a [(&'a str, &'a str)],
    pub(super) body: Option<&'a Value>,
    /// A player endpoint: a 403 or other 4xx whose body carries
    /// `error.reason` is a [`PlayerRefused`].
    pub(super) player: bool,
}

impl Call<'_> {
    /// Whether the request may be resent after no response or a 5xx.
    /// A `POST` (a playlist create, an items add, a skip or a queue add)
    /// is not: Spotify may have applied it, and a second one would make a
    /// second playlist, add the items twice, skip twice or queue twice.
    fn resendable(&self) -> bool {
        self.method != "POST"
    }

    /// The end of the message for a request that got no answer, or a 5xx,
    /// and is not resent. A player command cannot be undone, so its
    /// message says to check before running it again.
    fn not_resent(&self) -> &'static str {
        if self.player {
            "not resent, since Spotify may have applied it; run `sss player` to see whether it \
             did before running the command again"
        } else {
            "not resent, since Spotify may have applied it; re-run"
        }
    }
}

/// A paced Spotify Web API client for one profile.
pub struct Spotify {
    agent: ureq::Agent,
    api_base: String,
    auth_base: String,
    client_id: String,
    market: String,
    token: Token,
    state: StateDir,
    timing: Timing,
    pacer: Pacer,
    requests: u32,
    settles: u32,
    /// Receives each warning line; [`http::print_warning`] writes it to
    /// stderr.
    pub(crate) warn: fn(&str),
}

impl Spotify {
    /// A client for the token stored in `state`, renewed with `client_id`.
    /// `market` is the `[spotify] market` setting; empty means the
    /// market of the signed-in account. No stored token is kind `auth`.
    pub fn new(
        state: StateDir,
        client_id: &str,
        market: &str,
        api_base: &str,
        auth_base: &str,
        timing: Timing,
    ) -> Result<Self> {
        let Some(stored) = state.load_spotify_token()? else {
            fail!(
                Kind::Auth,
                "no Spotify token in {}; run `sss spotify auth`",
                state.dir().display()
            );
        };
        Ok(Self {
            agent: http::agent(),
            api_base: api_base.trim_end_matches('/').to_owned(),
            auth_base: auth_base.trim_end_matches('/').to_owned(),
            client_id: client_id.to_owned(),
            market: market.to_owned(),
            token: stored.token,
            state,
            timing,
            pacer: Pacer::new(timing.min_interval),
            requests: 0,
            settles: 0,
            warn: http::print_warning,
        })
    }

    /// A live-paced client against the base URLs of the environment.
    pub fn from_env(state: StateDir, client_id: &str, market: &str) -> Result<Self> {
        Self::new(
            state,
            client_id,
            market,
            &api_base_from_env(),
            &auth::auth_base_from_env(),
            Timing::LIVE,
        )
    }

    /// HTTP responses received so far, retried attempts included.
    pub fn requests(&self) -> u32 {
        self.requests
    }

    /// Waits `Timing::settle`, so that a state read after a player command
    /// sees the command's effect.
    pub fn settle(&mut self) {
        self.settles += 1;
        std::thread::sleep(self.timing.settle);
    }

    /// Writes `line` where the client's own warnings go.
    pub fn warning(&self, line: &str) {
        (self.warn)(line);
    }

    /// How many times [`Spotify::settle`] has waited.
    pub fn settles(&self) -> u32 {
        self.settles
    }

    /// The `auth` failure unless the stored token was granted every scope
    /// in `scopes`; `profile` names the sign-in to run.
    pub fn require_scopes(&self, profile: &str, scopes: &[&str]) -> Result<()> {
        let missing: Vec<&str> = scopes
            .iter()
            .copied()
            .filter(|s| !self.token.scopes.iter().any(|g| g == s))
            .collect();
        if !missing.is_empty() {
            fail!(
                Kind::Auth,
                "the Spotify token for profile {profile} was granted without {}; run \
                 `sss spotify auth --profile {profile}` to sign in again with the playback scopes",
                missing.join(", ")
            );
        }
        Ok(())
    }

    /// `GET search`: up to `limit` results of each of `kinds` for `q`, in
    /// one request.
    pub fn search(&mut self, q: &str, kinds: &[IdKind], limit: u32) -> Result<SearchResults> {
        let limit = limit.to_string();
        let types = kinds
            .iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let market = self.market.clone();
        let mut query = vec![("q", q), ("type", types.as_str()), ("limit", &limit)];
        // With no `market`, Spotify uses the country of the user's token.
        if !market.is_empty() {
            query.push(("market", &market));
        }
        let wire: WireSearch = self.json(&Call {
            method: "GET",
            path: "search",
            query: &query,
            body: None,
            player: false,
        })?;
        Ok(SearchResults {
            tracks: parsed_items::<WireTrack>(wire.tracks)
                .into_iter()
                .map(FoundTrack::from)
                .collect(),
            artists: parsed_items::<WireArtist>(wire.artists)
                .into_iter()
                .map(|a| FoundArtist {
                    uri: a.uri,
                    name: a.name,
                })
                .collect(),
            albums: parsed_items::<WireAlbum>(wire.albums)
                .into_iter()
                .map(|a| FoundAlbum {
                    uri: a.uri,
                    name: a.name,
                    artists: names(a.artists),
                    album_type: a.album_type,
                    release_date: a.release_date,
                })
                .collect(),
        })
    }

    /// `POST me/playlists`: a new playlist of the member's.
    pub fn playlist_create(
        &mut self,
        name: &str,
        public: bool,
        description: &str,
    ) -> Result<CreatedPlaylist> {
        let body = json!({"name": name, "public": public, "description": description});
        let wire: WireCreated = self.json(&Call {
            method: "POST",
            path: "me/playlists",
            query: &[],
            body: Some(&body),
            player: false,
        })?;
        Ok(CreatedPlaylist {
            id: wire.id,
            snapshot_id: wire.snapshot_id,
        })
    }

    /// `GET playlists/{id}/items`, every page: each item's URI in playlist
    /// order, `None` for a local or removed item. An id Spotify does not
    /// know is an [`ApiError`] with status 404.
    pub fn playlist_items(&mut self, playlist_id: &str) -> Result<Vec<Option<String>>> {
        let path = format!("playlists/{playlist_id}/items");
        let limit = PLAYLIST_CHUNK.to_string();
        let mut uris = Vec::new();
        loop {
            let offset = uris.len().to_string();
            let page: WireItemsPage = self.json(&Call {
                method: "GET",
                path: &path,
                query: &[
                    ("fields", "items(item(uri)),next"),
                    ("limit", &limit),
                    ("offset", &offset),
                ],
                body: None,
                player: false,
            })?;
            let got = page.items.len();
            uris.extend(
                page.items
                    .into_iter()
                    .map(|i| i.and_then(|i| i.item).and_then(|i| i.uri)),
            );
            if page.next.is_none() || got == 0 {
                return Ok(uris);
            }
        }
    }

    /// `POST playlists/{id}/items` in chunks of [`PLAYLIST_CHUNK`], in
    /// order. `added` is called after each chunk lands with its URIs and
    /// the playlist's new snapshot id; its error stops the writes.
    pub fn playlist_add_items(
        &mut self,
        playlist_id: &str,
        uris: &[String],
        added: &mut ItemsAdded<'_>,
    ) -> Result<()> {
        let path = format!("playlists/{playlist_id}/items");
        for chunk in uris.chunks(PLAYLIST_CHUNK) {
            let body = json!({ "uris": chunk });
            let wire: WireSnapshot = self.json(&Call {
                method: "POST",
                path: &path,
                query: &[],
                body: Some(&body),
                player: false,
            })?;
            added(chunk, wire.snapshot_id.as_deref())?;
        }
        Ok(())
    }

    /// `PUT me/library` in chunks of [`LIBRARY_CHUNK`]: saves tracks and
    /// albums and follows artists. `saved` is called after each chunk
    /// lands; its error stops the writes.
    pub fn library_add(
        &mut self,
        uris: &[String],
        saved: &mut dyn FnMut(&[String]) -> Result<()>,
    ) -> Result<()> {
        let body = json!({});
        for chunk in uris.chunks(LIBRARY_CHUNK) {
            let joined = chunk.join(",");
            self.call(&Call {
                method: "PUT",
                path: "me/library",
                query: &[("uris", &joined)],
                body: Some(&body),
                player: false,
            })?;
            saved(chunk)?;
        }
        Ok(())
    }

    /// `GET me/library/contains` in chunks of [`LIBRARY_CHUNK`]: whether
    /// each of `uris` is already in the member's library.
    pub fn library_contains(&mut self, uris: &[String]) -> Result<Vec<bool>> {
        let mut held = Vec::with_capacity(uris.len());
        for chunk in uris.chunks(LIBRARY_CHUNK) {
            let joined = chunk.join(",");
            let answer: Vec<bool> = self.json(&Call {
                method: "GET",
                path: "me/library/contains",
                query: &[("uris", &joined)],
                body: None,
                player: false,
            })?;
            if answer.len() != chunk.len() {
                return Err(anyhow!(
                    "GET me/library/contains: {} answers for {} URIs",
                    answer.len(),
                    chunk.len()
                ));
            }
            held.extend(answer);
        }
        Ok(held)
    }

    /// Sends `call` and parses the 2xx body as `T`.
    pub(super) fn json<T: DeserializeOwned>(&mut self, call: &Call<'_>) -> Result<T> {
        let body = self.call(call)?;
        serde_json::from_str(&body)
            .with_context(|| format!("{} {}: parsing the response", call.method, call.path))
    }

    /// Sends `call`: `None` when the 2xx body is empty (a 204, or a 200
    /// with no body), else the body parsed as `T`.
    pub(super) fn json_opt<T: DeserializeOwned>(&mut self, call: &Call<'_>) -> Result<Option<T>> {
        let body = self.call(call)?;
        if body.trim().is_empty() {
            return Ok(None);
        }
        serde_json::from_str(&body)
            .map(Some)
            .with_context(|| format!("{} {}: parsing the response", call.method, call.path))
    }

    /// Sends `call` with the retry policy and returns the 2xx body.
    pub(super) fn call(&mut self, call: &Call<'_>) -> Result<String> {
        let what = format!("{} {}", call.method, call.path);
        let mut renewed_after_401 = false;
        let mut renew_now = false;
        let mut rate_tries = 0;
        let mut outages = Outages {
            tries: 0,
            backoff: self.timing.first_backoff,
        };
        loop {
            if renew_now || self.expires_soon() {
                match self.renew() {
                    Ok(()) => renew_now = false,
                    Err(e) if error::kind(&e) == Kind::Unavailable => {
                        if outages.retry(self.warn, &what, &format!("renewing the token: {e:#}")) {
                            continue;
                        }
                        return Err(e).with_context(|| {
                            format!("{what}: no token renewal after {MAX_TRIES} tries")
                        });
                    }
                    Err(e) => return Err(e),
                }
            }
            self.pacer.pace();
            let answer = match self.send(call) {
                Ok(a) => a,
                Err(e) => {
                    if !call.resendable() {
                        return Err(e).with_kind(Kind::Unavailable, || {
                            format!("{what}: no response; {}", call.not_resent())
                        });
                    }
                    if outages.retry(self.warn, &what, &format!("no response ({e:#})")) {
                        continue;
                    }
                    return Err(e).with_kind(Kind::Unavailable, || {
                        format!("{what}: no response after {MAX_TRIES} tries")
                    });
                }
            };
            self.requests += 1;
            let status = answer.status;
            match status {
                200..=299 => return Ok(answer.body),
                401 if !renewed_after_401 => {
                    renewed_after_401 = true;
                    renew_now = true;
                }
                401 => fail!(
                    Kind::Auth,
                    "{what}: Spotify refused the access token ({}); run `sss spotify auth`",
                    describe(status, &answer.body)
                ),
                403 => {
                    if call.player
                        && let Some(refused) = player_refusal(&what, status, &answer.body)
                    {
                        return Err(anyhow::Error::new(refused));
                    }
                    fail!(
                        Kind::Config,
                        "{what}: Spotify refused access ({}): the member may not be on the app's \
                         user allowlist, a scope may be missing, or the app owner's Premium may \
                         have lapsed (see {SETUP_DOC})",
                        describe(status, &answer.body)
                    )
                }
                429 => {
                    rate_tries += 1;
                    let wait = answer
                        .retry_after
                        .as_deref()
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .unwrap_or(1);
                    if wait > MAX_RETRY_AFTER_S
                        || mentions_quota(&answer.body)
                        || rate_tries >= MAX_TRIES
                    {
                        fail!(
                            Kind::Quota,
                            "{what}: Spotify's rate or quota limit reached ({}); everything \
                             done so far is recorded — re-run later (Retry-After {wait} s)",
                            describe(status, &answer.body)
                        );
                    }
                    (self.warn)(&format!(
                        "warning: {what}: rate limited on attempt {rate_tries}/{MAX_TRIES}; \
                         waiting {wait} s"
                    ));
                    std::thread::sleep(
                        self.timing
                            .retry_after_unit
                            .saturating_mul(u32::try_from(wait).unwrap_or(u32::MAX)),
                    );
                }
                500..=599 => {
                    let detail = describe(status, &answer.body);
                    if !call.resendable() {
                        fail!(Kind::Unavailable, "{what}: {detail}; {}", call.not_resent());
                    }
                    if !outages.retry(self.warn, &what, &detail) {
                        fail!(
                            Kind::Unavailable,
                            "{what}: {detail} after {MAX_TRIES} tries"
                        );
                    }
                }
                _ => {
                    if call.player
                        && let Some(refused) = player_refusal(&what, status, &answer.body)
                    {
                        return Err(anyhow::Error::new(refused));
                    }
                    return Err(anyhow::Error::new(ApiError {
                        status,
                        detail: format!("{what}: {}", describe(status, &answer.body)),
                    }));
                }
            }
        }
    }

    /// Whether the token expires within [`REFRESH_MARGIN_S`] (or its
    /// expiry cannot be read).
    fn expires_soon(&self) -> bool {
        OffsetDateTime::parse(&self.token.expires_at, &Rfc3339)
            .map(|at| (at - OffsetDateTime::now_utc()).whole_seconds())
            .unwrap_or(i64::MIN)
            <= REFRESH_MARGIN_S
    }

    /// Renews the token and writes it to the token file; the new token is
    /// used only once it is on disk. A token another process wrote to the
    /// file since this one read it is taken first, and is refreshed only if
    /// it too is about to expire: its refresh token is the live one.
    fn renew(&mut self) -> Result<()> {
        if let Some(stored) = self.state.load_spotify_token()?
            && stored.token != self.token
        {
            self.token = stored.token;
            if !self.expires_soon() {
                return Ok(());
            }
        }
        let fresh = auth::refresh(&self.auth_base, &self.token, &self.client_id)?;
        self.state.save_spotify_token(&fresh)?;
        self.token = fresh;
        Ok(())
    }

    /// One HTTP exchange, whatever its status; `Err` only when no response
    /// came.
    fn send(&self, call: &Call<'_>) -> Result<Reply> {
        let url = format!("{}/{}", self.api_base, call.path);
        let mut request = self.agent.request(call.method, &url).set(
            "Authorization",
            &format!("Bearer {}", self.token.access_token),
        );
        for (k, v) in call.query {
            request = request.query(k, v);
        }
        // A bodyless write still carries `Content-Length: 0`, which some
        // front ends require of a `PUT` or `POST`.
        let result = match call.body {
            Some(body) => request
                .set("Content-Type", "application/json")
                .send_string(&body.to_string()),
            None if call.method == "GET" => request.call(),
            None => request.send_bytes(&[]),
        };
        http::read(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spotify::test_support::{FAST, in_seconds, state_with};
    use httpmock::prelude::*;
    use std::path::Path;
    use std::time::Instant;

    fn quiet(_: &str) {}

    fn token(access: &str, expires_in: i64) -> Token {
        Token {
            access_token: access.into(),
            refresh_token: "refresh-secret".into(),
            expires_at: in_seconds(expires_in),
            scopes: vec!["user-library-read".into()],
        }
    }

    fn client(state: StateDir, server: &MockServer, timing: Timing) -> Spotify {
        let mut c = Spotify::new(
            state,
            "test-client-id",
            "",
            &server.url("/v1"),
            &server.base_url(),
            timing,
        )
        .unwrap();
        c.warn = quiet;
        c
    }

    /// A client whose stored token is valid for an hour.
    fn fresh_client(tmp: &Path, server: &MockServer) -> Spotify {
        client(state_with(tmp, &token("access-secret", 3600)), server, FAST)
    }

    fn search_ok(server: &MockServer) -> httpmock::Mock<'_> {
        server.mock(|when, then| {
            when.method(GET).path("/v1/search");
            then.status(200).json_body(json!({"tracks": {"items": []}}));
        })
    }

    fn refresh_mock<'a>(server: &'a MockServer, reply: Value) -> httpmock::Mock<'a> {
        server.mock(|when, then| {
            when.method(POST)
                .path("/api/token")
                .form_urlencoded_tuple("grant_type", "refresh_token")
                .form_urlencoded_tuple("refresh_token", "refresh-secret")
                .form_urlencoded_tuple("client_id", "test-client-id");
            then.status(200).json_body(reply);
        })
    }

    #[test]
    fn no_stored_token_is_auth() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::under(tmp.path(), "alice").unwrap();
        let err = Spotify::new(state, "id", "", "http://a", "http://b", FAST)
            .err()
            .unwrap();
        assert_eq!(crate::error::kind(&err), Kind::Auth);
        assert!(
            format!("{err:#}").contains("run `sss spotify auth`"),
            "{err:#}"
        );
    }

    #[test]
    fn search_with_no_market_sends_none_and_parses_a_lenient_body() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/search")
                .header("Authorization", "Bearer access-secret")
                .query_param("q", "isrc:USRC17607839")
                .query_param("type", "track")
                .query_param("limit", "10")
                .query_param_missing("market");
            then.status(200)
                .json_body(json!({"tracks": {"href": "x", "items": [
                {"uri": "spotify:track:a", "name": "Song", "popularity": 3,
                 "artists": [{"name": "Band"}, {"id": "nameless"}],
                 "album": {"name": "LP", "album_type": "album", "release_date": "2004-03-01"},
                 "duration_ms": 217000, "explicit": false,
                 "external_ids": {"isrc": "USRC17607839"}},
                {"uri": "spotify:track:b", "name": "Song (Live)"},
                null,
                {"name": "no uri"}
            ]}, "unknown": 1}));
        });
        let mut c = fresh_client(tmp.path(), &server);
        let found = c
            .search("isrc:USRC17607839", &[IdKind::Track], SEARCH_LIMIT)
            .unwrap();
        mock.assert();
        assert_eq!(
            found.tracks,
            [
                FoundTrack {
                    uri: "spotify:track:a".into(),
                    name: "Song".into(),
                    artists: vec!["Band".into()],
                    album: Some("LP".into()),
                    album_type: Some("album".into()),
                    release_date: Some("2004-03-01".into()),
                    duration_ms: Some(217_000),
                    explicit: Some(false),
                    isrc: Some("USRC17607839".into()),
                },
                FoundTrack {
                    uri: "spotify:track:b".into(),
                    name: "Song (Live)".into(),
                    artists: vec![],
                    album: None,
                    album_type: None,
                    release_date: None,
                    duration_ms: None,
                    explicit: None,
                    isrc: None,
                },
            ]
        );
        assert!(found.artists.is_empty() && found.albums.is_empty());
        assert_eq!(c.requests(), 1);
    }

    #[test]
    fn artist_and_album_searches_and_a_configured_market() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET)
                .path("/v1/search")
                .query_param("type", "artist")
                .query_param("market", "US");
            then.status(200).json_body(json!({"artists": {"items": [
                {"uri": "spotify:artist:a", "name": "Band", "genres": []}]}}));
        });
        server.mock(|when, then| {
            when.method(GET)
                .path("/v1/search")
                .query_param("type", "album");
            then.status(200).json_body(json!({"albums": {"items": [
                {"uri": "spotify:album:b", "name": "LP", "artists": [{"name": "Band"}],
                 "album_type": "compilation", "release_date": "1999"}]}}));
        });
        let mut c = fresh_client(tmp.path(), &server);
        c.market = "US".into();
        assert_eq!(
            c.search("Band", &[IdKind::Artist], SEARCH_LIMIT)
                .unwrap()
                .artists,
            [FoundArtist {
                uri: "spotify:artist:a".into(),
                name: "Band".into()
            }]
        );
        assert_eq!(
            c.search("LP Band", &[IdKind::Album], SEARCH_LIMIT)
                .unwrap()
                .albums,
            [FoundAlbum {
                uri: "spotify:album:b".into(),
                name: "LP".into(),
                artists: vec!["Band".into()],
                album_type: Some("compilation".into()),
                release_date: Some("1999".into()),
            }]
        );
    }

    #[test]
    fn requests_are_paced() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        search_ok(&server);
        let timing = Timing {
            min_interval: Duration::from_millis(60),
            ..FAST
        };
        let mut c = client(
            state_with(tmp.path(), &token("access-secret", 3600)),
            &server,
            timing,
        );
        let start = Instant::now();
        for _ in 0..3 {
            c.search("x", &[IdKind::Track], SEARCH_LIMIT).unwrap();
        }
        assert!(start.elapsed() >= Duration::from_millis(120));
    }

    #[test]
    fn an_expired_token_is_renewed_and_saved_before_the_request() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let renew = refresh_mock(
            &server,
            json!({"access_token": "fresh", "expires_in": 3600}),
        );
        let state = state_with(tmp.path(), &token("stale", -10));
        let token_path = state.spotify_token_path();
        let api = server.mock(move |when, then| {
            when.method(GET)
                .path("/v1/search")
                .header("Authorization", "Bearer fresh")
                .is_true(move |_| {
                    // The token file already holds the new token when the
                    // request arrives.
                    std::fs::read_to_string(&token_path)
                        .is_ok_and(|t| t.contains("\"access_token\": \"fresh\""))
                });
            then.status(200).json_body(json!({}));
        });
        let mut c = client(state.clone(), &server, FAST);
        c.search("x", &[IdKind::Track], SEARCH_LIMIT).unwrap();
        renew.assert();
        api.assert();
        let stored = state.load_spotify_token().unwrap().unwrap().token;
        assert_eq!(stored.access_token, "fresh");
        assert_eq!(
            stored.refresh_token, "refresh-secret",
            "an omitted refresh token is kept"
        );
    }

    #[test]
    fn a_token_near_expiry_is_renewed_and_one_further_off_is_not() {
        for (expires_in, renewed) in [(30, true), (120, false)] {
            let tmp = tempfile::tempdir().unwrap();
            let server = MockServer::start();
            let renew = refresh_mock(
                &server,
                json!({"access_token": "fresh", "expires_in": 3600}),
            );
            search_ok(&server);
            let mut c = client(
                state_with(tmp.path(), &token("old", expires_in)),
                &server,
                FAST,
            );
            c.search("x", &[IdKind::Track], SEARCH_LIMIT).unwrap();
            renew.assert_calls(usize::from(renewed));
        }
    }

    #[test]
    fn a_refused_refresh_is_auth_and_no_request_is_sent() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/api/token");
            then.status(400)
                .json_body(json!({"error": "invalid_grant"}));
        });
        let api = search_ok(&server);
        let mut c = client(state_with(tmp.path(), &token("old", -1)), &server, FAST);
        let err = c.search("x", &[IdKind::Track], SEARCH_LIMIT).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Auth);
        api.assert_calls(0);
    }

    #[test]
    fn a_token_another_process_renewed_is_taken_instead_of_refreshing() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut rotate = refresh_mock(
            &server,
            json!({"access_token": "fresh", "expires_in": 3600,
                   "refresh_token": "rotated"}),
        );
        let api = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/search")
                .header("Authorization", "Bearer fresh");
            then.status(200).json_body(json!({"tracks": {"items": []}}));
        });
        let state = state_with(tmp.path(), &token("old", -1));
        let mut first = client(state.clone(), &server, FAST);
        let mut second = client(state.clone(), &server, FAST);
        first.search("x", &[IdKind::Track], SEARCH_LIMIT).unwrap();
        rotate.assert_calls(1);
        rotate.delete();
        // The refresh token both started with is spent now.
        let refused = server.mock(|when, then| {
            when.method(POST).path("/api/token");
            then.status(400)
                .json_body(json!({"error": "invalid_grant"}));
        });
        second.search("x", &[IdKind::Track], SEARCH_LIMIT).unwrap();
        refused.assert_calls(0);
        api.assert_calls(2);
        let stored = state.load_spotify_token().unwrap().unwrap().token;
        assert_eq!(stored.refresh_token, "rotated");
    }

    #[test]
    fn a_renewal_without_an_answer_is_retried_like_an_outage() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let busy_once = server.mock(move |when, then| {
            when.method(POST)
                .path("/api/token")
                .is_true(move |_| seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0);
            then.status(503);
        });
        let renew = refresh_mock(
            &server,
            json!({"access_token": "fresh", "expires_in": 3600}),
        );
        let api = search_ok(&server);
        let state = state_with(tmp.path(), &token("old", -1));
        let mut c = client(state.clone(), &server, FAST);
        c.search("x", &[IdKind::Track], SEARCH_LIMIT).unwrap();
        busy_once.assert_calls(1);
        renew.assert_calls(1);
        api.assert_calls(1);
        let stored = state.load_spotify_token().unwrap().unwrap().token;
        assert_eq!(stored.access_token, "fresh");

        let server = MockServer::start();
        let busy = server.mock(|when, then| {
            when.method(POST).path("/api/token");
            then.status(503);
        });
        let api = search_ok(&server);
        let tmp = tempfile::tempdir().unwrap();
        let mut c = client(state_with(tmp.path(), &token("old", -1)), &server, FAST);
        let err = c.search("x", &[IdKind::Track], SEARCH_LIMIT).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Unavailable);
        assert!(
            format!("{err:#}").starts_with("GET search: no token renewal after 5 tries: "),
            "{err:#}"
        );
        busy.assert_calls(MAX_TRIES as usize);
        api.assert_calls(0);
    }

    #[test]
    fn an_unwritable_token_file_fails_before_the_request() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        refresh_mock(
            &server,
            json!({"access_token": "fresh", "expires_in": 3600}),
        );
        let api = search_ok(&server);
        let state = state_with(tmp.path(), &token("old", -1));
        let mut c = client(state.clone(), &server, FAST);
        std::fs::remove_file(state.spotify_token_path()).unwrap();
        std::fs::create_dir_all(state.spotify_token_path().join("x")).unwrap();
        let err = c.search("x", &[IdKind::Track], SEARCH_LIMIT).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Other);
        assert!(format!("{err:#}").contains("spotify-token.json"), "{err:#}");
        api.assert_calls(0);
    }

    #[test]
    fn a_401_renews_once_and_retries_and_a_second_is_auth() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let renew = refresh_mock(
            &server,
            json!({"access_token": "fresh", "expires_in": 3600}),
        );
        let ok = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/search")
                .header("Authorization", "Bearer fresh");
            then.status(200).json_body(json!({}));
        });
        let refused = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/search")
                .header("Authorization", "Bearer access-secret");
            then.status(401).json_body(
                json!({"error": {"status": 401, "message": "The access token expired"}}),
            );
        });
        let mut c = fresh_client(tmp.path(), &server);
        c.search("x", &[IdKind::Track], SEARCH_LIMIT).unwrap();
        refused.assert_calls(1);
        renew.assert_calls(1);
        ok.assert_calls(1);

        let server = MockServer::start();
        refresh_mock(
            &server,
            json!({"access_token": "fresh", "expires_in": 3600}),
        );
        let api = server.mock(|when, then| {
            when.method(GET).path("/v1/search");
            then.status(401)
                .json_body(json!({"error": {"status": 401, "message": "Invalid access token"}}));
        });
        let tmp = tempfile::tempdir().unwrap();
        let mut c = fresh_client(tmp.path(), &server);
        let err = c.search("x", &[IdKind::Track], SEARCH_LIMIT).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Auth);
        assert_eq!(
            format!("{err:#}"),
            "GET search: Spotify refused the access token (HTTP 401: Invalid access token); \
             run `sss spotify auth`"
        );
        api.assert_calls(2);
    }

    #[test]
    fn a_429_is_waited_out_and_retried() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let limited = server.mock(|when, then| {
            when.method(GET).path("/v1/search").query_param("q", "a");
            then.status(429).header("Retry-After", "1");
        });
        let mut c = fresh_client(tmp.path(), &server);
        let err = c.search("a", &[IdKind::Track], SEARCH_LIMIT).unwrap_err();
        limited.assert_calls(MAX_TRIES as usize);
        assert_eq!(crate::error::kind(&err), Kind::Quota);

        let limited_once = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = limited_once.clone();
        let first = server.mock(move |when, then| {
            when.method(GET)
                .path("/v1/search")
                .query_param("q", "b")
                .is_true(move |_| seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0);
            then.status(429).header("Retry-After", "1");
        });
        let ok = server.mock(|when, then| {
            when.method(GET).path("/v1/search").query_param("q", "b");
            then.status(200).json_body(json!({}));
        });
        c.search("b", &[IdKind::Track], SEARCH_LIMIT).unwrap();
        first.assert_calls(1);
        ok.assert();
    }

    #[test]
    fn a_long_retry_after_or_a_quota_body_is_a_quota_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let long = server.mock(|when, then| {
            when.method(GET).path("/v1/search").query_param("q", "a");
            then.status(429).header("Retry-After", "3600");
        });
        let quota = server.mock(|when, then| {
            when.method(GET).path("/v1/search").query_param("q", "b");
            then.status(429)
                .json_body(json!({"error": {"status": 429, "message": "Daily quota exceeded"}}));
        });
        let mut c = fresh_client(tmp.path(), &server);
        let err = c.search("a", &[IdKind::Track], SEARCH_LIMIT).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Quota);
        assert_eq!(
            format!("{err:#}"),
            "GET search: Spotify's rate or quota limit reached (HTTP 429); everything done so \
             far is recorded — re-run later (Retry-After 3600 s)"
        );
        long.assert_calls(1);
        let err = c.search("b", &[IdKind::Track], SEARCH_LIMIT).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Quota);
        assert!(
            format!("{err:#}").contains("Daily quota exceeded"),
            "{err:#}"
        );
        quota.assert_calls(1);
    }

    #[test]
    fn server_errors_and_no_response_are_unavailable_after_every_try() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/v1/search");
            then.status(503).body("<html>busy</html>");
        });
        let mut c = fresh_client(tmp.path(), &server);
        let err = c.search("x", &[IdKind::Track], SEARCH_LIMIT).unwrap_err();
        mock.assert_calls(MAX_TRIES as usize);
        assert_eq!(crate::error::kind(&err), Kind::Unavailable);
        assert_eq!(
            format!("{err:#}"),
            "GET search: HTTP 503: \"<html>busy</html>\" after 5 tries"
        );

        let tmp = tempfile::tempdir().unwrap();
        let state = state_with(tmp.path(), &token("access-secret", 3600));
        let mut c = Spotify::new(state, "id", "", "http://127.0.0.1:1", "http://b", FAST).unwrap();
        c.warn = quiet;
        let err = c.search("x", &[IdKind::Track], SEARCH_LIMIT).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Unavailable);
        assert!(
            format!("{err:#}").starts_with("GET search: no response after 5 tries: "),
            "{err:#}"
        );
        assert_eq!(c.requests(), 0);
    }

    #[test]
    fn a_403_is_config_and_names_the_setup_doc() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/me/playlists");
            then.status(403).json_body(json!({"error": {"status": 403,
                "message": "User not registered in the Developer Dashboard"}}));
        });
        let mut c = fresh_client(tmp.path(), &server);
        let err = c.playlist_create("n", false, "d").unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Config);
        let text = format!("{err:#}");
        assert!(
            text.starts_with(
                "POST me/playlists: Spotify refused access (HTTP 403: User not registered in \
                 the Developer Dashboard)"
            ),
            "{text}"
        );
        assert!(text.ends_with("(see docs/spotify-app-setup.md)"), "{text}");
        assert!(!text.contains("secret"), "{text}");
    }

    #[test]
    fn other_4xx_are_other_with_their_status_and_text() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/playlists/gone/items");
            then.status(404)
                .json_body(json!({"error": {"status": 404, "message": "Resource not found"}}));
        });
        server.mock(|when, then| {
            when.method(GET).path("/v1/search");
            then.status(400).body("bad market");
        });
        let mut c = fresh_client(tmp.path(), &server);
        let err = c.playlist_items("gone").unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Other);
        assert_eq!(http_status(&err), Some(404));
        assert_eq!(
            format!("{err:#}"),
            "GET playlists/gone/items: HTTP 404: Resource not found"
        );
        let err = c.search("x", &[IdKind::Track], SEARCH_LIMIT).unwrap_err();
        assert_eq!(format!("{err:#}"), "GET search: HTTP 400: \"bad market\"");
        assert_eq!(http_status(&err), Some(400));
        assert_eq!(http_status(&anyhow!("plain")), None);
    }

    #[test]
    fn playlist_create_posts_its_json_body() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/me/playlists")
                .header("Content-Type", "application/json")
                .json_body(json!({"name": "Pandora: Rock", "public": false,
                                  "description": "d sss:ST:1"}));
            then.status(201)
                .json_body(json!({"id": "pl1", "snapshot_id": "s1", "items": null}));
        });
        let mut c = fresh_client(tmp.path(), &server);
        assert_eq!(
            c.playlist_create("Pandora: Rock", false, "d sss:ST:1")
                .unwrap(),
            CreatedPlaylist {
                id: "pl1".into(),
                snapshot_id: Some("s1".into())
            }
        );
        mock.assert();
    }

    #[test]
    fn playlist_items_reads_every_page() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let first: Vec<Value> = (0..100)
            .map(|n| json!({"item": {"uri": format!("spotify:track:{n}")}}))
            .collect();
        let p1 = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/playlists/pl1/items")
                .query_param("offset", "0")
                .query_param("limit", "100")
                .query_param("fields", "items(item(uri)),next");
            then.status(200)
                .json_body(json!({"items": first, "next": "https://x/next"}));
        });
        let p2 = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/playlists/pl1/items")
                .query_param("offset", "100");
            then.status(200).json_body(json!({"items": [
                {"item": {"uri": "spotify:track:last"}}, {"item": null}, {"item": {"uri": null}}
            ], "next": null}));
        });
        let mut c = fresh_client(tmp.path(), &server);
        let uris = c.playlist_items("pl1").unwrap();
        p1.assert();
        p2.assert();
        assert_eq!(uris.len(), 103);
        assert_eq!(uris[99].as_deref(), Some("spotify:track:99"));
        assert_eq!(uris[100].as_deref(), Some("spotify:track:last"));
        assert_eq!(uris[101], None);
        assert_eq!(uris[102], None);
    }

    fn uris(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("spotify:track:{i}")).collect()
    }

    #[test]
    fn playlist_items_are_added_in_hundreds_and_reported_per_chunk() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let all = uris(250);
        let mut mocks = Vec::new();
        for (n, chunk) in all.chunks(100).enumerate() {
            let chunk = chunk.to_vec();
            mocks.push(server.mock(move |when, then| {
                when.method(POST)
                    .path("/v1/playlists/pl1/items")
                    .json_body(json!({ "uris": chunk }));
                then.status(201)
                    .json_body(json!({"snapshot_id": format!("s{n}")}));
            }));
        }
        let mut c = fresh_client(tmp.path(), &server);
        let mut seen = Vec::new();
        c.playlist_add_items("pl1", &all, &mut |chunk, snap| {
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
                (50, Some("s2".into()))
            ]
        );
        let err = c
            .playlist_add_items("pl1", &all, &mut |_, _| Err(anyhow!("ledger write failed")))
            .unwrap_err();
        assert_eq!(err.to_string(), "ledger write failed");
        mocks[1].assert_calls(1);
    }

    #[test]
    fn a_post_is_not_resent_after_a_server_error_or_no_response() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let create = server.mock(|when, then| {
            when.method(POST).path("/v1/me/playlists");
            then.status(502).body("bad gateway");
        });
        let add = server.mock(|when, then| {
            when.method(POST).path("/v1/playlists/pl1/items");
            then.status(503).body("busy");
        });
        let mut c = fresh_client(tmp.path(), &server);
        let err = c.playlist_create("n", false, "d").unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Unavailable);
        assert_eq!(
            format!("{err:#}"),
            "POST me/playlists: HTTP 502: \"bad gateway\"; not resent, since Spotify may \
             have applied it; re-run"
        );
        create.assert_calls(1);
        let mut chunks = 0;
        let err = c
            .playlist_add_items("pl1", &uris(150), &mut |_, _| {
                chunks += 1;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Unavailable);
        add.assert_calls(1);
        assert_eq!(chunks, 0);

        let tmp = tempfile::tempdir().unwrap();
        let state = state_with(tmp.path(), &token("access-secret", 3600));
        let mut c = Spotify::new(state, "id", "", "http://127.0.0.1:1", "http://b", FAST).unwrap();
        c.warn = quiet;
        let err = c.playlist_create("n", false, "d").unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Unavailable);
        assert!(
            format!("{err:#}").starts_with(
                "POST me/playlists: no response; not resent, since Spotify may have applied \
                 it; re-run: "
            ),
            "{err:#}"
        );
    }

    #[test]
    fn library_writes_and_checks_go_in_forties() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let all = uris(90);
        let mut puts = Vec::new();
        let mut gets = Vec::new();
        for (n, chunk) in all.chunks(40).enumerate() {
            let joined = chunk.join(",");
            let answer: Vec<bool> = chunk.iter().map(|_| n == 1).collect();
            let q = joined.clone();
            puts.push(server.mock(move |when, then| {
                when.method(PUT)
                    .path("/v1/me/library")
                    .query_param("uris", q)
                    .json_body(json!({}));
                then.status(200);
            }));
            gets.push(server.mock(move |when, then| {
                when.method(GET)
                    .path("/v1/me/library/contains")
                    .query_param("uris", joined);
                then.status(200).json_body(json!(answer));
            }));
        }
        let mut c = fresh_client(tmp.path(), &server);
        let held = c.library_contains(&all).unwrap();
        assert_eq!(held.len(), 90);
        assert!(!held[39] && held[40] && held[79] && !held[80]);
        let mut sizes = Vec::new();
        c.library_add(&all, &mut |chunk| {
            sizes.push(chunk.len());
            Ok(())
        })
        .unwrap();
        assert_eq!(sizes, [40, 40, 10]);
        for m in puts.iter().chain(&gets) {
            m.assert();
        }
    }

    #[test]
    fn a_contains_answer_of_the_wrong_length_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/me/library/contains");
            then.status(200).json_body(json!([true]));
        });
        let mut c = fresh_client(tmp.path(), &server);
        let err = c.library_contains(&uris(2)).unwrap_err();
        assert!(
            format!("{err:#}").contains("1 answers for 2 URIs"),
            "{err:#}"
        );
    }

    #[test]
    fn require_scopes_names_what_the_token_lacks() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut granted = token("access-secret", 3600);
        granted.scopes.push("user-read-playback-state".into());
        let c = client(state_with(tmp.path(), &granted), &server, FAST);
        c.require_scopes("alice", &["user-read-playback-state"])
            .unwrap();
        c.require_scopes("alice", &[]).unwrap();
        let err = c
            .require_scopes(
                "alice",
                &["user-read-playback-state", "user-modify-playback-state"],
            )
            .unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Auth);
        assert_eq!(
            format!("{err:#}"),
            "the Spotify token for profile alice was granted without \
             user-modify-playback-state; run `sss spotify auth --profile alice` to sign in \
             again with the playback scopes"
        );
    }

    #[derive(Deserialize, Debug, PartialEq)]
    struct Doc {
        a: u32,
    }

    #[test]
    fn json_opt_is_none_on_an_empty_body() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        for (path, status, body) in [("/v1/e204", 204, ""), ("/v1/e200", 200, " \n")] {
            server.mock(|when, then| {
                when.method(GET).path(path);
                then.status(status).body(body);
            });
        }
        server.mock(|when, then| {
            when.method(GET).path("/v1/doc");
            then.status(200).json_body(json!({"a": 1}));
        });
        let mut c = fresh_client(tmp.path(), &server);
        let get = |path| Call {
            method: "GET",
            path,
            query: &[],
            body: None,
            player: false,
        };
        assert_eq!(c.json_opt::<Doc>(&get("e204")).unwrap(), None);
        assert_eq!(c.json_opt::<Doc>(&get("e200")).unwrap(), None);
        assert_eq!(c.json_opt::<Doc>(&get("doc")).unwrap(), Some(Doc { a: 1 }));
    }

    #[test]
    fn a_search_of_several_kinds_is_one_request_with_its_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/search")
                .query_param("type", "track,artist,album")
                .query_param("limit", "3");
            then.status(200).json_body(json!({
                "tracks": {"items": [{"uri": "spotify:track:t", "name": "Song"}]},
                "artists": {"items": [{"uri": "spotify:artist:a", "name": "Band"}]},
                "albums": {"items": [{"uri": "spotify:album:b", "name": "LP"}]}
            }));
        });
        let mut c = fresh_client(tmp.path(), &server);
        let found = c.search("x", &IdKind::ALL, 3).unwrap();
        mock.assert();
        assert_eq!(
            (found.tracks.len(), found.artists.len(), found.albums.len()),
            (1, 1, 1)
        );
    }

    #[test]
    fn a_reason_on_a_refused_search_keeps_its_routing() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/search").query_param("q", "a");
            then.status(403).json_body(
                json!({"error": {"status": 403, "message": "m", "reason": "PREMIUM_REQUIRED"}}),
            );
        });
        server.mock(|when, then| {
            when.method(GET).path("/v1/search").query_param("q", "b");
            then.status(404).json_body(
                json!({"error": {"status": 404, "message": "m", "reason": "NO_ACTIVE_DEVICE"}}),
            );
        });
        let mut c = fresh_client(tmp.path(), &server);
        let err = c.search("a", &[IdKind::Track], SEARCH_LIMIT).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Config);
        let err = c.search("b", &[IdKind::Track], SEARCH_LIMIT).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Other);
        assert_eq!(http_status(&err), Some(404));
    }

    #[test]
    fn error_bodies_are_described() {
        assert_eq!(describe(500, ""), "HTTP 500");
        assert_eq!(describe(502, "oops\n"), "HTTP 502: \"oops\"");
        assert_eq!(
            describe(
                429,
                r#"{"error":{"status":429,"message":"m","reason":"r"}}"#
            ),
            "HTTP 429: m (r)"
        );
        assert!(mentions_quota(r#"{"error":{"reason":"QUOTA_EXCEEDED"}}"#));
        assert!(!mentions_quota(r#"{"error":{"message":"slow down"}}"#));
        assert!(!mentions_quota("quota"));
    }
}
