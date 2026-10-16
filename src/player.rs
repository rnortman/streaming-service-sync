//! The playback commands: `devices`, `player`, `play`, `pause`, `next`,
//! `previous`, `queue`, `shuffle`, `repeat`, `volume` and `seek`, against
//! the profile's Spotify account. Each resolves its arguments locally
//! first, then signs in, checks the token's scopes, reads the open devices
//! and chooses one by [`choose_device`], sends its request(s) and reports
//! the player's state as Spotify then gives it.

use std::borrow::BorrowMut;
use std::collections::BTreeMap;
use std::io::{self, Write};

use anyhow::{Context as _, Result};
use serde::Serialize;

use crate::error::{self, Kind};
use crate::fail;
use crate::matching::model::{self as matches, MatchEntry};
use crate::pandora::model::{self, Station};
use crate::push::{self, Ledger, PlaylistEntry};
use crate::repo::ProfilePaths;
use crate::secrets;
use crate::spotify::auth::{PLAYBACK_MODIFY_SCOPE, PLAYBACK_READ_SCOPE};
use crate::spotify::client::Spotify;
use crate::spotify::ids::{self, ContextKind, IdKind, Target};
use crate::spotify::item::Offered;
use crate::spotify::playback::{
    Device, NO_ACTIVE_DEVICE, PlayBody, PlayerRefused, PlayerState, Repeat, SEVERAL_DEVICES,
    device_names,
};

/// The scopes of a command that only reads the player.
const READ_SCOPES: &[&str] = &[PLAYBACK_READ_SCOPE];
/// The scopes of a command that changes playback.
const MODIFY_SCOPES: &[&str] = &[PLAYBACK_READ_SCOPE, PLAYBACK_MODIFY_SCOPE];

/// Station names listed at most when `--station` names none.
const STATIONS_LISTED: usize = 20;

/// The `hint` of a play the read-back says is not playing.
pub const AUTOPLAY_HINT: &str = "Spotify loaded the track but is not playing. In a browser, \
                                 click play once, or allow autoplay for the Spotify tab: site \
                                 icon → Permissions → Autoplay → Allow Audio and Video.";

/// The `hint` of a command Spotify accepted whose state read afterwards
/// failed.
pub const UNREAD_HINT: &str = "Spotify accepted the command, but the player could not be read \
                               afterwards; run `sss player` to see its state.";

/// What `sss play` plays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaySource {
    /// Nothing named: resume.
    Resume,
    /// These track URIs, in order.
    Tracks(Vec<String>),
    /// A playlist, album or artist.
    Context { kind: ContextKind, id: String },
}

impl PlaySource {
    fn body(&self, offset: Option<u32>) -> PlayBody {
        match self {
            PlaySource::Resume => PlayBody::Resume,
            PlaySource::Tracks(uris) => PlayBody::Uris(uris.clone()),
            PlaySource::Context { kind, id } => PlayBody::Context {
                uri: kind.uri(id),
                offset,
            },
        }
    }
}

/// What `sss play` plays, from at most one of its `targets`, `--station`
/// and `--item`, resolved against the profile's files without contacting
/// Spotify.
pub fn resolve_source(
    paths: &ProfilePaths,
    targets: &[String],
    station: Option<&str>,
    item: Option<&str>,
) -> Result<PlaySource> {
    let named = [!targets.is_empty(), station.is_some(), item.is_some()];
    if named.iter().filter(|n| **n).count() > 1 {
        fail!(
            Kind::Input,
            "play takes targets, --station or --item, only one of them"
        );
    }
    if let Some(name) = station {
        let stations = model::read_stations(paths)?;
        let ledger = Ledger::load(&paths.spotify_state())?;
        let id = station_playlist(&stations, &ledger, name)?;
        return Ok(PlaySource::Context {
            kind: ContextKind::Playlist,
            id,
        });
    }
    if let Some(item) = item {
        let entries = matches::read_latest(&paths.track_matches())?;
        return Ok(PlaySource::Tracks(item_uris(&entries, item)?));
    }
    targets_source(targets)
}

/// The source `targets` name: any number of tracks, or one context; none
/// is a resume.
pub fn targets_source(targets: &[String]) -> Result<PlaySource> {
    let parsed = targets
        .iter()
        .map(|t| ids::parse_target(t))
        .collect::<Result<Vec<_>>>()?;
    match parsed.as_slice() {
        [] => Ok(PlaySource::Resume),
        [Target::Context { kind, id }] => Ok(PlaySource::Context {
            kind: *kind,
            id: id.clone(),
        }),
        all if all.iter().all(|t| matches!(t, Target::Track(_))) => {
            Ok(PlaySource::Tracks(all.iter().map(Target::uri).collect()))
        }
        _ => fail!(
            Kind::Input,
            "play one playlist, album or artist at a time, or any number of tracks"
        ),
    }
}

/// Lowercased and trimmed, for comparing names.
fn folded(name: &str) -> String {
    name.trim().to_lowercase()
}

/// The Spotify playlist id of the station named `name` (case-insensitive,
/// trimmed): the first station of that name, in export order, with a
/// created playlist in the ledger.
pub fn station_playlist(stations: &[Station], ledger: &Ledger, name: &str) -> Result<String> {
    let created = |station: &Station| match ledger.playlists.get(&push::station_key(station)) {
        Some(PlaylistEntry::Created { spotify_id, .. }) => Some(spotify_id.clone()),
        _ => None,
    };
    let wanted = folded(name);
    let named: Vec<&Station> = stations
        .iter()
        .filter(|s| s.name.as_deref().is_some_and(|n| folded(n) == wanted))
        .collect();
    if let Some(id) = named.iter().find_map(|s| created(s)) {
        return Ok(id);
    }
    if !named.is_empty() {
        fail!(
            Kind::Input,
            "station {} has no Spotify playlist yet; run `sss push --only stations`",
            name.trim()
        );
    }
    let pushed: Vec<&str> = stations
        .iter()
        .filter(|s| created(s).is_some())
        .filter_map(|s| s.name.as_deref())
        .collect();
    if pushed.is_empty() {
        fail!(
            Kind::Input,
            "no station named {} in the export, and no station has a Spotify playlist yet; run \
             `sss push --only stations`",
            name.trim()
        );
    }
    let mut listed = pushed
        .iter()
        .take(STATIONS_LISTED)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if pushed.len() > STATIONS_LISTED {
        listed.push_str(", …");
    }
    fail!(
        Kind::Input,
        "no station named {} in the export (stations with a Spotify playlist: {listed})",
        name.trim()
    )
}

