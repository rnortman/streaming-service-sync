//! The YouTube Data API v3 client: `videos.list` by id with an API key,
//! paced, retried on outages, every response stored verbatim in the
//! profile's append-only cache before anything parses it, and the cache read
//! back as the latest answer per video.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::error::{Kind, WithKind};
use crate::fail;
use crate::http::{self, Pacer};
use crate::repo;

/// The live API base.
pub const DEFAULT_BASE_URL: &str = "https://www.googleapis.com/youtube/v3";

/// Environment variable that replaces [`DEFAULT_BASE_URL`] (used to point
/// the binary at a test server).
pub const BASE_URL_ENV: &str = "SSS_YOUTUBE_API_BASE_URL";

/// Attempts per request, the first included, for outages.
pub const MAX_TRIES: u32 = 5;

/// Video ids per `videos.list` request, the API's maximum.
pub const CHUNK: usize = 50;

/// The parts `videos.list` is asked for.
pub const PART: &str = "snippet,contentDetails,topicDetails";

/// The path every cached exchange has.
pub const VIDEOS_PATH: &str = "videos";

/// The public video a key is verified against.
pub const VERIFY_VIDEO_ID: &str = "dQw4w9WgXcQ";

/// The cache file name after its sequence number.
const CACHE_SUFFIX: &str = "-videos-list.json";

/// Request pacing and outage backoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Minimum time between the starts of two requests.
    pub min_interval: Duration,
    /// Wait before the second attempt; doubled before each further one.
    pub first_backoff: Duration,
}

impl Timing {
    /// At most 5 requests/s; backoff 1 s, 2 s, 4 s, 8 s.
    pub const LIVE: Timing = Timing {
        min_interval: Duration::from_millis(200),
        first_backoff: Duration::from_secs(1),
    };
}

/// What a `videos.list` request asked for. The key is not part of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub ids: Vec<String>,
    pub part: String,
}

/// One request and its response, as stored in the cache. It holds no URL
/// and no headers, so the key never reaches it.
#[derive(Debug, Serialize, Deserialize)]
pub struct Exchange {
    /// When the request was sent, RFC 3339 UTC.
    pub at: String,
    pub method: String,
    pub path: String,
    pub request: Request,
    pub status: u16,
    /// The response body verbatim when it is JSON, else the body as a JSON
    /// string.
    pub response: Box<RawValue>,
}

/// The facts `videos.list` gives about one video.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiVideo {
    pub video_id: String,
    pub title: Option<String>,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub category_id: Option<String>,
    /// `None` for a live stream or a length that does not parse.
    pub duration_s: Option<u32>,
    /// `topicDetails.topicCategories`: Wikipedia addresses.
    pub topics: Vec<String>,
}

/// The latest cached answer about one video.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cached {
    /// The video was in the response.
    Found(ApiVideo),
    /// The video was asked for and not returned: removed, private or never
    /// there.
    Absent,
}

