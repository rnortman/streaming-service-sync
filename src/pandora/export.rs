//! `sss pandora export`: one capture run. Every response is written
//! verbatim to `raw/<run-id>/NNNN-<endpoint>.json` as it arrives, before
//! anything reads it; `manifest.json` records each step's outcome when the
//! run ends, and the run is `complete` only when every step is `ok`.

use std::collections::HashSet;
use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::pandora::client::{Client, Reply, endpoint};
use crate::pandora::model::Summary;
use crate::repo::{self, ProfilePaths};

/// Page size of every paged request; the largest Pandora accepts.
pub const PAGE: usize = 100;

/// The capture steps, in run order. Each discriminant is the step's index
/// in [`Step::ALL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Stations,
    StationDetails,
    FeedbackUp,
    FeedbackDown,
    Playlists,
    PlaylistTracks,
    Collection,
    Bookmarks,
}

impl Step {
    pub const ALL: [Step; 8] = [
        Step::Stations,
        Step::StationDetails,
        Step::FeedbackUp,
        Step::FeedbackDown,
        Step::Playlists,
        Step::PlaylistTracks,
        Step::Collection,
        Step::Bookmarks,
    ];

    /// The step's key in the manifest.
    pub fn name(self) -> &'static str {
        match self {
            Step::Stations => "stations",
            Step::StationDetails => "station_details",
            Step::FeedbackUp => "feedback_up",
            Step::FeedbackDown => "feedback_down",
            Step::Playlists => "playlists",
            Step::PlaylistTracks => "playlist_tracks",
            Step::Collection => "collection",
            Step::Bookmarks => "bookmarks",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Ok,
    Failed,
}

/// One step's line in the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepResult {
    pub status: StepStatus,
    /// Records captured.
    pub count: u64,
    /// Why the step failed: the first failure, and how many more followed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Default for StepResult {
    fn default() -> Self {
        Self {
            status: StepStatus::Ok,
            count: 0,
            error: None,
        }
    }
}

/// The ledger, one entry per [`Step`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Steps {
    pub stations: StepResult,
    pub station_details: StepResult,
    pub feedback_up: StepResult,
    pub feedback_down: StepResult,
    pub playlists: StepResult,
    pub playlist_tracks: StepResult,
    pub collection: StepResult,
    pub bookmarks: StepResult,
}

impl Steps {
    pub fn get(&self, step: Step) -> &StepResult {
        match step {
            Step::Stations => &self.stations,
            Step::StationDetails => &self.station_details,
            Step::FeedbackUp => &self.feedback_up,
            Step::FeedbackDown => &self.feedback_down,
            Step::Playlists => &self.playlists,
            Step::PlaylistTracks => &self.playlist_tracks,
            Step::Collection => &self.collection,
            Step::Bookmarks => &self.bookmarks,
        }
    }

    fn get_mut(&mut self, step: Step) -> &mut StepResult {
        match step {
            Step::Stations => &mut self.stations,
            Step::StationDetails => &mut self.station_details,
            Step::FeedbackUp => &mut self.feedback_up,
            Step::FeedbackDown => &mut self.feedback_down,
            Step::Playlists => &mut self.playlists,
            Step::PlaylistTracks => &mut self.playlist_tracks,
            Step::Collection => &mut self.collection,
            Step::Bookmarks => &mut self.bookmarks,
        }
    }

    /// The steps that failed, in run order.
    pub fn failed(&self) -> Vec<Step> {
        Step::ALL
            .into_iter()
            .filter(|&s| self.get(s).status == StepStatus::Failed)
            .collect()
    }
}

/// `raw/<run-id>/manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub run_id: String,
    /// RFC 3339 UTC.
    pub started: String,
    /// RFC 3339 UTC.
    pub finished: String,
    /// HTTP responses received, retried attempts included; one raw file each.
    pub requests: u32,
    pub steps: Steps,
    /// `true` only when every step is `ok`.
    pub complete: bool,
}

/// `v1/station/getStations` → `v1-station-getStations`.
fn slug(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The newest run whose manifest says `complete`.
pub fn latest_complete_run(paths: &ProfilePaths) -> Result<Option<Manifest>> {
    for id in repo::run_ids(&paths.raw_dir())?.iter().rev() {
        if let Some(m) = repo::read_manifest::<Manifest>(&paths.raw_run_dir(id))?
            && m.complete
        {
            return Ok(Some(m));
        }
    }
    Ok(None)
}

/// The array under `key` in a response object.
fn array<'v>(body: &'v Value, key: &str) -> Option<&'v Vec<Value>> {
    body.get(key)?.as_array()
}

/// The string under `key` in a record.
fn string<'v>(record: &'v Value, key: &str) -> Option<&'v str> {
    record.get(key)?.as_str()
}

/// Whether to request the page after one that started at `start` (zero
/// based), returned `len` records of which `new` were not seen before, in a
/// listing that reports `total` records when it says.
fn more_pages(start: usize, len: usize, new: usize, total: Option<u64>) -> bool {
    if len == 0 || new == 0 {
        return false;
    }
    match total {
        Some(total) => ((start + len) as u64) < total,
        None => len >= PAGE,
    }
}