/// The stored candidates of track `item`, in stored order.
pub fn item_uris(entries: &BTreeMap<String, MatchEntry>, item: &str) -> Result<Vec<String>> {
    let item = item.trim();
    if IdKind::of_pandora_id(item) != Some(IdKind::Track) {
        fail!(
            Kind::Input,
            "{item:?} is not a Pandora track id (TR:…); --item plays a track's stored candidates"
        );
    }
    let Some(entry) = entries.get(item) else {
        fail!(
            Kind::Input,
            "{item} has no match entry; run `sss match` first"
        );
    };
    if entry.candidates.is_empty() {
        fail!(Kind::Input, "{item} has no stored candidates");
    }
    Ok(entry.candidates.iter().map(|c| c.uri.clone()).collect())
}

/// The 0-based track a shuffled start begins at: `random` modulo `count`;
/// `None` for an empty context.
pub fn shuffle_offset(count: u32, random: u32) -> Option<u32> {
    (count > 0).then(|| random % count)
}

fn random_u32() -> Result<u32> {
    let mut bytes = [0u8; 4];
    secrets::fill_random(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

/// The device a command for `action` goes to: the one `wanted` names (its
/// id, else its name, case-insensitive and trimmed); else `None` when one
/// is active, for Spotify sends a command to the active player; else the
/// only unrestricted one. No unrestricted device, or several, is a
/// [`PlayerRefused`] listing `devices`.
pub fn choose_device(
    devices: &[Device],
    wanted: Option<&str>,
    action: &str,
) -> Result<Option<String>> {
    if let Some(wanted) = wanted {
        let wanted = wanted.trim();
        if let Some(d) = devices.iter().find(|d| d.id == wanted) {
            return Ok(Some(d.id.clone()));
        }
        let named: Vec<&Device> = devices
            .iter()
            .filter(|d| folded(&d.name) == folded(wanted))
            .collect();
        return match named.as_slice() {
            [one] => Ok(Some(one.id.clone())),
            [] if devices.is_empty() => fail!(
                Kind::Input,
                "no Spotify player named {wanted} (none is open)"
            ),
            [] => fail!(
                Kind::Input,
                "no Spotify player named {wanted} (open: {})",
                device_names(devices)
            ),
            several => fail!(
                Kind::Input,
                "several Spotify players are named {wanted} (ids: {}); pass --device ID",
                several
                    .iter()
                    .map(|d| d.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
    }
    if devices.iter().any(|d| d.is_active) {
        return Ok(None);
    }
    let free: Vec<&Device> = devices.iter().filter(|d| !d.is_restricted).collect();
    let reason = match free.as_slice() {
        [one] => return Ok(Some(one.id.clone())),
        [] => NO_ACTIVE_DEVICE,
        _ => SEVERAL_DEVICES,
    };
    Err(anyhow::Error::new(PlayerRefused {
        what: action.to_owned(),
        status: 0,
        reason: reason.to_owned(),
        message: String::new(),
        devices: devices.to_vec(),
    }))
}

/// The device a command went to: the chosen one, else the active one.
fn used_device(devices: &[Device], chosen: Option<&str>) -> Option<Device> {
    match chosen {
        Some(id) => devices.iter().find(|d| d.id == id),
        None => devices.iter().find(|d| d.is_active),
    }
    .cloned()
}

/// Signs in with `connect`, checks the playback scopes, reads the devices
/// and chooses one for `action`: the client, the chosen id and the device
/// it names.
fn prepare<S: BorrowMut<Spotify>>(
    profile: &str,
    action: &str,
    wanted: Option<&str>,
    connect: impl FnOnce() -> Result<S>,
) -> Result<(S, Option<String>, Option<Device>)> {
    let mut client = connect()?;
    let c = client.borrow_mut();
    c.require_scopes(profile, MODIFY_SCOPES)?;
    let devices = c.devices()?;
    let chosen = choose_device(&devices, wanted, action)?;
    let device = used_device(&devices, chosen.as_deref());
    Ok((client, chosen, device))
}

/// Runs `read` after a command Spotify accepted. The command stands
/// whatever the read gives: a failure is a warning, `T::default()`, and
/// `unread` set.
fn read_after<T: Default>(
    c: &mut Spotify,
    unread: &mut bool,
    read: impl FnOnce(&mut Spotify) -> Result<T>,
) -> T {
    match read(c) {
        Ok(value) => value,
        Err(e) => {
            c.warning(&format!(
                "warning: reading the player after the command: {e:#}"
            ));
            *unread = true;
            T::default()
        }
    }
}

/// Where a context play started, for the text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Started {
    uri: String,
    kind: ContextKind,
    offset: Option<u32>,
}

/// How a [`PlayerReport`] reads as text.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Text {
    /// The state line under the action's name.
    State,
    /// What `sss play` started, and where when it was a context.
    Play(Option<Started>),
}

/// What a command that changes playback reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlayerReport {
    pub profile: String,
    /// The command: `play`, `pause`, …
    pub action: &'static str,
    /// The device the command went to: the chosen one, or the active one.
    pub device: Option<Device>,
    /// The player's state read after the command; `null` when none is
    /// active or the read failed.
    pub state: Option<PlayerState>,
    pub hint: Option<String>,
    #[serde(skip)]
    text: Text,
    /// The state read after the command failed.
    #[serde(skip)]
    unread: bool,
}

/// `sss play`: sets shuffle when `shuffle` says so, then plays `source` on
/// the chosen device. With shuffle on and a playlist or album, playback
/// starts at a random track of it.
pub fn play<S: BorrowMut<Spotify>>(
    profile: &str,
    source: &PlaySource,
    shuffle: Option<bool>,
    wanted: Option<&str>,
    connect: impl FnOnce() -> Result<S>,
) -> Result<PlayerReport> {
    let (mut client, chosen, device) = prepare(profile, "play", wanted, connect)?;
    let c = client.borrow_mut();
    let chosen = chosen.as_deref();
    let mut hints = Vec::new();
    let mut offset = None;
    if let (Some(true), PlaySource::Context { kind, id }) = (shuffle, source) {
        let count = match kind {
            ContextKind::Playlist => Some(c.playlist_track_count(id)),
            ContextKind::Album => Some(c.album_track_count(id)),
            ContextKind::Artist => None,
        };
        match count {
            None => {}
            Some(Ok(count)) => offset = shuffle_offset(count, random_u32()?),
            // A refusal the play itself need not share: playback still
            // starts, at the context's first track.
            Some(Err(e)) if matches!(error::kind(&e), Kind::Config | Kind::Other) => {
                c.warning(&format!(
                    "warning: reading the {kind}'s length for a shuffled start: {e:#}"
                ));
                hints.push(format!(
                    "Spotify would not give the {kind}'s length; shuffle is on but playback \
                     starts at its first track"
                ));
            }
            Some(Err(e)) => return Err(e),
        }
    }
    if let Some(on) = shuffle {
        c.set_shuffle(chosen, on)?;
    }
    c.play(chosen, &source.body(offset))?;
    c.settle();
    let mut unread = false;
    let state = read_after(c, &mut unread, Spotify::player_state);
    if unread {
        hints.push(UNREAD_HINT.to_owned());
    }
    if state.as_ref().is_some_and(|s| !s.is_playing) {
        hints.push(AUTOPLAY_HINT.to_owned());
    }
    let started = match source {
        PlaySource::Context { kind, id } => Some(Started {
            uri: kind.uri(id),
            kind: *kind,
            offset,
        }),
        _ => None,
    };
    Ok(PlayerReport {
        profile: profile.to_owned(),
        action: "play",
        device,
        state,
        hint: (!hints.is_empty()).then(|| hints.join(" ")),
        text: Text::Play(started),
        unread,
    })
}

/// A command that changes playback with one request and no target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Pause,
    Next,
    Previous,
    Shuffle(bool),
    Repeat(Repeat),
    /// 0–100.
    Volume(u8),
    /// Whole seconds into the playing item.
    Seek(u64),
}

impl Control {
    /// The command's name.
    pub fn action(self) -> &'static str {
        match self {
            Control::Pause => "pause",
            Control::Next => "next",
            Control::Previous => "previous",
            Control::Shuffle(_) => "shuffle",
            Control::Repeat(_) => "repeat",
            Control::Volume(_) => "volume",
            Control::Seek(_) => "seek",
        }
    }

    fn send(self, c: &mut Spotify, device: Option<&str>) -> Result<()> {
        match self {
            Control::Pause => c.pause(device),
            Control::Next => c.next(device),
            Control::Previous => c.previous(device),
            Control::Shuffle(on) => c.set_shuffle(device, on),
            Control::Repeat(mode) => c.set_repeat(device, mode),
            Control::Volume(percent) => c.set_volume(device, percent),
            Control::Seek(seconds) => c.seek(device, seconds.saturating_mul(1000)),
        }
    }
}