/// A `videos.list` body. `items` is required: every real answer carries it,
/// empty when nothing matched, and a body without it must not mark the
/// asked ids unavailable.
#[derive(Debug, Deserialize)]
struct WireList {
    items: Vec<WireVideo>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct WireVideo {
    id: String,
    snippet: WireSnippet,
    content_details: WireContent,
    topic_details: WireTopics,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct WireSnippet {
    title: Option<String>,
    channel_id: Option<String>,
    channel_title: Option<String>,
    category_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WireContent {
    duration: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct WireTopics {
    topic_categories: Vec<String>,
}

impl From<WireVideo> for ApiVideo {
    fn from(v: WireVideo) -> Self {
        let text = |s: Option<String>| s.filter(|s| !s.is_empty());
        ApiVideo {
            video_id: v.id,
            title: text(v.snippet.title),
            channel_id: text(v.snippet.channel_id),
            channel_title: text(v.snippet.channel_title),
            category_id: text(v.snippet.category_id),
            duration_s: v.content_details.duration.as_deref().and_then(duration_s),
            topics: v.topic_details.topic_categories,
        }
    }
}

/// The videos of a `videos.list` response body; `None` when it is not one.
fn parse_list(body: &Value) -> Option<Vec<ApiVideo>> {
    let list = WireList::deserialize(body).ok()?;
    Some(list.items.into_iter().map(ApiVideo::from).collect())
}

/// Seconds in an ISO 8601 duration as `contentDetails.duration` writes it
/// (`PT4M13S`, `P1DT2H`, `PT45S`); `None` for zero (a live stream) or a
/// form it does not have.
pub fn duration_s(iso: &str) -> Option<u32> {
    let rest = iso.strip_prefix('P')?;
    let (date, time) = match rest.split_once('T') {
        Some((d, t)) if !t.is_empty() => (d, t),
        Some(_) => return None,
        None => (rest, ""),
    };
    let total = fields(date, &[('W', 604_800), ('D', 86_400)])?
        .checked_add(fields(time, &[('H', 3_600), ('M', 60), ('S', 1)])?)?;
    u32::try_from(total).ok().filter(|&s| s > 0)
}

/// The sum of `<n><unit>` fields of `s`, units in the order of `units`,
/// each at most once.
fn fields(s: &str, units: &[(char, u64)]) -> Option<u64> {
    let mut total: u64 = 0;
    let mut rest = s;
    let mut next = 0;
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit())?;
        if digits == 0 {
            return None;
        }
        let n: u64 = rest[..digits].parse().ok()?;
        let unit = rest[digits..].chars().next()?;
        let at = units[next..].iter().position(|&(u, _)| u == unit)? + next;
        total = total.checked_add(n.checked_mul(units[at].1)?)?;
        next = at + 1;
        rest = &rest[digits + unit.len_utf8()..];
    }
    Some(total)
}

/// Every `reason` in a Google error body: `error.errors[].reason` and
/// `error.details[].reason`.
fn reasons(body: Option<&Value>) -> Vec<String> {
    let Some(error) = body.and_then(|b| b.get("error")) else {
        return Vec::new();
    };
    ["errors", "details"]
        .iter()
        .filter_map(|k| error.get(k).and_then(Value::as_array))
        .flatten()
        .filter_map(|e| e.get("reason").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

/// `HTTP <status>` plus the error body's reasons and message.
fn describe(status: u16, body: Option<&Value>) -> String {
    let mut text = format!("HTTP {status}");
    let reasons = reasons(body);
    if !reasons.is_empty() {
        text.push_str(&format!(", reason {}", reasons.join("/")));
    }
    if let Some(m) = body
        .and_then(|b| b.pointer("/error/message"))
        .and_then(Value::as_str)
    {
        text.push_str(&format!(", message {m:?}"));
    }
    text
}

/// The error of a non-2xx response that is not an outage, by its kind.
fn refusal(status: u16, body: Option<&Value>) -> anyhow::Error {
    let reasons = reasons(body);
    let has = |names: &[&str]| reasons.iter().any(|r| names.contains(&r.as_str()));
    let detail = describe(status, body);
    let (kind, message) = if status == 429
        || has(&[
            "quotaExceeded",
            "rateLimitExceeded",
            "dailyLimitExceeded",
            "userRateLimitExceeded",
        ]) {
        (
            Kind::Quota,
            format!(
                "the YouTube Data API quota is spent ({detail}); the daily quota resets at \
                 midnight Pacific; everything fetched so far is recorded"
            ),
        )
    } else if has(&["keyInvalid", "API_KEY_INVALID"]) {
        (
            Kind::Input,
            format!(
                "the YouTube Data API refused the key as invalid ({detail}); store a valid one \
                 with `sss youtube api-key`"
            ),
        )
    } else if has(&["accessNotConfigured", "SERVICE_DISABLED"]) {
        (
            Kind::Config,
            format!(
                "YouTube Data API v3 is not enabled in the key's Google Cloud project ({detail}); \
                 enable it in the Cloud console under APIs & Services → Library, then retry"
            ),
        )
    } else {
        (
            Kind::Other,
            format!("GET {VIDEOS_PATH}: the YouTube Data API refused the request ({detail})"),
        )
    };
    anyhow::Error::new(crate::error::Failure::new(kind, message))
}

/// A paced YouTube Data API client for one API key.
pub struct Client {
    agent: ureq::Agent,
    base: String,
    key: String,
    timing: Timing,
    pacer: Pacer,
    requests: u32,
    /// Receives each warning line; [`http::print_warning`] writes it to
    /// stderr.
    warn: fn(&str),
}

impl Client {
    /// A client for `base_url` (such as [`DEFAULT_BASE_URL`]) sending `key`.
    pub fn new(base_url: &str, key: &str, timing: Timing) -> Self {
        Self {
            agent: http::agent(),
            base: base_url.trim_end_matches('/').to_owned(),
            key: key.to_owned(),
            timing,
            pacer: Pacer::new(timing.min_interval),
            requests: 0,
            warn: http::print_warning,
        }
    }

    /// A live-paced client for `$SSS_YOUTUBE_API_BASE_URL`, else the live
    /// API.
    pub fn from_env(key: &str) -> Self {
        let base = http::base_url_from_env(BASE_URL_ENV, DEFAULT_BASE_URL);
        Self::new(&base, key, Timing::LIVE)
    }

    /// Responses received so far, retried attempts included.
    pub fn requests(&self) -> u32 {
        self.requests
    }

    /// Checks the key with one `videos.list` for [`VERIFY_VIDEO_ID`]. The
    /// response is not stored: it says nothing about any profile.
    pub fn verify(&mut self) -> Result<()> {
        let request = Request {
            ids: vec![VERIFY_VIDEO_ID.to_owned()],
            part: "id".to_owned(),
        };
        self.videos_list(&request, &mut |_| Ok(())).map(|_| ())
    }

    /// `videos.list` for `ids` (at most [`CHUNK`]). Every response, retried
    /// ones included, is passed to `record` before it is parsed. Returns the
    /// videos found.
    pub fn videos(
        &mut self,
        ids: &[String],
        record: &mut dyn FnMut(&Exchange) -> Result<()>,
    ) -> Result<Vec<ApiVideo>> {
        assert!(ids.len() <= CHUNK, "videos.list takes at most {CHUNK} ids");
        let request = Request {
            ids: ids.to_vec(),
            part: PART.to_owned(),
        };
        self.videos_list(&request, record)
    }

    fn videos_list(
        &mut self,
        request: &Request,
        record: &mut dyn FnMut(&Exchange) -> Result<()>,
    ) -> Result<Vec<ApiVideo>> {
        let mut backoff = self.timing.first_backoff;
        for attempt in 1..=MAX_TRIES {
            if attempt > 1 {
                std::thread::sleep(backoff);
                backoff = backoff.saturating_mul(2);
            }
            self.pacer.pace();
            let at = repo::now_rfc3339()?;
            let reply = match self.send(request) {
                Ok(reply) => reply,
                Err(e) if attempt < MAX_TRIES => {
                    (self.warn)(&format!(
                        "warning: GET {VIDEOS_PATH}: no response on attempt \
                         {attempt}/{MAX_TRIES} ({e:#}); retrying"
                    ));
                    continue;
                }
                Err(e) => {
                    return Err(e).with_kind(Kind::Unavailable, || {
                        format!("GET {VIDEOS_PATH}: no response after {MAX_TRIES} tries")
                    });
                }
            };
            self.requests += 1;
            let (parsed, response) = http::stored_body(&reply.body)?;
            record(&Exchange {
                at,
                method: "GET".to_owned(),
                path: VIDEOS_PATH.to_owned(),
                request: request.clone(),
                status: reply.status,
                response,
            })?;
            let status = reply.status;
            if (500..600).contains(&status) {
                if attempt < MAX_TRIES {
                    (self.warn)(&format!(
                        "warning: GET {VIDEOS_PATH}: {} on attempt {attempt}/{MAX_TRIES}; \
                         retrying",
                        describe(status, parsed.as_ref())
                    ));
                    continue;
                }
                fail!(
                    Kind::Unavailable,
                    "GET {VIDEOS_PATH}: {} after {MAX_TRIES} tries",
                    describe(status, parsed.as_ref())
                );
            }
            if !(200..300).contains(&status) {
                return Err(refusal(status, parsed.as_ref()));
            }
            return parsed.as_ref().and_then(parse_list).ok_or_else(|| {
                anyhow!("GET {VIDEOS_PATH}: HTTP {status}, the response is not a video list")
            });
        }
        unreachable!("the last attempt always returns")
    }

    /// One HTTP exchange. A transport error's text has the key replaced, as
    /// it may quote the request address.
    fn send(&self, request: &Request) -> Result<http::Reply> {
        let ids = request.ids.join(",");
        let url = format!("{}/{VIDEOS_PATH}", self.base);
        let result = self
            .agent
            .get(&url)
            .query("part", &request.part)
            .query("id", &ids)
            .query("key", &self.key)
            .set("Accept", "application/json")
            .call();
        http::read(result).map_err(|e| anyhow!("{}", self.redact(&format!("{e:#}"))))
    }

    fn redact(&self, text: &str) -> String {
        if self.key.is_empty() {
            return text.to_owned();
        }
        let encoded: String = url::form_urlencoded::byte_serialize(self.key.as_bytes()).collect();
        text.replace(&self.key, "<key>").replace(&encoded, "<key>")
    }
}

/// The cache directory's files, as `(sequence number, path)`, in order.
fn cache_files(dir: &Path) -> Result<Vec<(u64, PathBuf)>> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let name = entry.file_name();
        if let Some(seq) = name
            .to_str()
            .and_then(|n| n.strip_suffix(CACHE_SUFFIX))
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|n| n.parse::<u64>().ok())
        {
            files.push((seq, entry.path()));
        }
    }
    files.sort();
    Ok(files)
}

/// The highest sequence number in the cache `dir`; 0 when it has none.
pub fn last_seq(dir: &Path) -> Result<u64> {
    Ok(cache_files(dir)?.last().map_or(0, |(n, _)| *n))
}

/// Writes `exchange` into the cache `dir` as the first free
/// `NNNN-videos-list.json` after `*seq`, never over an existing file, and
/// sets `*seq` to its number.
pub fn record(dir: &Path, seq: &mut u64, exchange: &Exchange) -> Result<PathBuf> {
    let contents = repo::to_json(exchange)?;
    loop {
        *seq += 1;
        let path = dir.join(format!("{:04}{CACHE_SUFFIX}", *seq));
        if repo::write_new_atomic(&path, contents.as_bytes())? {
            return Ok(path);
        }
    }
}

/// The stored fields of a cached exchange that reading needs.
#[derive(Deserialize)]
struct Stored {
    request: Request,
    status: u16,
    response: Value,
}

/// The latest cached answer for every video the cache `dir` names: files
/// read in sequence order, a later answer replacing an earlier one. Only
/// 2xx responses that parse as a video list answer anything; a refused or
/// unreadable one leaves its ids unasked.
pub fn read_cache(dir: &Path) -> Result<BTreeMap<String, Cached>> {
    let mut cache = BTreeMap::new();
    for (_, path) in cache_files(dir)? {
        let stored: Stored = repo::read_json(&path)?;
        if !(200..300).contains(&stored.status) {
            continue;
        }
        let Some(videos) = parse_list(&stored.response) else {
            continue;
        };
        let mut found: BTreeMap<String, ApiVideo> = videos
            .into_iter()
            .map(|v| (v.video_id.clone(), v))
            .collect();
        for id in stored.request.ids {
            let answer = found.remove(&id).map_or(Cached::Absent, Cached::Found);
            cache.insert(id, answer);
        }
    }
    Ok(cache)
}

/// What one [`fetch`] got.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Fetched {
    /// Ids asked for.
    pub requested: usize,
    /// Ids the API returned.
    pub found: usize,
    /// Ids asked for and not returned.
    pub unavailable: usize,
}