/// A station as the later steps need it.
struct StationRef {
    /// `stationId` exactly as the station list gave it.
    id: Value,
    shuffle: bool,
}

/// One capture run in progress.
struct Run<'a> {
    client: &'a mut Client,
    dir: PathBuf,
    seq: u32,
    steps: Steps,
    /// Failures after the first, per step (the first is the step's `error`).
    more_failures: [u32; Step::ALL.len()],
    finished: [bool; Step::ALL.len()],
}

impl Run<'_> {
    /// Sends one request belonging to `step`, writing every response to the
    /// run directory first. `None` when the request failed; the failure is
    /// then recorded on `step`. An error stops the run.
    fn post(&mut self, step: Step, path: &str, body: Value) -> Result<Option<Value>> {
        let dir = &self.dir;
        let seq = &mut self.seq;
        let reply = self.client.post(path, &body, &mut |exchange| {
            *seq += 1;
            let file = dir.join(format!("{:04}-{}.json", *seq, slug(path)));
            repo::write_json(&file, exchange)
        })?;
        match reply {
            Reply::Ok(value) => Ok(Some(value)),
            Reply::Failed(why) | Reply::Unavailable(why) => {
                self.fail(step, format!("{path}: {why}"));
                Ok(None)
            }
        }
    }

    /// Marks `step` failed, keeping the first reason.
    fn fail(&mut self, step: Step, why: String) {
        let result = self.steps.get_mut(step);
        result.status = StepStatus::Failed;
        if result.error.is_none() {
            result.error = Some(why);
        } else {
            self.more_failures[step as usize] += 1;
        }
    }

    fn count(&mut self, step: Step, n: usize) {
        self.steps.get_mut(step).count += n as u64;
    }

    /// Marks `step` failed because the step it works from failed.
    fn depends_on(&mut self, step: Step, prerequisite: Step) {
        if self.steps.get(prerequisite).status == StepStatus::Failed {
            self.fail(
                step,
                format!(
                    "incomplete: the `{}` step it works from failed",
                    prerequisite.name()
                ),
            );
        }
    }

    /// Closes `step` and prints its ledger line.
    fn finish(&mut self, step: Step, out: &mut dyn Write) {
        self.finished[step as usize] = true;
        let extra = self.more_failures[step as usize];
        let result = self.steps.get_mut(step);
        if extra > 0
            && let Some(error) = &mut result.error
        {
            error.push_str(&format!(" (and {extra} more failed requests)"));
        }
        write_step_line(out, step, result)
    }

    fn capture(&mut self, out: &mut dyn Write) -> Result<()> {
        let stations = self.stations()?;
        self.finish(Step::Stations, out);

        for station in &stations {
            let body = json!({"stationId": station.id, "isCurrentStation": false});
            if self
                .post(Step::StationDetails, endpoint::GET_STATION_DETAILS, body)?
                .is_some()
            {
                self.count(Step::StationDetails, 1);
            }
        }
        self.depends_on(Step::StationDetails, Step::Stations);
        self.finish(Step::StationDetails, out);

        for (step, positive) in [(Step::FeedbackUp, true), (Step::FeedbackDown, false)] {
            for station in stations.iter().filter(|s| !s.shuffle) {
                self.feedback(step, &station.id, positive)?;
            }
            self.depends_on(step, Step::Stations);
            self.finish(step, out);
        }

        let playlists = self.playlists()?;
        self.finish(Step::Playlists, out);
        for id in &playlists {
            self.playlist_tracks(id)?;
        }
        self.depends_on(Step::PlaylistTracks, Step::Playlists);
        self.finish(Step::PlaylistTracks, out);

        self.collection()?;
        self.finish(Step::Collection, out);

        for kind in ["track", "album"] {
            let body = json!({"type": kind});
            if let Some(reply) = self.post(Step::Bookmarks, endpoint::GET_BOOKMARKS, body)? {
                let n = reply
                    .as_object()
                    .map(|o| o.values().filter_map(Value::as_array).map(Vec::len).sum())
                    .unwrap_or(0);
                self.count(Step::Bookmarks, n);
            }
        }
        self.finish(Step::Bookmarks, out);
        Ok(())
    }

    /// `v1/station/getStations`, paged by `startIndex`.
    fn stations(&mut self) -> Result<Vec<StationRef>> {
        const PATH: &str = endpoint::GET_STATIONS;
        let mut stations = Vec::new();
        let mut seen = HashSet::new();
        let mut start = 0;
        loop {
            let body = json!({"pageSize": PAGE, "startIndex": start});
            let Some(reply) = self.post(Step::Stations, PATH, body)? else {
                break;
            };
            let Some(page) = array(&reply, "stations") else {
                self.fail(Step::Stations, format!("{PATH}: no `stations` array"));
                break;
            };
            let mut new = 0;
            for station in page {
                let Some(id) = station.get("stationId").filter(|v| !v.is_null()) else {
                    self.fail(
                        Step::Stations,
                        format!("{PATH}: a station has no `stationId`"),
                    );
                    continue;
                };
                if seen.insert(id.to_string()) {
                    new += 1;
                    stations.push(StationRef {
                        id: id.clone(),
                        shuffle: station.get("isShuffle").and_then(Value::as_bool) == Some(true),
                    });
                }
            }
            let total = reply.get("totalStations").and_then(Value::as_u64);
            if !more_pages(start, page.len(), new, total) {
                break;
            }
            start += page.len();
        }
        self.count(Step::Stations, stations.len());
        Ok(stations)
    }

    /// `v1/station/getStationFeedback` for one station and polarity, paged
    /// by `startIndex` until an empty page, the reported total, or a page
    /// with no new `feedbackId`.
    fn feedback(&mut self, step: Step, station: &Value, positive: bool) -> Result<()> {
        const PATH: &str = endpoint::GET_STATION_FEEDBACK;
        let mut seen = HashSet::new();
        let mut start = 0;
        loop {
            let body = json!({
                "stationId": station,
                "positive": positive,
                "pageSize": PAGE,
                "startIndex": start,
            });
            let Some(reply) = self.post(step, PATH, body)? else {
                break;
            };
            let Some(page) = array(&reply, "feedback") else {
                self.fail(step, format!("{PATH}: no `feedback` array"));
                break;
            };
            let new = page
                .iter()
                .filter(|record| {
                    let key = match record.get("feedbackId") {
                        Some(id) if !id.is_null() => id.to_string(),
                        _ => record.to_string(),
                    };
                    seen.insert(key)
                })
                .count();
            let total = reply.get("total").and_then(Value::as_u64);
            if !more_pages(start, page.len(), new, total) {
                break;
            }
            start += page.len();
        }
        self.count(step, seen.len());
        Ok(())
    }

    /// `v6/collections/getSortedPlaylists`, paged by `offset`; the
    /// playlists' `PL:` ids in listing order.
    fn playlists(&mut self) -> Result<Vec<String>> {
        const PATH: &str = endpoint::GET_SORTED_PLAYLISTS;
        let mut ids = Vec::new();
        let mut seen = HashSet::new();
        let mut offset = 0;
        loop {
            let body = json!({"request": {
                "sortOrder": "MOST_RECENT_MODIFIED",
                "offset": offset,
                "limit": PAGE,
                "annotationLimit": PAGE,
            }});
            let Some(reply) = self.post(Step::Playlists, PATH, body)? else {
                break;
            };
            let Some(page) = array(&reply, "items") else {
                self.fail(Step::Playlists, format!("{PATH}: no `items` array"));
                break;
            };
            let mut new = 0;
            for item in page {
                let Some(id) = string(item, "pandoraId") else {
                    self.fail(
                        Step::Playlists,
                        format!("{PATH}: an item has no `pandoraId`"),
                    );
                    continue;
                };
                if seen.insert(id.to_owned()) {
                    new += 1;
                    ids.push(id.to_owned());
                }
            }
            if !more_pages(offset, page.len(), new, None) {
                break;
            }
            offset += page.len();
        }
        self.count(Step::Playlists, ids.len());
        Ok(ids)
    }

    /// `v7/playlists/getTracks` for one playlist, paged by `offset`. The
    /// first page is requested without `playlistVersion`; every later page
    /// sends the `version` the first page returned.
    fn playlist_tracks(&mut self, playlist: &str) -> Result<()> {
        const PATH: &str = endpoint::GET_PLAYLIST_TRACKS;
        let step = Step::PlaylistTracks;
        let mut version: Option<Value> = None;
        let mut offset = 0;
        loop {
            let mut request = json!({
                "pandoraId": playlist,
                "offset": offset,
                "limit": PAGE,
                "annotationLimit": PAGE,
            });
            if offset > 0 {
                let Some(v) = &version else {
                    self.fail(
                        step,
                        format!("{PATH}: {playlist}: no `version` to page with"),
                    );
                    break;
                };
                request["playlistVersion"] = v.clone();
            }
            let Some(reply) = self.post(step, PATH, json!({"request": request}))? else {
                break;
            };
            if offset == 0 {
                version = reply.get("version").filter(|v| !v.is_null()).cloned();
            }
            let Some(page) = array(&reply, "tracks") else {
                self.fail(step, format!("{PATH}: {playlist}: no `tracks` array"));
                break;
            };
            let len = page.len();
            self.count(step, len);
            let total = reply.get("totalTracks").and_then(Value::as_u64);
            if !more_pages(offset, len, len, total) {
                break;
            }
            offset += len;
        }
        Ok(())
    }

    /// `v6/collections/getItems`, paged by cursor, then
    /// `v4/catalog/annotateObjects` for every non-playlist item, 100 ids per
    /// request.
    fn collection(&mut self) -> Result<()> {
        const PATH: &str = endpoint::GET_COLLECTION_ITEMS;
        const ANNOTATE: &str = endpoint::ANNOTATE_OBJECTS;
        let step = Step::Collection;
        let mut ids: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        let mut cursor: Option<Value> = None;
        loop {
            let mut request = json!({"limit": PAGE});
            if let Some(c) = &cursor {
                request["cursor"] = c.clone();
            }
            let Some(reply) = self.post(step, PATH, json!({"request": request}))? else {
                break;
            };
            let Some(page) = array(&reply, "items") else {
                self.fail(step, format!("{PATH}: no `items` array"));
                break;
            };
            for item in page {
                let Some(id) = string(item, "pandoraId") else {
                    self.fail(step, format!("{PATH}: an item has no `pandoraId`"));
                    continue;
                };
                if !id.starts_with("PL:") && seen.insert(id.to_owned()) {
                    ids.push(id.to_owned());
                }
            }
            let next = reply
                .get("cursor")
                .filter(|c| !c.is_null() && c.as_str() != Some(""))
                .cloned();
            if page.is_empty() || next.is_none() || next == cursor {
                break;
            }
            cursor = next;
        }
        self.count(step, ids.len());
        for batch in ids.chunks(PAGE) {
            self.post(step, ANNOTATE, json!({"pandoraIds": batch}))?;
        }
        Ok(())
    }
}