/// `sss pause`, `next`, `previous`, `shuffle`, `repeat`, `volume`, `seek`:
/// the request on the chosen device, then the player's state.
pub fn control<S: BorrowMut<Spotify>>(
    profile: &str,
    control: Control,
    wanted: Option<&str>,
    connect: impl FnOnce() -> Result<S>,
) -> Result<PlayerReport> {
    let action = control.action();
    let (mut client, chosen, device) = prepare(profile, action, wanted, connect)?;
    let c = client.borrow_mut();
    control.send(c, chosen.as_deref())?;
    c.settle();
    let mut unread = false;
    let state = read_after(c, &mut unread, Spotify::player_state);
    Ok(PlayerReport {
        profile: profile.to_owned(),
        action,
        device,
        state,
        hint: unread.then(|| UNREAD_HINT.to_owned()),
        text: Text::State,
        unread,
    })
}

/// What `sss queue` reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueueReport {
    pub profile: String,
    pub device: Option<Device>,
    /// The URIs added, in order.
    pub queued: Vec<String>,
    /// The queue after them, as Spotify gives it.
    pub queue: Vec<Offered>,
    pub state: Option<PlayerState>,
    /// Set when the queue or the state could not be read afterwards.
    pub hint: Option<String>,
}

/// `sss queue`: every target must be a track; each is added in order. A
/// failure after some were added names them in its message, since a
/// queued item cannot be taken back.
pub fn queue<S: BorrowMut<Spotify>>(
    profile: &str,
    targets: &[String],
    wanted: Option<&str>,
    connect: impl FnOnce() -> Result<S>,
) -> Result<QueueReport> {
    let mut uris = Vec::with_capacity(targets.len());
    for text in targets {
        match ids::parse_target(text)? {
            track @ Target::Track(_) => uris.push(track.uri()),
            Target::Context { .. } => fail!(
                Kind::Input,
                "queue takes tracks; to play a playlist, album or artist use `sss play` ({:?})",
                text.trim()
            ),
        }
    }
    let (mut client, chosen, device) = prepare(profile, "queue", wanted, connect)?;
    let c = client.borrow_mut();
    for (done, uri) in uris.iter().enumerate() {
        if let Err(e) = c.add_to_queue(chosen.as_deref(), uri) {
            if done == 0 {
                return Err(e);
            }
            return Err(e.context(format!(
                "queued {done} of {} ({}), then",
                uris.len(),
                uris[..done].join(", ")
            )));
        }
    }
    c.settle();
    let mut unread = false;
    let queue = read_after(c, &mut unread, |c| c.queue_state().map(|q| q.queue));
    let state = read_after(c, &mut unread, Spotify::player_state);
    Ok(QueueReport {
        profile: profile.to_owned(),
        device,
        queued: uris,
        queue,
        state,
        hint: unread.then(|| UNREAD_HINT.to_owned()),
    })
}

/// What `sss devices` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DevicesReport {
    pub profile: String,
    pub devices: Vec<Device>,
}

/// `sss devices`: the open players; none is a success.
pub fn devices<S: BorrowMut<Spotify>>(
    profile: &str,
    connect: impl FnOnce() -> Result<S>,
) -> Result<DevicesReport> {
    let mut client = connect()?;
    let c = client.borrow_mut();
    c.require_scopes(profile, READ_SCOPES)?;
    Ok(DevicesReport {
        profile: profile.to_owned(),
        devices: c.devices()?,
    })
}

/// What `sss player` reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlayerStateReport {
    pub profile: String,
    /// `null` when no player is active.
    pub state: Option<PlayerState>,
    pub queue: Vec<Offered>,
}

/// `sss player`: the player's state and, when one is active, its queue.
pub fn player<S: BorrowMut<Spotify>>(
    profile: &str,
    connect: impl FnOnce() -> Result<S>,
) -> Result<PlayerStateReport> {
    let mut client = connect()?;
    let c = client.borrow_mut();
    c.require_scopes(profile, READ_SCOPES)?;
    let state = c.player_state()?;
    let queue = match state {
        Some(_) => c.queue_state().context("reading the queue")?.queue,
        None => Vec::new(),
    };
    Ok(PlayerStateReport {
        profile: profile.to_owned(),
        state,
        queue,
    })
}