/// `videos.list` for `ids` in chunks of [`CHUNK`], each response written to
/// the cache `dir` before it is parsed. An error stops the fetch; what was
/// fetched before it stays in the cache.
pub fn fetch(client: &mut Client, dir: &Path, ids: &[String]) -> Result<Fetched> {
    let mut fetched = Fetched::default();
    let mut seq = last_seq(dir)?;
    for chunk in ids.chunks(CHUNK) {
        let videos = client.videos(chunk, &mut |exchange| {
            record(dir, &mut seq, exchange).map(|_| ())
        })?;
        let found = chunk
            .iter()
            .filter(|id| videos.iter().any(|v| &v.video_id == *id))
            .count();
        fetched.requested += chunk.len();
        fetched.found += found;
        fetched.unavailable += chunk.len() - found;
    }
    Ok(fetched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;

    const KEY: &str = "AIzaTestKey_0123456789";
    const FAST: Timing = Timing {
        min_interval: Duration::ZERO,
        first_backoff: Duration::from_millis(1),
    };

    fn client(server: &MockServer) -> Client {
        let mut c = Client::new(&server.url("/youtube/v3"), KEY, FAST);
        c.warn = |_| {};
        c
    }

    fn id(n: usize) -> String {
        format!("vid{n:08}")
    }

    fn item(id: &str) -> Value {
        json!({
            "kind": "youtube#video",
            "id": id,
            "snippet": {"title": "Calm Waters", "channelId": "UCtttttttttttttttttttttt",
                        "channelTitle": "Chill Out Radio - Topic", "categoryId": "10"},
            "contentDetails": {"duration": "PT3M40S"},
            "topicDetails": {"topicCategories": ["https://en.wikipedia.org/wiki/Pop_music"]}
        })
    }

    fn error_body(status: u16, reason: &str) -> Value {
        json!({"error": {"code": status, "message": "refused",
                         "errors": [{"reason": reason, "domain": "x"}]}})
    }

    fn cache_text(dir: &Path) -> String {
        cache_files(dir)
            .unwrap()
            .iter()
            .map(|(_, p)| fs::read_to_string(p).unwrap())
            .collect()
    }

    #[test]
    fn durations_parse_from_iso_8601() {
        for (iso, want) in [
            ("PT3M40S", Some(220)),
            ("PT45S", Some(45)),
            ("PT1H", Some(3600)),
            ("PT1H2M3S", Some(3723)),
            ("P1DT2H", Some(93_600)),
            ("P1W", Some(604_800)),
            ("P0D", None),
            ("PT0S", None),
            ("PT", None),
            ("T3M", None),
            ("PT3S4M", None),
            ("PT3.5S", None),
            ("PTM", None),
            ("", None),
        ] {
            assert_eq!(duration_s(iso), want, "{iso}");
        }
    }

    #[test]
    fn requests_are_chunked_at_fifty_and_cached_without_the_key() {
        let server = MockServer::start();
        let ids: Vec<String> = (0..60).map(id).collect();
        let first = server.mock(|when, then| {
            when.method(GET)
                .path("/youtube/v3/videos")
                .query_param("part", PART)
                .query_param("id", ids[..50].join(","))
                .query_param("key", KEY);
            then.status(200)
                .json_body(json!({"items": ids[..50].iter().map(|i| item(i)).collect::<Vec<_>>()}));
        });
        let second = server.mock(|when, then| {
            when.method(GET)
                .path("/youtube/v3/videos")
                .query_param("id", ids[50..].join(","));
            then.status(200)
                .json_body(json!({"items": [item(&ids[50]), item(&ids[59])]}));
        });
        let tmp = tempfile::tempdir().unwrap();
        let mut c = client(&server);
        let got = fetch(&mut c, tmp.path(), &ids).unwrap();
        first.assert();
        second.assert();
        assert_eq!(
            got,
            Fetched {
                requested: 60,
                found: 52,
                unavailable: 8
            }
        );
        let files: Vec<String> = cache_files(tmp.path())
            .unwrap()
            .iter()
            .map(|(_, p)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files, ["0001-videos-list.json", "0002-videos-list.json"]);
        let text = cache_text(tmp.path());
        assert!(!text.contains(KEY));
        let stored: Value = serde_json::from_str(
            &fs::read_to_string(tmp.path().join("0002-videos-list.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(stored["method"], "GET");
        assert_eq!(stored["path"], "videos");
        assert_eq!(stored["request"]["ids"].as_array().unwrap().len(), 10);
        assert_eq!(stored["request"]["part"], PART);
        assert_eq!(stored["status"], 200);
        assert_eq!(stored["response"]["items"][1]["id"], ids[59].as_str());

        let cache = read_cache(tmp.path()).unwrap();
        assert_eq!(cache.len(), 60);
        assert_eq!(cache[&ids[51]], Cached::Absent);
        let Cached::Found(v) = &cache[&ids[59]] else {
            panic!("{:?}", cache[&ids[59]]);
        };
        assert_eq!(
            v,
            &ApiVideo {
                video_id: ids[59].clone(),
                title: Some("Calm Waters".into()),
                channel_id: Some("UCtttttttttttttttttttttt".into()),
                channel_title: Some("Chill Out Radio - Topic".into()),
                category_id: Some("10".into()),
                duration_s: Some(220),
                topics: vec!["https://en.wikipedia.org/wiki/Pop_music".into()],
            }
        );
    }

    #[test]
    fn numbering_continues_from_the_highest_file_and_the_latest_answer_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let write = |name: &str, ids: &[&str], status: u16, response: Value| {
            let exchange = json!({"at": "2026-01-01T00:00:00Z", "method": "GET",
                "path": "videos", "request": {"ids": ids, "part": PART},
                "status": status, "response": response});
            fs::write(dir.join(name), exchange.to_string()).unwrap();
        };
        let a = "aaaaaaaaaaa";
        let b = "bbbbbbbbbbb";
        write(
            "0007-videos-list.json",
            &[a, b],
            200,
            json!({"items": [item(a)]}),
        );
        write("0009-videos-list.json", &[a], 200, json!({"items": []}));
        write(
            "0010-videos-list.json",
            &[b],
            403,
            error_body(403, "quotaExceeded"),
        );
        write("0011-videos-list.json", &[b], 200, json!("not a list"));
        let c = "ccccccccccc";
        write("0013-videos-list.json", &[c], 200, json!({}));
        write("0014-videos-list.json", &[c], 200, json!({"status": "ok"}));
        fs::write(dir.join("notes.txt"), "x").unwrap();
        fs::write(dir.join("0012-videos-myRating-like.json"), "{}").unwrap();
        let mut found = item(b);
        found["snippet"]["title"] = json!("Second Answer");
        write(
            "10000-videos-list.json",
            &[b],
            200,
            json!({"items": [found]}),
        );

        let cache = read_cache(dir).unwrap();
        assert_eq!(cache[a], Cached::Absent);
        let Cached::Found(v) = &cache[b] else {
            panic!()
        };
        assert_eq!(v.title.as_deref(), Some("Second Answer"));
        // A 2xx body without `items` answers nothing: `c` is still unasked.
        assert!(!cache.contains_key(c), "{cache:?}");

        let exchange = Exchange {
            at: "2026-01-02T00:00:00Z".into(),
            method: "GET".into(),
            path: "videos".into(),
            request: Request {
                ids: vec![a.into()],
                part: PART.into(),
            },
            status: 200,
            response: RawValue::from_string("{\"items\":[]}".into()).unwrap(),
        };
        let mut seq = last_seq(dir).unwrap();
        let path = record(dir, &mut seq, &exchange).unwrap();
        assert!(path.ends_with("10001-videos-list.json"), "{path:?}");
        assert_eq!(seq, 10001);
        // A number taken behind the counter's back is skipped, not overwritten.
        fs::write(dir.join("10002-videos-list.json"), b"{}").unwrap();
        let path = record(dir, &mut seq, &exchange).unwrap();
        assert!(path.ends_with("10003-videos-list.json"), "{path:?}");
        assert_eq!(fs::read(dir.join("10002-videos-list.json")).unwrap(), b"{}");
    }

    #[test]
    fn refusals_map_to_kinds() {
        for (status, reason, kind) in [
            (403, "quotaExceeded", Kind::Quota),
            (403, "rateLimitExceeded", Kind::Quota),
            (429, "other", Kind::Quota),
            (400, "keyInvalid", Kind::Input),
            (400, "badRequest", Kind::Other),
            (403, "accessNotConfigured", Kind::Config),
            (404, "notFound", Kind::Other),
        ] {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(GET).path("/youtube/v3/videos");
                then.status(status).json_body(error_body(status, reason));
            });
            let err = client(&server)
                .videos(&[id(1)], &mut |_| Ok(()))
                .unwrap_err();
            assert_eq!(crate::error::kind(&err), kind, "{status} {reason}: {err:#}");
            assert!(format!("{err:#}").contains(reason), "{err:#}");
            assert!(!format!("{err:#}").contains(KEY));
        }
        // Google's current form names the invalid key in `details`.
        let body = json!({"error": {"code": 400, "message": "API key not valid.",
            "errors": [{"reason": "badRequest"}],
            "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo",
                         "reason": "API_KEY_INVALID"}]}});
        assert_eq!(crate::error::kind(&refusal(400, Some(&body))), Kind::Input);
    }

    #[test]
    fn outages_are_retried_then_unavailable_and_every_response_is_recorded() {
        let server = MockServer::start();
        let outage = server.mock(|when, then| {
            when.method(GET).path("/youtube/v3/videos");
            then.status(503).body("busy");
        });
        let mut recorded = 0;
        let err = client(&server)
            .videos(&[id(1)], &mut |e| {
                assert_eq!(e.status, 503);
                recorded += 1;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Unavailable);
        outage.assert_calls(MAX_TRIES as usize);
        assert_eq!(recorded, MAX_TRIES);
    }

    #[test]
    fn no_response_is_unavailable_and_the_message_hides_the_key() {
        static WARNINGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let mut c = Client::new("http://127.0.0.1:9/youtube/v3", KEY, FAST);
        c.warn = |line| WARNINGS.lock().unwrap().push(line.to_owned());
        let err = c.videos(&[id(1)], &mut |_| Ok(())).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Unavailable);
        assert!(!format!("{err:#}").contains(KEY), "{err:#}");
        let lines = WARNINGS.lock().unwrap();
        assert_eq!(lines.len(), MAX_TRIES as usize - 1);
        assert!(lines.iter().all(|l| !l.contains(KEY)), "{lines:?}");
        assert_eq!(c.requests(), 0);
    }

    #[test]
    fn a_quota_stop_keeps_what_was_fetched() {
        let server = MockServer::start();
        let ids: Vec<String> = (0..51).map(id).collect();
        server.mock(|when, then| {
            when.method(GET)
                .path("/youtube/v3/videos")
                .query_param("id", ids[..50].join(","));
            then.status(200)
                .json_body(json!({"items": [item(&ids[0])]}));
        });
        server.mock(|when, then| {
            when.method(GET)
                .path("/youtube/v3/videos")
                .query_param("id", ids[50].as_str());
            then.status(403).json_body(error_body(403, "quotaExceeded"));
        });
        let tmp = tempfile::tempdir().unwrap();
        let err = fetch(&mut client(&server), tmp.path(), &ids).unwrap_err();
        assert_eq!(crate::error::kind(&err), Kind::Quota);
        let cache = read_cache(tmp.path()).unwrap();
        assert_eq!(cache.len(), 50);
        assert!(!cache.contains_key(&ids[50]));
        assert_eq!(cache_files(tmp.path()).unwrap().len(), 2);
    }

    #[test]
    fn a_2xx_body_without_items_is_not_a_video_list() {
        for body in [json!({}), json!({"status": "ok"})] {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(GET).path("/youtube/v3/videos");
                then.status(200).json_body(body.clone());
            });
            let tmp = tempfile::tempdir().unwrap();
            let err = fetch(&mut client(&server), tmp.path(), &[id(1)]).unwrap_err();
            assert!(
                format!("{err:#}").contains("the response is not a video list"),
                "{err:#}"
            );
            assert_eq!(cache_files(tmp.path()).unwrap().len(), 1);
            assert!(read_cache(tmp.path()).unwrap().is_empty());
        }
    }

    #[test]
    fn verify_asks_for_one_public_video_and_records_nothing() {
        let server = MockServer::start();
        let m = server.mock(|when, then| {
            when.method(GET)
                .path("/youtube/v3/videos")
                .query_param("part", "id")
                .query_param("id", VERIFY_VIDEO_ID)
                .query_param("key", KEY);
            then.status(200)
                .json_body(json!({"items": [{"id": VERIFY_VIDEO_ID}]}));
        });
        client(&server).verify().unwrap();
        m.assert();
    }
}