/// The ledger line of `step`: name, `ok` or `FAILED`, count, and a failed
/// step's error.
pub fn step_line(step: Step, result: &StepResult) -> String {
    let line = match result.status {
        StepStatus::Ok => format!("{:<16} ok      {}", step.name(), result.count),
        StepStatus::Failed => format!(
            "{:<16} FAILED  {}  {}",
            step.name(),
            result.count,
            result.error.as_deref().unwrap_or("")
        ),
    };
    line.trim_end().to_owned()
}

/// Progress is for people: a failed write of it does not stop the run.
fn write_step_line(out: &mut dyn Write, step: Step, result: &StepResult) {
    let _ = writeln!(out, "{}", step_line(step, result)).and_then(|()| out.flush());
}

/// A run that stopped before its end: the session was rejected, Pandora
/// stopped answering, or a write failed. Its manifest is written; `source`
/// is why it stopped. It reads as `source` does: its text is that of
/// `source`'s outermost error and its [`std::error::Error::source`] is the
/// next one, so a chain holding it prints exactly as `source` would.
#[derive(Debug)]
pub struct ExportStopped {
    pub run_id: String,
    pub run_dir: PathBuf,
    pub source: anyhow::Error,
}

impl fmt::Display for ExportStopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.source)
    }
}

impl std::error::Error for ExportStopped {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
}