/// `m:ss`.
pub(crate) fn clock(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// `"Song" by Artist, Other`, or `"Song"` when it names no artist.
fn describe(item: &Offered) -> String {
    if item.artists.is_empty() {
        format!("{:?}", item.name)
    } else {
        format!("{:?} by {}", item.name, item.artists.join(", "))
    }
}

/// ` on <device>`: the state's device, else `fallback`; empty when neither
/// names one.
fn on_device(state: &PlayerState, fallback: Option<&Device>) -> String {
    state
        .device
        .as_ref()
        .or(fallback)
        .map(|d| format!(" on {}", d.name))
        .unwrap_or_default()
}

/// `<action>: "Song" by Artist on Firefox (playing, 1:23/4:05, shuffle on,
/// repeat off)`, or `<action>: no active player`.
fn state_line(action: &str, state: Option<&PlayerState>, fallback: Option<&Device>) -> String {
    let Some(state) = state else {
        return format!("{action}: no active player");
    };
    let what = state
        .item
        .as_ref()
        .map_or_else(|| "nothing".to_owned(), describe);
    let mut details = vec![
        if state.is_playing {
            "playing"
        } else {
            "paused"
        }
        .to_owned(),
    ];
    match (
        state.progress_ms,
        state.item.as_ref().and_then(|i| i.duration_ms),
    ) {
        (Some(at), Some(of)) => details.push(format!("{}/{}", clock(at), clock(of))),
        (Some(at), None) => details.push(clock(at)),
        _ => {}
    }
    details.push(format!(
        "shuffle {}",
        if state.shuffle { "on" } else { "off" }
    ));
    details.push(format!("repeat {}", state.repeat.as_str()));
    format!(
        "{action}: {what}{} ({})",
        on_device(state, fallback),
        details.join(", ")
    )
}

impl PlayerReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        if self.unread {
            let hint = self.hint.as_deref().unwrap_or(UNREAD_HINT);
            return writeln!(out, "{}: {hint}", self.action);
        }
        let line = match &self.text {
            Text::Play(started) => self.play_line(started.as_ref()),
            Text::State => state_line(self.action, self.state.as_ref(), self.device.as_ref()),
        };
        writeln!(out, "{line}")
    }

    fn play_line(&self, started: Option<&Started>) -> String {
        let Some(state) = &self.state else {
            return match &self.hint {
                Some(hint) => format!("play: no active player; {hint}"),
                None => "play: no active player".to_owned(),
            };
        };
        let what = match started {
            Some(Started { uri, kind, offset }) => match offset {
                Some(p) => format!("{kind} {uri} from track {}", u64::from(*p) + 1),
                None => format!("{kind} {uri}"),
            },
            None => state
                .item
                .as_ref()
                .map_or_else(|| "nothing".to_owned(), describe),
        };
        let on = on_device(state, self.device.as_ref());
        let line = if state.is_playing {
            format!("playing {what}{on}")
        } else {
            format!("loaded {what}{on}, not playing")
        };
        match &self.hint {
            Some(hint) => format!("{line}: {hint}"),
            None => line,
        }
    }
}

impl QueueReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        writeln!(out, "queued {}", self.queued.join(", "))?;
        match &self.hint {
            Some(hint) => writeln!(out, "queue: {hint}")?,
            None => writeln!(
                out,
                "{}",
                state_line("queue", self.state.as_ref(), self.device.as_ref())
            )?,
        }
        write_queue(&self.queue, out)
    }
}

/// The queue, one item per line under a header; nothing when it is empty.
fn write_queue(queue: &[Offered], out: &mut dyn Write) -> io::Result<()> {
    if queue.is_empty() {
        return Ok(());
    }
    writeln!(out, "up next:")?;
    for item in queue {
        writeln!(out, "  {} {}", item.id, describe(item))?;
    }
    Ok(())
}

impl DevicesReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        if self.devices.is_empty() {
            return writeln!(out, "no Spotify player open");
        }
        for d in &self.devices {
            let mut traits = vec![d.kind.clone()];
            if d.is_active {
                traits.push("active".into());
            }
            if d.is_restricted {
                traits.push("not controllable".into());
            }
            if let Some(v) = d.volume_percent {
                traits.push(format!("volume {v}"));
            }
            writeln!(out, "{} ({}) {}", d.name, traits.join(", "), d.id)?;
        }
        Ok(())
    }
}