/// A run that ended with failed steps; its raw responses are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportIncomplete(pub Captured);

impl ExportIncomplete {
    /// The names of the failed steps, in run order.
    pub fn failed_steps(&self) -> Vec<&'static str> {
        self.0
            .manifest
            .steps
            .failed()
            .iter()
            .map(|s| s.name())
            .collect()
    }
}

impl fmt::Display for ExportIncomplete {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "run {} is incomplete: failed steps {}; its raw responses are kept",
            self.0.manifest.run_id,
            self.failed_steps().join(", ")
        )
    }
}

impl std::error::Error for ExportIncomplete {}

/// What `sss pandora export` reports for a complete run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportReport {
    pub profile: String,
    pub run_id: String,
    #[serde(flatten)]
    pub captured: Captured,
    pub normalized: Summary,
}

impl ExportReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        writeln!(out, "run {} complete", self.run_id)?;
        self.normalized.write_text(out)
    }
}

/// A completed capture: its directory and manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Captured {
    pub run_dir: PathBuf,
    pub manifest: Manifest,
}

/// Runs one capture into a new raw run of `paths` and writes its manifest,
/// writing the run directory and each step's ledger line to `progress` as
/// the step ends; a failed write to `progress` is ignored. An error before
/// the run directory exists is returned as it is. Any later error (the
/// session rejected, Pandora unreachable, a write failing) stops the run:
/// the manifest is still written, with every step that had not ended marked
/// failed, and the error is returned as an [`ExportStopped`] naming the run.
pub fn export(
    paths: &ProfilePaths,
    client: &mut Client,
    progress: &mut dyn Write,
) -> Result<Captured> {
    let started = repo::now_rfc3339()?;
    let (run_id, dir) = repo::create_run_dir(&paths.raw_dir(), &repo::run_id_now())?;
    match capture_run(run_id.clone(), dir.clone(), started, client, progress) {
        Ok(manifest) => Ok(Captured {
            run_dir: dir,
            manifest,
        }),
        Err(source) => Err(anyhow::Error::new(ExportStopped {
            run_id,
            run_dir: dir,
            source,
        })),
    }
}