impl PlayerStateReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        writeln!(out, "{}", state_line("player", self.state.as_ref(), None))?;
        write_queue(&self.queue, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matching::model::{Candidate, Status};
    use crate::spotify::test_support::client;
    use httpmock::prelude::*;
    use serde_json::{Value, json};

    const TRACK: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";
    const OTHER: &str = "spotify:track:1301WleyT98MSxVHPZCA6M";

    fn device(id: &str, name: &str, active: bool, restricted: bool) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            kind: "Computer".into(),
            is_active: active,
            is_restricted: restricted,
            volume_percent: None,
        }
    }

    fn refusal(err: &anyhow::Error) -> &PlayerRefused {
        err.chain()
            .find_map(|e| e.downcast_ref::<PlayerRefused>())
            .unwrap_or_else(|| panic!("not a player refusal: {err:#}"))
    }

    #[test]
    fn the_device_rule() {
        let firefox = device("d1", "Firefox", false, false);
        let kitchen = device("d2", "Kitchen", false, false);
        let sonos = device("d3", "Kitchen Sonos", false, true);
        let active = device("d4", "Phone", true, false);
        let choose = |devices: &[Device], wanted| choose_device(devices, wanted, "play");

        assert_eq!(
            choose(&[firefox.clone(), active.clone()], None).unwrap(),
            None
        );
        assert_eq!(
            choose(&[firefox.clone(), sonos.clone()], None).unwrap(),
            Some("d1".into())
        );
        let err = choose(&[], None).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Player);
        assert_eq!(refusal(&err).reason, "NO_ACTIVE_DEVICE");
        assert!(format!("{err:#}").starts_with("play: no Spotify player is open"));
        let err = choose(std::slice::from_ref(&sonos), None).unwrap_err();
        assert_eq!(refusal(&err).reason, "NO_ACTIVE_DEVICE");
        assert_eq!(refusal(&err).devices, std::slice::from_ref(&sonos));
        assert!(format!("{err:#}").contains("(Kitchen Sonos) cannot be controlled"));
        let err = choose(&[firefox.clone(), kitchen.clone()], None).unwrap_err();
        assert_eq!(refusal(&err).reason, "SEVERAL_DEVICES");
        assert!(format!("{err:#}").contains("(Firefox, Kitchen); pass --device NAME"));

        let open = [firefox.clone(), kitchen.clone(), sonos.clone()];
        assert_eq!(choose(&open, Some("d2")).unwrap(), Some("d2".into()));
        assert_eq!(choose(&open, Some(" firefox ")).unwrap(), Some("d1".into()));
        assert_eq!(
            choose(&open, Some("KITCHEN SONOS")).unwrap(),
            Some("d3".into())
        );
        let err = choose(&open, Some("Den")).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert_eq!(
            format!("{err:#}"),
            "no Spotify player named Den (open: Firefox, Kitchen, Kitchen Sonos)"
        );
        let twins = [firefox.clone(), device("d5", "firefox", false, false)];
        let err = choose(&twins, Some("Firefox")).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert!(format!("{err:#}").contains("(ids: d1, d5)"), "{err:#}");
    }

    fn station(id: &str, name: &str) -> Station {
        Station {
            station_id: id.into(),
            pandora_id: Some(format!("ST:{id}")),
            name: Some(name.into()),
            created: None,
            last_played: None,
            is_thumbprint: false,
            is_shuffle: false,
            genres: Vec::new(),
            positive_feedback_count: None,
            negative_feedback_count: None,
            seeds: Vec::new(),
            feedback: Vec::new(),
        }
    }

    fn created(id: &str) -> PlaylistEntry {
        PlaylistEntry::Created {
            spotify_id: id.into(),
            snapshot_id: None,
            name: None,
            items: Vec::new(),
            at: None,
        }
    }

    #[test]
    fn a_station_resolves_to_its_pushed_playlist() {
        let stations = [
            station("1", "Jazz"),
            station("2", "Jazz"),
            station("3", "Folk"),
            station("4", "Rock"),
        ];
        let mut ledger = Ledger::default();
        ledger.playlists.insert("ST:2".into(), created("p2"));
        ledger.playlists.insert("ST:3".into(), created("p3"));
        ledger.playlists.insert(
            "ST:4".into(),
            PlaylistEntry::Pending {
                name: "Rock".into(),
                at: "2026-10-15T12:00:00Z".into(),
            },
        );
        assert_eq!(
            station_playlist(&stations, &ledger, " jazz ").unwrap(),
            "p2"
        );
        assert_eq!(station_playlist(&stations, &ledger, "FOLK").unwrap(), "p3");
        let err = station_playlist(&stations, &ledger, "Rock").unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert_eq!(
            format!("{err:#}"),
            "station Rock has no Spotify playlist yet; run `sss push --only stations`"
        );
        let err = station_playlist(&stations, &ledger, "Blues").unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert_eq!(
            format!("{err:#}"),
            "no station named Blues in the export (stations with a Spotify playlist: Jazz, Folk)"
        );
        let err = station_playlist(&stations, &Ledger::default(), "Jazz").unwrap_err();
        assert!(format!("{err:#}").contains("has no Spotify playlist yet"));

        let many: Vec<Station> = (0..25)
            .map(|i| station(&i.to_string(), &format!("S{i}")))
            .collect();
        let mut ledger = Ledger::default();
        for i in 0..25 {
            ledger.playlists.insert(format!("ST:{i}"), created("p"));
        }
        let text = format!("{:#}", station_playlist(&many, &ledger, "x").unwrap_err());
        assert!(text.ends_with("S18, S19, …)"), "{text}");
        assert!(!text.contains("S20"), "{text}");
    }

    fn entry(id: &str, uris: &[&str]) -> MatchEntry {
        MatchEntry {
            pandora_id: id.into(),
            status: Status::Ambiguous,
            spotify_uri: None,
            method: None,
            confidence: None,
            candidates: uris
                .iter()
                .map(|u| Candidate {
                    uri: (*u).into(),
                    ..Candidate::default()
                })
                .collect(),
            queries: Vec::new(),
            decided_at: "2026-10-15T12:00:00Z".into(),
            session: None,
            reason: None,
        }
    }

    #[test]
    fn an_item_resolves_to_its_candidates_in_stored_order() {
        let mut entries = BTreeMap::new();
        entries.insert("TR:1".to_owned(), entry("TR:1", &[OTHER, TRACK]));
        entries.insert("TR:2".to_owned(), entry("TR:2", &[]));
        assert_eq!(item_uris(&entries, " TR:1 ").unwrap(), [OTHER, TRACK]);
        for (item, text) in [
            ("TR:2", "TR:2 has no stored candidates"),
            ("TR:3", "TR:3 has no match entry; run `sss match` first"),
            ("AR:1", "\"AR:1\" is not a Pandora track id"),
        ] {
            let err = item_uris(&entries, item).unwrap_err();
            assert_eq!(error::kind(&err), Kind::Input, "{item}");
            assert!(format!("{err:#}").starts_with(text), "{err:#}");
        }
    }

    #[test]
    fn targets_play_tracks_or_one_context() {
        let s = |v: &[&str]| v.iter().map(|t| t.to_string()).collect::<Vec<_>>();
        assert_eq!(targets_source(&[]).unwrap(), PlaySource::Resume);
        assert_eq!(
            targets_source(&s(&[TRACK, "1301WleyT98MSxVHPZCA6M"])).unwrap(),
            PlaySource::Tracks(vec![TRACK.into(), OTHER.into()])
        );
        assert_eq!(
            targets_source(&s(&[
                "https://open.spotify.com/album/4uLU6hMCjMI75M1A2tKUQC"
            ]))
            .unwrap(),
            PlaySource::Context {
                kind: ContextKind::Album,
                id: "4uLU6hMCjMI75M1A2tKUQC".into()
            }
        );
        let playlist = "spotify:playlist:4uLU6hMCjMI75M1A2tKUQC";
        for mixed in [s(&[playlist, TRACK]), s(&[playlist, playlist])] {
            let err = targets_source(&mixed).unwrap_err();
            assert_eq!(error::kind(&err), Kind::Input);
            assert!(format!("{err:#}").starts_with("play one playlist"));
        }
        let err = targets_source(&s(&["spotify:show:4uLU6hMCjMI75M1A2tKUQC"])).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
    }

    #[test]
    fn play_takes_one_source_and_resolves_it_without_spotify() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::repo::Repo::at(tmp.path()).profile("alice").unwrap();
        let targets = [TRACK.to_owned()];
        for (t, station, item) in [
            (&targets[..], Some("Jazz"), None),
            (&targets[..], None, Some("TR:1")),
            (&[][..], Some("Jazz"), Some("TR:1")),
        ] {
            let err = resolve_source(&paths, t, station, item).unwrap_err();
            assert_eq!(error::kind(&err), Kind::Input);
            assert!(format!("{err:#}").contains("only one of them"));
        }
        // No export: the station cannot be looked up.
        let err = resolve_source(&paths, &[], Some("Jazz"), None).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Other);
        assert!(format!("{err:#}").contains("run `sss pandora export`"));
        crate::matching::model::append(&paths.track_matches(), &entry("TR:1", &[TRACK])).unwrap();
        assert_eq!(
            resolve_source(&paths, &[], None, Some("TR:1")).unwrap(),
            PlaySource::Tracks(vec![TRACK.into()])
        );
    }

    #[test]
    fn a_shuffled_start_is_the_random_number_modulo_the_count() {
        assert_eq!(shuffle_offset(0, 7), None);
        assert_eq!(shuffle_offset(40, 37), Some(37));
        assert_eq!(shuffle_offset(40, 77), Some(37));
        assert_eq!(shuffle_offset(1, u32::MAX), Some(0));
    }

    fn wire_device(id: &str, name: &str, active: bool) -> Value {
        json!({"id": id, "name": name, "type": "Computer", "is_active": active,
               "is_restricted": false, "volume_percent": 80})
    }

    fn wire_state(playing: bool) -> Value {
        json!({"device": wire_device("d1", "Firefox", true), "is_playing": playing,
               "progress_ms": 83000, "shuffle_state": false, "repeat_state": "off",
               "item": {"type": "track", "uri": TRACK, "name": "Song",
                        "artists": [{"name": "Band"}], "album": {"name": "LP"},
                        "duration_ms": 245000}})
    }

    /// A mock account with one inactive device, `d1` "Firefox", whose
    /// state reads back as `playing`.
    fn one_inactive_device(server: &MockServer, playing: bool) {
        server.mock(|when, then| {
            when.method(GET).path("/v1/me/player/devices");
            then.status(200)
                .json_body(json!({"devices": [wire_device("d1", "Firefox", false)]}));
        });
        server.mock(|when, then| {
            when.method(GET).path("/v1/me/player");
            then.status(200).json_body(wire_state(playing));
        });
    }

    const BOTH: &[&str] = MODIFY_SCOPES;

    #[test]
    fn a_token_without_the_playback_scopes_makes_no_request() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut c = client(tmp.path(), &server, &[PLAYBACK_READ_SCOPE]);
        let err = control("alice", Control::Pause, None, || Ok(&mut c)).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Auth);
        assert!(format!("{err:#}").contains("without user-modify-playback-state"));
        assert_eq!(c.requests(), 0);
        let mut c = client(tmp.path(), &server, &[]);
        let err = player("alice", || Ok(&mut c)).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Auth);
        assert_eq!(c.requests(), 0);
    }

    #[test]
    fn play_sends_the_tracks_to_the_only_device_and_reports_the_state() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        one_inactive_device(&server, true);
        let play = server.mock(|when, then| {
            when.method(PUT)
                .path("/v1/me/player/play")
                .query_param("device_id", "d1")
                .json_body(json!({"uris": [TRACK, OTHER]}));
            then.status(204);
        });
        let mut c = client(tmp.path(), &server, BOTH);
        let source = PlaySource::Tracks(vec![TRACK.into(), OTHER.into()]);
        let report = super::play("alice", &source, None, None, || Ok(&mut c)).unwrap();
        play.assert();
        assert_eq!(c.settles(), 1);
        assert_eq!(report.device.as_ref().unwrap().name, "Firefox");
        assert_eq!(report.hint, None);
        assert!(report.state.as_ref().unwrap().is_playing);
        assert_eq!(
            text(|o| report.write_text(o)),
            "playing \"Song\" by Band on Firefox\n"
        );
        let doc = serde_json::to_value(&report).unwrap();
        assert_eq!(doc["action"], "play");
        assert_eq!(doc["state"]["item"]["id"], TRACK);
        assert!(doc.get("text").is_none(), "{doc}");
    }

    #[test]
    fn a_play_that_is_not_playing_is_a_success_with_the_autoplay_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        one_inactive_device(&server, false);
        server.mock(|when, then| {
            when.method(PUT)
                .path("/v1/me/player/play")
                .json_body(json!({}));
            then.status(204);
        });
        let mut c = client(tmp.path(), &server, BOTH);
        let report = super::play("alice", &PlaySource::Resume, None, None, || Ok(&mut c)).unwrap();
        assert_eq!(report.hint.as_deref(), Some(AUTOPLAY_HINT));
        assert_eq!(
            text(|o| report.write_text(o)),
            format!("loaded \"Song\" by Band on Firefox, not playing: {AUTOPLAY_HINT}\n")
        );
    }

    thread_local! {
        static WARNINGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    fn record_warning(line: &str) {
        WARNINGS.with(|w| w.borrow_mut().push(line.to_owned()));
    }

    /// The warnings this thread's clients wrote since the last call.
    fn warnings() -> Vec<String> {
        WARNINGS.with(|w| std::mem::take(&mut *w.borrow_mut()))
    }

    /// `play` with `shuffle` of `source` against a count read answering
    /// `count_status` with `count_body`: the play request's body, whether
    /// shuffle was set, and the report or error.
    fn shuffled(
        source: &PlaySource,
        shuffle: Option<bool>,
        count_status: u16,
        count_body: Value,
    ) -> (Option<Value>, usize, usize, Result<PlayerReport>) {
        let (body, counted, set, _, result) =
            shuffled_in_order(source, shuffle, count_status, count_body);
        (body, counted, set, result)
    }

    /// [`shuffled`], plus the order the shuffle and play requests came in.
    fn shuffled_in_order(
        source: &PlaySource,
        shuffle: Option<bool>,
        count_status: u16,
        count_body: Value,
    ) -> (
        Option<Value>,
        usize,
        usize,
        Vec<&'static str>,
        Result<PlayerReport>,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        one_inactive_device(&server, true);
        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let shuffled_at = order.clone();
        let count = server.mock(|when, then| {
            when.method(GET)
                .path_matches(r"^/v1/(playlists/[^/]+/items|albums/[^/]+)$");
            then.status(count_status)
                .header("Retry-After", "3600")
                .json_body(count_body);
        });
        let set = server.mock(|when, then| {
            when.method(PUT)
                .path("/v1/me/player/shuffle")
                .is_true(move |_| {
                    shuffled_at.lock().unwrap().push("shuffle");
                    true
                });
            then.status(200).body("snapshot123");
        });
        let body = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen = body.clone();
        let played_at = order.clone();
        server.mock(|when, then| {
            when.method(PUT)
                .path("/v1/me/player/play")
                .is_true(move |req| {
                    *seen.lock().unwrap() = serde_json::from_slice(req.body_ref()).ok();
                    played_at.lock().unwrap().push("play");
                    true
                });
            then.status(204);
        });
        let mut c = client(tmp.path(), &server, BOTH);
        c.warn = record_warning;
        let result = super::play("alice", source, shuffle, None, || Ok(&mut c));
        let sent = body.lock().unwrap().take();
        let order = order.lock().unwrap().clone();
        (sent, count.calls(), set.calls(), order, result)
    }

    fn context(kind: ContextKind) -> PlaySource {
        PlaySource::Context {
            kind,
            id: "4uLU6hMCjMI75M1A2tKUQC".into(),
        }
    }

    #[test]
    fn a_shuffled_playlist_or_album_starts_at_a_random_track() {
        for (kind, count) in [
            (ContextKind::Playlist, json!({"total": 40})),
            (ContextKind::Album, json!({"total_tracks": 40})),
        ] {
            let (body, counted, set, order, report) =
                shuffled_in_order(&context(kind), Some(true), 200, count);
            assert_eq!(
                order,
                ["shuffle", "play"],
                "shuffle must be on before the play"
            );
            let body = body.unwrap();
            let position = body["offset"]["position"].as_u64().unwrap();
            assert!(position < 40, "{body}");
            assert_eq!((counted, set), (1, 1));
            let report = report.unwrap();
            assert_eq!(report.hint, None);
            let line = text(|o| report.write_text(o));
            assert!(
                line.starts_with(&format!(
                    "playing {kind} spotify:{kind}:4uLU6hMCjMI75M1A2tKUQC from track {} on Firefox",
                    position + 1
                )),
                "{line}"
            );
        }
    }

    #[test]
    fn no_offset_without_shuffle_on_a_playlist_or_album() {
        let tracks = PlaySource::Tracks(vec![TRACK.into()]);
        for (source, shuffle, set_calls) in [
            (context(ContextKind::Artist), Some(true), 1),
            (tracks, Some(true), 1),
            (context(ContextKind::Playlist), Some(false), 1),
            (context(ContextKind::Playlist), None, 0),
        ] {
            let (body, counted, set, report) =
                shuffled(&source, shuffle, 200, json!({"total": 40}));
            report.unwrap();
            assert!(body.unwrap().get("offset").is_none(), "{source:?}");
            assert_eq!((counted, set), (0, set_calls), "{source:?}");
        }
    }

    #[test]
    fn a_refused_count_read_still_plays_from_the_first_track() {
        for (status, kind) in [(403, ContextKind::Playlist), (404, ContextKind::Album)] {
            let refused = json!({"error": {"status": status, "message": "not listable"}});
            let (body, counted, set, report) =
                shuffled(&context(kind), Some(true), status, refused);
            assert_eq!(
                body.unwrap(),
                json!({"context_uri": format!("spotify:{kind}:4uLU6hMCjMI75M1A2tKUQC")})
            );
            assert_eq!((counted, set), (1, 1));
            let report = report.unwrap();
            assert_eq!(
                report.hint.unwrap(),
                format!(
                    "Spotify would not give the {kind}'s length; shuffle is on but playback \
                     starts at its first track"
                )
            );
            let warned = warnings();
            assert!(
                matches!(warned.as_slice(), [w] if w.starts_with(&format!(
                    "warning: reading the {kind}'s length for a shuffled start: "
                )) && w.contains("not listable")),
                "{warned:?}"
            );
        }
    }

    #[test]
    fn a_count_read_stopped_by_quota_changes_nothing() {
        let quota = json!({"error": {"status": 429, "message": "slow down"}});
        let (body, counted, set, report) =
            shuffled(&context(ContextKind::Playlist), Some(true), 429, quota);
        assert_eq!(error::kind(&report.unwrap_err()), Kind::Quota);
        assert_eq!((body, counted, set), (None, 1, 0));
    }

    #[test]
    fn a_control_goes_to_the_chosen_device_and_settles_once() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        one_inactive_device(&server, false);
        let seek = server.mock(|when, then| {
            when.method(PUT)
                .path("/v1/me/player/seek")
                .query_param("position_ms", "30000")
                .query_param("device_id", "d1");
            then.status(204);
        });
        let mut c = client(tmp.path(), &server, BOTH);
        let report = control("alice", Control::Seek(30), Some("firefox"), || Ok(&mut c)).unwrap();
        seek.assert();
        assert_eq!(c.settles(), 1);
        assert_eq!(report.action, "seek");
        assert_eq!(
            text(|o| report.write_text(o)),
            "seek: \"Song\" by Band on Firefox (paused, 1:23/4:05, shuffle off, repeat off)\n"
        );

        server.mock(|when, then| {
            when.method(GET).path("/v1/me/player/queue");
            then.status(200).json_body(json!({"queue": [
                {"type": "track", "uri": OTHER, "name": "Next", "artists": [{"name": "Band"}]}]}));
        });
        let mut c = client(tmp.path(), &server, BOTH);
        assert_eq!(devices("alice", || Ok(&mut c)).unwrap().devices.len(), 1);
        let read = player("alice", || Ok(&mut c)).unwrap();
        assert!(read.state.is_some());
        assert_eq!(read.queue[0].id, OTHER);
        assert_eq!(c.settles(), 0, "reading commands do not settle");
    }

    #[test]
    fn player_reads_the_queue_only_when_a_player_is_active() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut state = server.mock(|when, then| {
            when.method(GET).path("/v1/me/player");
            then.status(204);
        });
        let queue = server.mock(|when, then| {
            when.method(GET).path("/v1/me/player/queue");
            then.status(200)
                .json_body(json!({"currently_playing": null, "queue": [
                {"type": "track", "uri": OTHER, "name": "Next", "artists": [{"name": "Band"}]}]}));
        });
        let mut c = client(tmp.path(), &server, &[PLAYBACK_READ_SCOPE]);
        let report = player("alice", || Ok(&mut c)).unwrap();
        assert_eq!((report.state.as_ref(), report.queue.len()), (None, 0));
        assert_eq!(queue.calls(), 0);
        assert_eq!(text(|o| report.write_text(o)), "player: no active player\n");
        state.delete();
        server.mock(|when, then| {
            when.method(GET).path("/v1/me/player");
            then.status(200).json_body(wire_state(true));
        });
        let report = player("alice", || Ok(&mut c)).unwrap();
        assert_eq!(report.queue[0].id, OTHER);
        assert_eq!(
            text(|o| report.write_text(o)),
            format!(
                "player: \"Song\" by Band on Firefox (playing, 1:23/4:05, shuffle off, repeat off)\n\
                 up next:\n  {OTHER} \"Next\" by Band\n"
            )
        );
    }

    /// `sss queue` of `TRACK, OTHER, TRACK` where every add of `OTHER`
    /// answers `status` with `body`.
    fn queue_stopping(status: u16, body: Value) -> anyhow::Error {
        queue_failing([TRACK, OTHER, TRACK], status, body)
    }

    /// `sss queue` of `targets` where every add of `OTHER` answers
    /// `status` with `body` and every add of `TRACK` succeeds.
    fn queue_failing(targets: [&str; 3], status: u16, body: Value) -> anyhow::Error {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        one_inactive_device(&server, true);
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/me/player/queue")
                .query_param("uri", TRACK);
            then.status(200).body("snapshot123");
        });
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/me/player/queue")
                .query_param("uri", OTHER);
            then.status(status).json_body(body);
        });
        let mut c = client(tmp.path(), &server, BOTH);
        let targets = targets.map(String::from);
        queue("alice", &targets, None, || Ok(&mut c)).unwrap_err()
    }

    #[test]
    fn a_stopped_queue_names_what_landed() {
        let err = queue_stopping(503, json!({}));
        assert_eq!(error::kind(&err), Kind::Unavailable);
        assert!(
            format!("{err:#}").starts_with(&format!("queued 1 of 3 ({TRACK}), then: ")),
            "{err:#}"
        );
        let refused = json!({"error": {"status": 404, "message": "Player command failed: No active device found",
                                       "reason": "NO_ACTIVE_DEVICE"}});
        let err = queue_stopping(404, refused);
        assert_eq!(error::kind(&err), Kind::Player);
        assert_eq!(refusal(&err).reason, "NO_ACTIVE_DEVICE");
        assert!(format!("{err:#}").starts_with("queued 1 of 3"), "{err:#}");
    }

    #[test]
    fn a_queue_stopped_at_its_first_add_gives_the_failure_alone() {
        let err = queue_failing([OTHER, TRACK, TRACK], 503, json!({}));
        assert_eq!(error::kind(&err), Kind::Unavailable);
        let text = format!("{err:#}");
        assert!(text.starts_with("POST me/player/queue: "), "{text}");
        assert!(!text.contains("queued"), "{text}");
    }

    /// A mock account with one inactive device whose every state and
    /// queue read answers 503.
    fn unreadable_after_writes(server: &MockServer) {
        server.mock(|when, then| {
            when.method(GET).path("/v1/me/player/devices");
            then.status(200)
                .json_body(json!({"devices": [wire_device("d1", "Firefox", false)]}));
        });
        server.mock(|when, then| {
            when.method(GET).path_matches(r"^/v1/me/player(/queue)?$");
            then.status(503);
        });
    }

    #[test]
    fn an_accepted_command_whose_state_cannot_be_read_still_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        unreadable_after_writes(&server);
        let next = server.mock(|when, then| {
            when.method(POST).path("/v1/me/player/next");
            then.status(204);
        });
        let mut c = client(tmp.path(), &server, BOTH);
        c.warn = record_warning;
        warnings();
        let report = control("alice", Control::Next, None, || Ok(&mut c)).unwrap();
        next.assert_calls(1);
        assert_eq!(report.state, None);
        assert_eq!(report.hint.as_deref(), Some(UNREAD_HINT));
        assert_eq!(
            text(|o| report.write_text(o)),
            format!("next: {UNREAD_HINT}\n")
        );
        assert!(
            warnings()
                .iter()
                .any(|w| w.starts_with("warning: reading the player after the command: ")),
        );

        server.mock(|when, then| {
            when.method(PUT).path("/v1/me/player/play");
            then.status(204);
        });
        let report = super::play("alice", &PlaySource::Resume, None, None, || Ok(&mut c)).unwrap();
        assert_eq!(report.hint.as_deref(), Some(UNREAD_HINT));
        assert_eq!(
            text(|o| report.write_text(o)),
            format!("play: {UNREAD_HINT}\n")
        );
    }

    #[test]
    fn a_queue_whose_result_cannot_be_read_still_reports_what_was_queued() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        unreadable_after_writes(&server);
        let add = server.mock(|when, then| {
            when.method(POST).path("/v1/me/player/queue");
            then.status(200).body("snapshot123");
        });
        let mut c = client(tmp.path(), &server, BOTH);
        c.warn = record_warning;
        let targets = [TRACK, OTHER].map(String::from);
        let report = queue("alice", &targets, None, || Ok(&mut c)).unwrap();
        warnings();
        add.assert_calls(2);
        assert_eq!(report.queued, [TRACK, OTHER]);
        assert_eq!((report.queue.len(), report.state.as_ref()), (0, None));
        assert_eq!(report.hint.as_deref(), Some(UNREAD_HINT));
        assert_eq!(
            text(|o| report.write_text(o)),
            format!("queued {TRACK}, {OTHER}\nqueue: {UNREAD_HINT}\n")
        );
    }

    #[test]
    fn queue_adds_tracks_in_order_and_refuses_a_context_before_any_request() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        let mut c = client(tmp.path(), &server, BOTH);
        let targets = [TRACK, "spotify:playlist:4uLU6hMCjMI75M1A2tKUQC"].map(String::from);
        let err = queue("alice", &targets, None, || Ok(&mut c)).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert!(format!("{err:#}").starts_with("queue takes tracks"));
        assert_eq!(c.requests(), 0);

        one_inactive_device(&server, true);
        let add = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/me/player/queue")
                .query_param("device_id", "d1");
            then.status(200).body("snapshot123");
        });
        server.mock(|when, then| {
            when.method(GET).path("/v1/me/player/queue");
            then.status(200).json_body(json!({"queue": [
                {"type": "track", "uri": OTHER, "name": "Next", "artists": [{"name": "Band"}]}]}));
        });
        let targets = [TRACK, OTHER].map(String::from);
        let report = queue("alice", &targets, None, || Ok(&mut c)).unwrap();
        assert_eq!(add.calls(), 2);
        assert_eq!(report.queued, [TRACK, OTHER]);
        assert_eq!(c.settles(), 1);
        assert_eq!(
            text(|o| report.write_text(o)),
            format!(
                "queued {TRACK}, {OTHER}\n\
                 queue: \"Song\" by Band on Firefox (playing, 1:23/4:05, shuffle off, repeat off)\n\
                 up next:\n  {OTHER} \"Next\" by Band\n"
            )
        );
    }

    fn text(write: impl FnOnce(&mut dyn Write) -> io::Result<()>) -> String {
        let mut out = Vec::new();
        write(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn text_lines() {
        let report = PlayerReport {
            profile: "alice".into(),
            action: "play",
            device: Some(device("d1", "Firefox", false, false)),
            state: Some(PlayerState {
                device: None,
                is_playing: true,
                item: None,
                progress_ms: None,
                context: None,
                shuffle: true,
                repeat: Repeat::Context,
            }),
            hint: None,
            text: Text::Play(Some(Started {
                uri: "spotify:playlist:p".into(),
                kind: ContextKind::Playlist,
                offset: Some(37),
            })),
            unread: false,
        };
        assert_eq!(
            text(|o| report.write_text(o)),
            "playing playlist spotify:playlist:p from track 38 on Firefox\n"
        );
        let unshuffled = PlayerReport {
            text: Text::Play(Some(Started {
                uri: "spotify:playlist:p".into(),
                kind: ContextKind::Playlist,
                offset: None,
            })),
            ..report.clone()
        };
        assert_eq!(
            text(|o| unshuffled.write_text(o)),
            "playing playlist spotify:playlist:p on Firefox\n"
        );
        let next = PlayerReport {
            action: "next",
            state: None,
            text: Text::State,
            ..report.clone()
        };
        assert_eq!(text(|o| next.write_text(o)), "next: no active player\n");
        let paused = PlayerReport {
            action: "pause",
            text: Text::State,
            ..report.clone()
        };
        assert_eq!(
            text(|o| paused.write_text(o)),
            "pause: nothing on Firefox (playing, shuffle on, repeat context)\n"
        );

        let none = DevicesReport {
            profile: "alice".into(),
            devices: Vec::new(),
        };
        assert_eq!(text(|o| none.write_text(o)), "no Spotify player open\n");
        let mut sonos = device("d3", "Kitchen Sonos", true, true);
        sonos.volume_percent = Some(30);
        let some = DevicesReport {
            profile: "alice".into(),
            devices: vec![device("d1", "Firefox", false, false), sonos],
        };
        assert_eq!(
            text(|o| some.write_text(o)),
            "Firefox (Computer) d1\nKitchen Sonos (Computer, active, not controllable, volume 30) d3\n"
        );
    }
}