/// The capture into the existing run directory `dir`.
fn capture_run(
    run_id: String,
    dir: PathBuf,
    started: String,
    client: &mut Client,
    progress: &mut dyn Write,
) -> Result<Manifest> {
    // Progress is for people: a failed write of it does not stop the run.
    let _ = writeln!(progress, "run {run_id}: {}", dir.display());
    let requests_before = client.requests();
    let mut run = Run {
        client,
        dir,
        seq: 0,
        steps: Steps::default(),
        more_failures: [0; Step::ALL.len()],
        finished: [false; Step::ALL.len()],
    };
    let outcome = run.capture(progress);
    if let Err(e) = &outcome {
        for step in Step::ALL {
            if !run.finished[step as usize] {
                run.fail(step, format!("not completed: the run stopped: {e}"));
                run.finished[step as usize] = true;
            }
        }
    }
    let manifest = Manifest {
        run_id,
        started,
        finished: repo::now_rfc3339()?,
        requests: run.client.requests() - requests_before,
        complete: outcome.is_ok() && run.steps.failed().is_empty(),
        steps: run.steps,
    };
    let written = repo::write_json(&run.dir.join(repo::MANIFEST_FILE), &manifest);
    outcome?;
    written?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pandora::client::SessionRejected;
    use crate::pandora::test_support::{FAST, session};
    use crate::repo::Repo;
    use httpmock::prelude::*;
    use std::fs;

    /// A mock answering `request` on `path` with `reply`.
    fn on<'a>(
        server: &'a MockServer,
        path: &str,
        request: Value,
        reply: Value,
    ) -> httpmock::Mock<'a> {
        server.mock(|when, then| {
            when.method(POST)
                .path(format!("/api/{path}"))
                .json_body(request);
            then.status(200).json_body(reply);
        })
    }

    fn stations_req(start: usize) -> Value {
        json!({"pageSize": PAGE, "startIndex": start})
    }

    fn details_req(id: &str) -> Value {
        json!({"stationId": id, "isCurrentStation": false})
    }

    fn feedback_req(id: &str, positive: bool, start: usize) -> Value {
        json!({"stationId": id, "positive": positive, "pageSize": PAGE, "startIndex": start})
    }

    fn playlists_req(offset: usize) -> Value {
        json!({"request": {"sortOrder": "MOST_RECENT_MODIFIED", "offset": offset,
                           "limit": PAGE, "annotationLimit": PAGE}})
    }

    fn tracks_req(id: &str, offset: usize, version: Option<u64>) -> Value {
        let mut r =
            json!({"pandoraId": id, "offset": offset, "limit": PAGE, "annotationLimit": PAGE});
        if let Some(v) = version {
            r["playlistVersion"] = json!(v);
        }
        json!({"request": r})
    }

    fn items_req(cursor: Option<&str>) -> Value {
        match cursor {
            Some(c) => json!({"request": {"limit": PAGE, "cursor": c}}),
            None => json!({"request": {"limit": PAGE}}),
        }
    }

    fn feedback(ids: &[&str]) -> Vec<Value> {
        ids.iter()
            .map(|id| json!({"feedbackId": id, "songTitle": "Song", "artistName": "Tee Grizzley"}))
            .collect()
    }

    /// Mocks for an account with no playlists, collection or bookmarks.
    fn empty_premium(server: &MockServer) {
        on(
            server,
            "v6/collections/getSortedPlaylists",
            playlists_req(0),
            json!({"items": []}),
        );
        on(
            server,
            "v6/collections/getItems",
            items_req(None),
            json!({"items": []}),
        );
        for kind in ["track", "album"] {
            on(
                server,
                "v1/bookmark/getBookmarks",
                json!({"type": kind}),
                json!({}),
            );
        }
    }

    /// Mocks for an account with no stations.
    fn no_stations(server: &MockServer) {
        on(
            server,
            "v1/station/getStations",
            stations_req(0),
            json!({"totalStations": 0, "stations": []}),
        );
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        paths: ProfilePaths,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Repo::at(tmp.path()).profile("alice").unwrap();
        Fixture { _tmp: tmp, paths }
    }

    fn run(server: &MockServer, fx: &Fixture) -> (Result<Manifest>, String) {
        let mut client = Client::new(&server.base_url(), &session(), FAST);
        let mut out = Vec::new();
        let result = export(&fx.paths, &mut client, &mut out);
        (result.map(|c| c.manifest), String::from_utf8(out).unwrap())
    }

    /// The single run directory and its files, sorted.
    fn run_files(fx: &Fixture) -> (PathBuf, Vec<String>) {
        let raw = fx.paths.pandora_dir().join("raw");
        let runs: Vec<_> = fs::read_dir(&raw)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(runs.len(), 1, "{runs:?}");
        let mut files: Vec<String> = fs::read_dir(&runs[0])
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        (runs[0].clone(), files)
    }

    fn manifest_on_disk(fx: &Fixture) -> Manifest {
        repo::read_manifest(&run_files(fx).0)
            .unwrap()
            .expect("manifest written")
    }

    #[test]
    fn a_full_run_captures_every_step_and_completes() {
        let server = MockServer::start();
        // Stations: two pages; the third station is a shuffle.
        on(
            &server,
            "v1/station/getStations",
            stations_req(0),
            json!({
                "totalStations": 3,
                "stations": [{"stationId": "11", "name": "Chill Out Radio"}, {"stationId": "12"}],
            }),
        );
        on(
            &server,
            "v1/station/getStations",
            stations_req(2),
            json!({
                "totalStations": 3,
                "stations": [{"stationId": "13", "isShuffle": true}],
            }),
        );
        for id in ["11", "12", "13"] {
            on(
                &server,
                "v1/station/getStationDetails",
                details_req(id),
                json!({"seeds": []}),
            );
        }
        // Station 11 has three thumbs up over two pages.
        on(
            &server,
            "v1/station/getStationFeedback",
            feedback_req("11", true, 0),
            json!({"total": 3, "feedback": feedback(&["f1", "f2"])}),
        );
        on(
            &server,
            "v1/station/getStationFeedback",
            feedback_req("11", true, 2),
            json!({"total": 3, "feedback": feedback(&["f3"])}),
        );
        on(
            &server,
            "v1/station/getStationFeedback",
            feedback_req("11", false, 0),
            json!({"total": 1, "feedback": feedback(&["f4"])}),
        );
        for positive in [true, false] {
            on(
                &server,
                "v1/station/getStationFeedback",
                feedback_req("12", positive, 0),
                json!({"total": 0, "feedback": []}),
            );
        }
        let shuffle_feedback = server.mock(|when, then| {
            when.path("/api/v1/station/getStationFeedback")
                .body_includes("\"13\"");
            then.status(500);
        });
        // Two playlists; PL:1 has three tracks over two pages.
        on(
            &server,
            "v6/collections/getSortedPlaylists",
            playlists_req(0),
            json!({
                "items": [{"pandoraId": "PL:1"}, {"pandoraId": "PL:2"}],
                "annotations": {"PL:1": {"name": "Mine"}, "PL:2": {"name": "Station thumbs", "linkedType": "StationThumbs"}},
            }),
        );
        on(
            &server,
            "v7/playlists/getTracks",
            tracks_req("PL:1", 0, None),
            json!({"version": 7, "totalTracks": 3, "tracks": [{"pandoraId": "TR:1"}, {"pandoraId": "TR:2"}]}),
        );
        let versioned = on(
            &server,
            "v7/playlists/getTracks",
            tracks_req("PL:1", 2, Some(7)),
            json!({"version": 7, "totalTracks": 3, "tracks": [{"pandoraId": "TR:3"}]}),
        );
        on(
            &server,
            "v7/playlists/getTracks",
            tracks_req("PL:2", 0, None),
            json!({"version": 2, "totalTracks": 1, "tracks": [{"pandoraId": "TR:1"}]}),
        );
        // Collection: two cursor pages; the PL item is not counted or annotated.
        on(
            &server,
            "v6/collections/getItems",
            items_req(None),
            json!({
                "items": [{"pandoraId": "PL:1"}, {"pandoraId": "TR:9"}, {"pandoraId": "AL:5"}],
                "cursor": "c2",
            }),
        );
        on(
            &server,
            "v6/collections/getItems",
            items_req(Some("c2")),
            json!({"items": [{"pandoraId": "TR:8"}]}),
        );
        let annotate = on(
            &server,
            "v4/catalog/annotateObjects",
            json!({"pandoraIds": ["TR:9", "AL:5", "TR:8"]}),
            json!({"TR:9": {"name": "Song"}}),
        );
        on(
            &server,
            "v1/bookmark/getBookmarks",
            json!({"type": "track"}),
            json!({"tracks": [{"pandoraId": "TR:1"}, {"pandoraId": "TR:2"}]}),
        );
        on(
            &server,
            "v1/bookmark/getBookmarks",
            json!({"type": "album"}),
            json!({"albums": []}),
        );

        let fx = fixture();
        let (result, out) = run(&server, &fx);
        let manifest = result.unwrap();
        shuffle_feedback.assert_calls(0);
        versioned.assert_calls(1);
        annotate.assert_calls(1);

        let counts: Vec<(StepStatus, u64)> = Step::ALL
            .iter()
            .map(|&s| (manifest.steps.get(s).status, manifest.steps.get(s).count))
            .collect();
        use StepStatus::Ok as K;
        assert_eq!(
            counts,
            [
                (K, 3),
                (K, 3),
                (K, 3),
                (K, 1),
                (K, 2),
                (K, 4),
                (K, 3),
                (K, 2)
            ]
        );
        assert!(manifest.complete, "{manifest:?}");
        assert_eq!(manifest.requests, 19);
        assert_eq!(manifest_on_disk(&fx), manifest);
        assert!(out.contains("playlist_tracks  ok      4"), "{out}");

        let (dir, files) = run_files(&fx);
        assert_eq!(
            files.len(),
            20,
            "one file per response plus the manifest: {files:?}"
        );
        assert_eq!(files[0], "0001-v1-station-getStations.json");
        assert_eq!(files[19], repo::MANIFEST_FILE);
        let first: Value = serde_json::from_slice(&fs::read(dir.join(&files[0])).unwrap()).unwrap();
        assert_eq!(first["path"], "v1/station/getStations");
        assert_eq!(first["request"], stations_req(0));
        assert_eq!(first["status"], 200);
        assert_eq!(first["response"]["totalStations"], 3);
        for file in &files {
            let text = fs::read_to_string(dir.join(file)).unwrap();
            assert!(!text.contains("tok-secret-value") && !text.contains("0123456789abcdef"));
        }
    }

    #[test]
    fn feedback_paging_stops_when_a_page_adds_nothing_new() {
        let server = MockServer::start();
        on(
            &server,
            "v1/station/getStations",
            stations_req(0),
            json!({"totalStations": 1, "stations": [{"stationId": "11"}]}),
        );
        on(
            &server,
            "v1/station/getStationDetails",
            details_req("11"),
            json!({}),
        );
        // A server that ignores startIndex: every page is the same two records.
        let up = server.mock(|when, then| {
            when.path("/api/v1/station/getStationFeedback")
                .body_includes("\"positive\":true");
            then.status(200)
                .json_body(json!({"total": 50, "feedback": feedback(&["f1", "f2"])}));
        });
        on(
            &server,
            "v1/station/getStationFeedback",
            feedback_req("11", false, 0),
            json!({"feedback": []}),
        );
        empty_premium(&server);
        let fx = fixture();
        let manifest = run(&server, &fx).0.unwrap();
        up.assert_calls(2);
        assert_eq!(manifest.steps.feedback_up.count, 2);
        assert!(manifest.complete);
    }

    #[test]
    fn progress_that_cannot_be_written_does_not_stop_the_run() {
        let server = MockServer::start();
        no_stations(&server);
        empty_premium(&server);
        let fx = fixture();
        let mut client = Client::new(&server.base_url(), &session(), FAST);
        let captured = export(
            &fx.paths,
            &mut client,
            &mut crate::pandora::test_support::ClosedPipe,
        )
        .unwrap();
        assert!(captured.manifest.complete, "{:?}", captured.manifest);
        assert_eq!(manifest_on_disk(&fx), captured.manifest);
    }

    #[test]
    fn a_rejected_premium_request_fails_its_step_and_the_run_goes_on() {
        let server = MockServer::start();
        no_stations(&server);
        let playlists = server.mock(|when, then| {
            when.path("/api/v6/collections/getSortedPlaylists");
            then.status(502)
                .json_body(json!({"errorCode": 0, "message": "bad request"}));
        });
        on(
            &server,
            "v6/collections/getItems",
            items_req(None),
            json!({"errorCode": 99022, "errorString": "SOMETHING"}),
        );
        let bookmarks = server.mock(|when, then| {
            when.path("/api/v1/bookmark/getBookmarks");
            then.status(200).json_body(json!({"tracks": []}));
        });
        let fx = fixture();
        let (result, out) = run(&server, &fx);
        let manifest = result.unwrap();
        playlists.assert_calls(1);
        bookmarks.assert_calls(2);
        assert!(!manifest.complete);
        assert_eq!(
            manifest.steps.failed(),
            [Step::Playlists, Step::PlaylistTracks, Step::Collection]
        );
        assert_eq!(
            manifest.steps.playlists.error.as_deref(),
            Some(
                "v6/collections/getSortedPlaylists: HTTP 502, errorCode 0, message \"bad request\""
            )
        );
        assert_eq!(
            manifest.steps.playlist_tracks.error.as_deref(),
            Some("incomplete: the `playlists` step it works from failed")
        );
        assert!(
            manifest
                .steps
                .collection
                .error
                .as_deref()
                .unwrap()
                .contains("errorCode 99022"),
            "{manifest:?}"
        );
        assert_eq!(manifest.steps.bookmarks.status, StepStatus::Ok);
        assert_eq!(manifest_on_disk(&fx), manifest);
        assert!(out.contains("playlists        FAILED  0  v6/"), "{out}");
    }

    #[test]
    fn plain_server_errors_are_retried_before_the_step_fails() {
        let server = MockServer::start();
        no_stations(&server);
        on(
            &server,
            "v6/collections/getSortedPlaylists",
            playlists_req(0),
            json!({"items": []}),
        );
        on(
            &server,
            "v6/collections/getItems",
            items_req(None),
            json!({"items": []}),
        );
        let busy = server.mock(|when, then| {
            when.path("/api/v1/bookmark/getBookmarks");
            then.status(503).body("busy");
        });
        let fx = fixture();
        let manifest = run(&server, &fx).0.unwrap();
        busy.assert_calls(10);
        assert_eq!(manifest.steps.failed(), [Step::Bookmarks]);
        assert_eq!(
            manifest.steps.bookmarks.error.as_deref(),
            Some(
                "v1/bookmark/getBookmarks: HTTP 503, response is not JSON after 5 tries \
                 (and 1 more failed requests)"
            )
        );
        // Every attempt is kept raw.
        assert_eq!(run_files(&fx).1.len(), 1 + 1 + 1 + 10 + 1);
    }

    #[test]
    fn a_rejected_session_stops_the_run_and_still_writes_the_manifest() {
        let server = MockServer::start();
        on(
            &server,
            "v1/station/getStations",
            stations_req(0),
            json!({"totalStations": 2, "stations": [{"stationId": "11"}, {"stationId": "12"}]}),
        );
        on(
            &server,
            "v1/station/getStationDetails",
            details_req("11"),
            json!({}),
        );
        server.mock(|when, then| {
            when.path("/api/v1/station/getStationDetails")
                .json_body(details_req("12"));
            then.status(200)
                .json_body(json!({"errorCode": 1001, "errorString": "INVALID_REQUEST"}));
        });
        let later = server.mock(|when, then| {
            when.path_excludes("getStation");
            then.status(200).json_body(json!({}));
        });
        let fx = fixture();
        let (result, _) = run(&server, &fx);
        let err = result.unwrap_err();
        let stopped = err.downcast_ref::<ExportStopped>().expect("ExportStopped");
        assert!(
            stopped.source.downcast_ref::<SessionRejected>().is_some(),
            "{err:#}"
        );
        assert_eq!(crate::error::kind(&err), crate::error::Kind::Auth);
        assert!(
            format!("{err:#}").starts_with("Pandora rejected the session"),
            "{err:#}"
        );
        later.assert_calls(0);
        let manifest = manifest_on_disk(&fx);
        assert!(!manifest.complete);
        assert_eq!(manifest.steps.stations.status, StepStatus::Ok);
        assert_eq!(manifest.steps.failed(), &Step::ALL[1..]);
        assert!(
            manifest
                .steps
                .station_details
                .error
                .as_deref()
                .unwrap()
                .contains("re-run `sss pandora auth`"),
            "{manifest:?}"
        );
        assert_eq!(manifest.requests, 3);
        // The refusal itself is stored raw.
        assert_eq!(
            run_files(&fx).1[2],
            "0003-v1-station-getStationDetails.json"
        );
    }

    #[test]
    fn an_unexpected_response_shape_fails_the_step_instead_of_counting_zero() {
        let server = MockServer::start();
        on(
            &server,
            "v1/station/getStations",
            stations_req(0),
            json!({"something": []}),
        );
        empty_premium(&server);
        let fx = fixture();
        let manifest = run(&server, &fx).0.unwrap();
        assert_eq!(
            manifest.steps.failed(),
            [
                Step::Stations,
                Step::StationDetails,
                Step::FeedbackUp,
                Step::FeedbackDown
            ]
        );
        assert_eq!(
            manifest.steps.stations.error.as_deref(),
            Some("v1/station/getStations: no `stations` array")
        );
    }

    #[test]
    fn playlist_tracks_without_a_version_cannot_page() {
        let server = MockServer::start();
        no_stations(&server);
        on(
            &server,
            "v6/collections/getSortedPlaylists",
            playlists_req(0),
            json!({"items": [{"pandoraId": "PL:1"}]}),
        );
        on(
            &server,
            "v7/playlists/getTracks",
            tracks_req("PL:1", 0, None),
            json!({"totalTracks": 2, "tracks": [{"pandoraId": "TR:1"}]}),
        );
        on(
            &server,
            "v6/collections/getItems",
            items_req(None),
            json!({"items": []}),
        );
        for kind in ["track", "album"] {
            on(
                &server,
                "v1/bookmark/getBookmarks",
                json!({"type": kind}),
                json!({}),
            );
        }
        let fx = fixture();
        let manifest = run(&server, &fx).0.unwrap();
        assert_eq!(manifest.steps.failed(), [Step::PlaylistTracks]);
        assert_eq!(manifest.steps.playlist_tracks.count, 1);
        assert_eq!(
            manifest.steps.playlist_tracks.error.as_deref(),
            Some("v7/playlists/getTracks: PL:1: no `version` to page with")
        );
    }

    #[test]
    fn paging_rule() {
        assert!(more_pages(0, 2, 2, Some(3)));
        assert!(!more_pages(0, 3, 3, Some(3)));
        assert!(!more_pages(0, 0, 0, Some(3)));
        assert!(!more_pages(2, 2, 0, Some(10)));
        assert!(more_pages(0, PAGE, PAGE, None));
        assert!(!more_pages(0, PAGE - 1, PAGE - 1, None));
    }

    #[test]
    fn step_names_and_indices_match_the_manifest() {
        let keys: Vec<String> = match serde_json::to_value(Steps::default()).unwrap() {
            Value::Object(map) => map.keys().cloned().collect(),
            other => panic!("{other}"),
        };
        let mut names: Vec<&str> = Step::ALL.iter().map(|s| s.name()).collect();
        names.sort_unstable();
        assert_eq!(keys, names);
        for (i, step) in Step::ALL.into_iter().enumerate() {
            assert_eq!(step as usize, i, "{step:?}");
        }
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("v1/station/getStations"), "v1-station-getStations");
    }

    #[test]
    fn a_run_without_a_manifest_reads_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(repo::read_manifest::<Manifest>(tmp.path()).unwrap(), None);
    }

    #[test]
    fn an_unreachable_pandora_stops_the_run_naming_it() {
        let fx = fixture();
        let mut client = Client::new("http://127.0.0.1:1", &session(), FAST);
        let err = export(&fx.paths, &mut client, &mut Vec::new()).unwrap_err();
        assert_eq!(crate::error::kind(&err), crate::error::Kind::Unavailable);
        let stopped = err.downcast_ref::<ExportStopped>().expect("ExportStopped");
        let (dir, _) = run_files(&fx);
        assert_eq!(stopped.run_dir, dir);
        assert_eq!(dir.file_name().unwrap().to_str(), Some(&*stopped.run_id));
        let manifest: Manifest = repo::read_manifest(&stopped.run_dir).unwrap().unwrap();
        assert!(!manifest.complete);
        assert_eq!(manifest.steps.failed(), Step::ALL);
        assert!(
            format!("{err:#}").contains("no response after 5 tries"),
            "{err:#}"
        );
    }

    #[test]
    fn a_failure_before_the_run_directory_exists_is_not_a_stopped_run() {
        let fx = fixture();
        fs::create_dir_all(fx.paths.pandora_dir()).unwrap();
        fs::write(fx.paths.raw_dir(), "a file where raw/ belongs").unwrap();
        let mut client = Client::new("http://127.0.0.1:1", &session(), FAST);
        let err = export(&fx.paths, &mut client, &mut Vec::new()).unwrap_err();
        assert!(err.downcast_ref::<ExportStopped>().is_none(), "{err:#}");
        assert_eq!(crate::error::kind(&err), crate::error::Kind::Other);
    }
}
