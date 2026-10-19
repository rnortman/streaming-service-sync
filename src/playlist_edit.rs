//! The `sss playlist` writes: `create`, `edit`, `add`, `remove`, `move`,
//! `replace` and `delete`. Each resolves what it is given and loads the push
//! ledger before connecting, reads the playlist, checks what Spotify would
//! refuse, and unless it is a dry run writes.

use std::borrow::BorrowMut;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, BufRead, Write};

use anyhow::{Context as _, Result};
use serde::Serialize;

use crate::error::Kind;
use crate::fail;
use crate::player::folded;
use crate::playlists::{self, PlaylistTarget, playlist_line};
use crate::push::{self, DESCRIPTION_MAX, Ledger, NAME_MAX, PlaylistEntry, RECOVER_PLAYLISTS_URL};
use crate::repo::{self, ProfilePaths};
use crate::search::result_line;
use crate::spotify::account::{Owner, Playlist, User};
use crate::spotify::auth::{PLAYLIST_FOLLOW_SCOPES, PLAYLIST_WRITE_SCOPES};
use crate::spotify::client::{PLAYLIST_CHUNK, Spotify};
use crate::spotify::edit::PlaylistChanges;
use crate::spotify::ids::ContextKind;
use crate::spotify::item::Offered;
use crate::targets::{self, Items, PLAYLIST_ITEMS, Progress};

/// `name` trimmed, refused with kind `input` unless it is 1 to
/// [`NAME_MAX`] characters.
fn playlist_name(name: &str) -> Result<String> {
    let name = name.trim();
    let n = name.chars().count();
    if n == 0 {
        fail!(Kind::Input, "a playlist name cannot be empty");
    }
    if n > NAME_MAX {
        fail!(
            Kind::Input,
            "a playlist name is at most {NAME_MAX} characters; this one is {n}"
        );
    }
    Ok(name.to_owned())
}

/// The description to send for `text`: cut to [`DESCRIPTION_MAX`]
/// characters, or, on a playlist pushed as `pushed_as`, stripped of the
/// `sss:<key>` markers it ends with (a description read back from the
/// playlist), cut to what fits beside one marker and followed by it; with
/// a line on `err` when it was cut.
fn description(text: &str, pushed_as: Option<&str>, err: &mut dyn Write) -> String {
    let text = match pushed_as {
        Some(key) => without_marker(text, key),
        None => text,
    };
    let (n, room) = match pushed_as {
        Some(key) => (text.chars().count(), push::marker_room(key)),
        None => (text.chars().count(), DESCRIPTION_MAX),
    };
    if n > room {
        let _ = match pushed_as {
            Some(key) => writeln!(
                err,
                "warning: the description is {n} characters; {room} fit beside the sss:{key} \
                 marker Spotify's {DESCRIPTION_MAX} must also hold, so it is cut"
            ),
            None => writeln!(
                err,
                "warning: the description is {n} characters; Spotify takes {DESCRIPTION_MAX}, so it is cut"
            ),
        };
    }
    match pushed_as {
        Some(key) => push::with_marker(text, key),
        None => push::cut(text, DESCRIPTION_MAX),
    }
}

/// `text` trimmed, without the `sss:<key>` markers it ends with.
fn without_marker<'a>(text: &'a str, key: &str) -> &'a str {
    let marker = format!("sss:{key}");
    let mut text = text.trim();
    while let Some(rest) = text.strip_suffix(marker.as_str())
        && (rest.is_empty() || rest.ends_with(char::is_whitespace))
    {
        text = rest.trim_end();
    }
    text
}

/// The profile's push ledger and `target` parsed against it (no request).
fn parse(paths: &ProfilePaths, target: &str) -> Result<(Ledger, PlaylistTarget)> {
    let ledger = Ledger::load(&paths.spotify_state())?;
    let parsed = playlists::parse_target(target, &ledger)?;
    Ok((ledger, parsed))
}

/// A playlist command's target as read: the account, the playlist, its
/// id, and the ledger key of the push that made it.
struct Opened {
    user: User,
    id: String,
    playlist: Playlist,
    pushed_as: Option<String>,
}

/// Reads the account, resolves `target` (a name through the listing) and
/// reads the playlist's details.
fn open(c: &mut Spotify, target: &PlaylistTarget, ledger: &Ledger) -> Result<Opened> {
    let user = c.me().context("reading the Spotify account")?;
    let id = playlists::resolve_ids(c, std::slice::from_ref(target))?
        .pop()
        .expect("one id per target");
    let playlist = playlists::details(c, &id)?;
    Ok(Opened {
        pushed_as: playlists::target_key(target, ledger, &id),
        user,
        id,
        playlist,
    })
}

impl Opened {
    fn owned(&self) -> bool {
        self.playlist.owner.id == self.user.id
    }

    /// Kind `input` unless the account may change the playlist's contents:
    /// it owns the playlist or the playlist is collaborative.
    fn check_contents_writable(&self) -> Result<()> {
        if self.owned() || self.playlist.collaborative {
            return Ok(());
        }
        fail!(
            Kind::Input,
            "{:?} is {}'s and not collaborative; its contents cannot be changed from this \
             account (follow or unfollow it with `sss library add|remove`)",
            self.playlist.name,
            self.playlist.owner.name
        )
    }

    /// The playlist's contents, and how many entries they do not show.
    fn contents(&self, c: &mut Spotify) -> Result<(Vec<Offered>, u32)> {
        let items = c.playlist_contents(&self.id).with_context(|| {
            format!(
                "reading the items of {}",
                ContextKind::Playlist.uri(&self.id)
            )
        })?;
        let hidden = playlists::unavailable(&self.playlist, &items);
        Ok((items, hidden))
    }
}

/// Kind `input` when the contents hide entries, so that a position the
/// person read in `sss playlist` is not Spotify's; `advice` says what to
/// do instead.
fn check_positions(playlist: &Playlist, hidden: u32, advice: &str) -> Result<()> {
    if hidden == 0 {
        return Ok(());
    }
    let items = if hidden == 1 { "item" } else { "items" };
    fail!(
        Kind::Input,
        "{:?} holds {hidden} {items} sss cannot show (removed or unreadable items), so \
         positions do not line up with `sss playlist`; {advice}",
        playlist.name
    )
}

/// The playlist `sss playlist create` will make, as sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToCreate {
    pub name: String,
    pub public: bool,
    pub collaborative: bool,
    pub description: String,
}

/// What `sss playlist create` reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CreateReport {
    pub profile: String,
    pub dry_run: bool,
    pub to_create: ToCreate,
    /// The new playlist; `None` in a dry run.
    pub created: Option<Playlist>,
    /// The member's own playlists of that name.
    pub existing: Vec<Playlist>,
    /// After a live run, the snapshot id Spotify gave the new playlist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<Option<String>>,
}

/// `sss playlist create` as asked.
pub struct Create<'a> {
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub public: bool,
    pub collaborative: bool,
    /// Make the playlist even when the member owns one of that name.
    pub duplicate: bool,
    pub dry_run: bool,
}

/// `sss playlist create`: a new playlist of the member's, refused when
/// they own one of that name unless `duplicate`.
pub fn create<S: BorrowMut<Spotify>>(
    profile: &str,
    args: &Create,
    connect: impl FnOnce() -> Result<S>,
    err: &mut dyn Write,
) -> Result<CreateReport> {
    let to_create = ToCreate {
        name: playlist_name(args.name)?,
        public: args.public,
        collaborative: args.collaborative,
        description: description(args.description.unwrap_or(""), None, err),
    };
    let mut client = connect()?;
    let c = client.borrow_mut();
    c.require_scopes(profile, PLAYLIST_WRITE_SCOPES)?;
    let user = c.me().context("reading the Spotify account")?;
    let wanted = folded(&to_create.name);
    let existing: Vec<Playlist> = c
        .my_playlists()
        .context("listing the playlists")?
        .into_iter()
        .filter(|p| p.owner.id == user.id && folded(&p.name) == wanted)
        .collect();
    if !existing.is_empty() && !args.duplicate {
        let uris: Vec<&str> = existing.iter().map(|p| p.id.as_str()).collect();
        fail!(
            Kind::Input,
            "a playlist named {:?} already exists ({}); pass --duplicate to make another",
            to_create.name,
            uris.join(", ")
        );
    }
    let (created, snapshot_id) = if args.dry_run {
        (None, None)
    } else {
        let made = c
            .playlist_create(
                &to_create.name,
                to_create.public,
                to_create.collaborative,
                &to_create.description,
            )
            .with_context(|| format!("creating playlist {:?}", to_create.name))?;
        let playlist = Playlist {
            id: ContextKind::Playlist.uri(&made.id),
            name: to_create.name.clone(),
            owner: Owner {
                id: user.id.clone(),
                name: user.name.clone(),
            },
            public: Some(to_create.public),
            collaborative: to_create.collaborative,
            total: Some(0),
            description: Some(to_create.description.clone()).filter(|d| !d.is_empty()),
            snapshot_id: made.snapshot_id.clone(),
        };
        (Some(playlist), Some(made.snapshot_id))
    };
    Ok(CreateReport {
        profile: profile.to_owned(),
        dry_run: args.dry_run,
        to_create,
        created,
        existing,
        snapshot_id,
    })
}

/// `sss playlist edit` as asked; `None` leaves a detail as it is.
pub struct Edit<'a> {
    pub name: Option<&'a str>,
    pub description: Option<&'a str>,
    pub public: Option<bool>,
    pub collaborative: Option<bool>,
    pub dry_run: bool,
}

/// What `sss playlist edit` reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EditReport {
    pub profile: String,
    pub dry_run: bool,
    /// The playlist with the changes applied.
    pub playlist: Playlist,
    pub pushed_as: Option<String>,
    /// What is sent: only the details asked for.
    pub changes: PlaylistChanges,
    /// The playlist as read before the change.
    pub before: Playlist,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<Option<String>>,
}

/// `sss playlist edit`: changes the details of a playlist the member owns.
/// A pushed playlist's description keeps its `sss:<key>` marker, and a
/// rename is recorded in its ledger entry once Spotify has it.
pub fn edit<S: BorrowMut<Spotify>>(
    profile: &str,
    paths: &ProfilePaths,
    target: &str,
    args: &Edit,
    connect: impl FnOnce() -> Result<S>,
    err: &mut dyn Write,
) -> Result<EditReport> {
    let name = args.name.map(playlist_name).transpose()?;
    let (ledger, parsed) = parse(paths, target)?;
    let mut client = connect()?;
    let c = client.borrow_mut();
    c.require_scopes(profile, PLAYLIST_WRITE_SCOPES)?;
    let opened = open(c, &parsed, &ledger)?;
    let before = &opened.playlist;
    if !opened.owned() {
        fail!(
            Kind::Input,
            "{:?} is {}'s; only its owner can change a playlist's details",
            before.name,
            before.owner.name
        );
    }
    let public = args.public.or(before.public);
    let collaborative = args.collaborative.unwrap_or(before.collaborative);
    if collaborative && public == Some(true) {
        fail!(
            Kind::Input,
            "{:?} would be public and collaborative; Spotify keeps a collaborative playlist \
             private, so pass --private to have it collaborative, or --no-collaborative to \
             have it public",
            before.name
        );
    }
    let description = args
        .description
        .map(|text| description(text, opened.pushed_as.as_deref(), err));
    let changes = PlaylistChanges {
        name,
        description,
        public: args.public,
        collaborative: args.collaborative,
    };
    let playlist = Playlist {
        name: changes.name.clone().unwrap_or_else(|| before.name.clone()),
        description: match &changes.description {
            Some(d) => Some(d.clone()).filter(|d| !d.is_empty()),
            None => before.description.clone(),
        },
        public,
        collaborative,
        ..before.clone()
    };
    let snapshot_id = if args.dry_run {
        None
    } else {
        c.playlist_change(&opened.id, &changes)
            .with_context(|| format!("changing playlist {}", before.id))?;
        if let (Some(new), Some(key)) = (&changes.name, &opened.pushed_as) {
            record_rename(paths, key, &opened.id, new).with_context(|| {
                format!(
                    "renamed {} in Spotify, but recording the name in {} failed",
                    before.id,
                    paths.spotify_state().display()
                )
            })?;
        }
        // Spotify answers a details change with no snapshot id.
        Some(None)
    };
    Ok(EditReport {
        profile: profile.to_owned(),
        dry_run: args.dry_run,
        before: opened.playlist.clone(),
        playlist,
        pushed_as: opened.pushed_as,
        changes,
        snapshot_id,
    })
}

/// Sets the name of ledger entry `key` to `name` when it is still the
/// created entry of playlist `id`. The ledger is loaded here, after the
/// rename landed, so that what another command saved since this one
/// started is kept.
fn record_rename(paths: &ProfilePaths, key: &str, id: &str, new: &str) -> Result<()> {
    let path = paths.spotify_state();
    let mut ledger = Ledger::load(&path)?;
    if let Some(PlaylistEntry::Created {
        spotify_id, name, ..
    }) = ledger.playlists.get_mut(key)
        && spotify_id == id
    {
        *name = Some(new.to_owned());
        ledger.save(&path)?;
    }
    Ok(())
}

/// What `sss playlist add` reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AddReport {
    pub profile: String,
    pub dry_run: bool,
    pub playlist: Playlist,
    pub pushed_as: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "repo::is_zero")]
    pub duplicates_dropped: usize,
    /// Entries the contents do not show.
    pub unavailable: u32,
    /// What is added, in order: the items not in the playlist, or every
    /// item with `--duplicates`.
    pub to_add: Vec<String>,
    /// The items the playlist already holds.
    pub already_present: Vec<String>,
    /// The 1-based position the items go in at; `None` appends.
    pub position: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<Option<String>>,
}

/// `sss playlist add` as asked.
pub struct Add<'a> {
    pub items: Items<'a>,
    /// The 1-based position to insert at; `None` appends.
    pub at: Option<u32>,
    /// Add the items the playlist already holds too.
    pub duplicates: bool,
}

/// `sss playlist add`: appends tracks, or inserts them at `at`, skipping
/// those the playlist holds unless `duplicates`. A failure after a chunk
/// landed is a [`PartialWrite`](crate::error::PartialWrite).
pub fn add<S: BorrowMut<Spotify>>(
    profile: &str,
    paths: &ProfilePaths,
    target: &str,
    args: &Add,
    input: &mut dyn BufRead,
    connect: impl FnOnce() -> Result<S>,
    err: &mut dyn Write,
) -> Result<AddReport> {
    let given = targets::given(paths, PLAYLIST_ITEMS, &args.items, input)?;
    let (ledger, parsed) = parse(paths, target)?;
    let mut client = connect()?;
    let c = client.borrow_mut();
    c.require_scopes(profile, PLAYLIST_WRITE_SCOPES)?;
    let opened = open(c, &parsed, &ledger)?;
    opened.check_contents_writable()?;
    let (items, hidden) = opened.contents(c)?;
    if let Some(at) = args.at {
        check_positions(
            &opened.playlist,
            hidden,
            "add without --at, or use `sss playlist replace`",
        )?;
        let total = u32::try_from(items.len()).unwrap_or(u32::MAX);
        if at == 0 || at > total + 1 {
            fail!(
                Kind::Input,
                "--at {at} is outside {:?}, which holds {total} items; give 1 to {}",
                opened.playlist.name,
                total + 1
            );
        }
    }
    let present: HashSet<&str> = items.iter().map(|i| i.id.as_str()).collect();
    let (already_present, absent): (Vec<String>, Vec<String>) = given
        .uris
        .iter()
        .cloned()
        .partition(|uri| present.contains(uri.as_str()));
    let to_add = if args.duplicates { given.uris } else { absent };
    let snapshot_id = if args.items.dry_run {
        None
    } else {
        Some(add_items(c, &opened.id, &to_add, args.at, err)?)
    };
    Ok(AddReport {
        profile: profile.to_owned(),
        dry_run: args.items.dry_run,
        playlist: opened.playlist,
        pushed_as: opened.pushed_as,
        resolved: given.resolved,
        duplicates_dropped: given.duplicates_dropped,
        unavailable: hidden,
        to_add,
        already_present,
        position: args.at,
        snapshot_id,
    })
}

/// Adds `uris` to playlist `id` at the 1-based `at`, a progress line on
/// `err` per chunk landed; the last snapshot id Spotify gave.
fn add_items(
    c: &mut Spotify,
    id: &str,
    uris: &[String],
    at: Option<u32>,
    err: &mut dyn Write,
) -> Result<Option<String>> {
    let mut progress = Progress::new("added", uris.len(), err);
    let mut snapshot = None;
    let mut landed = |chunk: &[String], s: Option<&str>| -> Result<()> {
        progress.landed(chunk);
        snapshot = s.map(str::to_owned);
        Ok(())
    };
    let result = c.playlist_add_items(id, uris, at.map(|n| n - 1), &mut landed);
    result.map_err(|e| progress.failed(e))?;
    Ok(snapshot)
}

/// A track `sss playlist remove` takes out, as the contents show it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Removal {
    pub item: Offered,
    /// How often the playlist holds it; every occurrence goes.
    pub occurrences: usize,
}

/// What `sss playlist remove` reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RemoveReport {
    pub profile: String,
    pub dry_run: bool,
    pub playlist: Playlist,
    pub pushed_as: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "repo::is_zero")]
    pub duplicates_dropped: usize,
    pub unavailable: u32,
    /// The items the playlist holds, in the order given.
    pub to_remove: Vec<Removal>,
    /// The items it does not.
    pub not_present: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<Option<String>>,
}

/// `sss playlist remove`: every occurrence of each track leaves the
/// playlist, the removal carrying the snapshot id the details gave.
/// A failure after a chunk landed is a [`PartialWrite`](crate::error::PartialWrite).
pub fn remove<S: BorrowMut<Spotify>>(
    profile: &str,
    paths: &ProfilePaths,
    target: &str,
    items: &Items,
    input: &mut dyn BufRead,
    connect: impl FnOnce() -> Result<S>,
    err: &mut dyn Write,
) -> Result<RemoveReport> {
    let given = targets::given(paths, PLAYLIST_ITEMS, items, input)?;
    let (ledger, parsed) = parse(paths, target)?;
    let mut client = connect()?;
    let c = client.borrow_mut();
    c.require_scopes(profile, PLAYLIST_WRITE_SCOPES)?;
    let opened = open(c, &parsed, &ledger)?;
    opened.check_contents_writable()?;
    let (contents, hidden) = opened.contents(c)?;
    let mut held: HashMap<&str, (&Offered, usize)> = HashMap::new();
    for item in &contents {
        held.entry(item.id.as_str()).or_insert((item, 0)).1 += 1;
    }
    let mut to_remove = Vec::new();
    let mut not_present = Vec::new();
    for uri in &given.uris {
        match held.get(uri.as_str()) {
            Some((item, n)) => to_remove.push(Removal {
                item: (*item).clone(),
                occurrences: *n,
            }),
            None => not_present.push(uri.clone()),
        }
    }
    let snapshot_id = if items.dry_run {
        None
    } else {
        let uris: Vec<String> = to_remove.iter().map(|r| r.item.id.clone()).collect();
        let snapshot = opened.playlist.snapshot_id.as_deref();
        Some(remove_items(c, &opened.id, &uris, snapshot, err)?)
    };
    Ok(RemoveReport {
        profile: profile.to_owned(),
        dry_run: items.dry_run,
        playlist: opened.playlist,
        pushed_as: opened.pushed_as,
        resolved: given.resolved,
        duplicates_dropped: given.duplicates_dropped,
        unavailable: hidden,
        to_remove,
        not_present,
        snapshot_id,
    })
}

/// Removes `uris` from playlist `id`, a progress line on `err` per chunk
/// landed; the last snapshot id Spotify gave, else `snapshot`.
fn remove_items(
    c: &mut Spotify,
    id: &str,
    uris: &[String],
    snapshot: Option<&str>,
    err: &mut dyn Write,
) -> Result<Option<String>> {
    if uris.is_empty() {
        return Ok(None);
    }
    let mut progress = Progress::new("removed", uris.len(), err);
    let mut last = None;
    let mut landed = |chunk: &[String], s: Option<&str>| -> Result<()> {
        progress.landed(chunk);
        last = s.map(str::to_owned);
        Ok(())
    };
    let result = c.playlist_remove_items(id, uris, snapshot, &mut landed);
    result.map_err(|e| progress.failed(e))?;
    Ok(last)
}

/// Where `sss playlist move` puts the range: before a 1-based position, or
/// at the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    Before(u32),
    End,
}

/// `sss playlist move` as asked, positions 1-based.
pub struct Move {
    pub from: u32,
    pub count: u32,
    pub to: Destination,
    pub dry_run: bool,
}

/// What `sss playlist move` reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MoveReport {
    pub profile: String,
    pub dry_run: bool,
    pub playlist: Playlist,
    pub pushed_as: Option<String>,
    pub unavailable: u32,
    /// The 1-based position of the range's first item.
    pub from: u32,
    pub count: u32,
    /// The items of the range, in order.
    pub to_move: Vec<Offered>,
    /// The item they land before; `None` for the end.
    pub before: Option<Offered>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<Option<String>>,
}

/// `sss playlist move`: the `count` items from position `from` move before
/// position `to`, or to the end, in one reorder that carries the details'
/// snapshot id and is never resent. Refused, kind `input`, when the
/// contents hide an entry or a position is outside the playlist.
pub fn move_items<S: BorrowMut<Spotify>>(
    profile: &str,
    paths: &ProfilePaths,
    target: &str,
    args: &Move,
    connect: impl FnOnce() -> Result<S>,
) -> Result<MoveReport> {
    let (ledger, parsed) = parse(paths, target)?;
    let mut client = connect()?;
    let c = client.borrow_mut();
    c.require_scopes(profile, PLAYLIST_WRITE_SCOPES)?;
    let opened = open(c, &parsed, &ledger)?;
    opened.check_contents_writable()?;
    let (items, hidden) = opened.contents(c)?;
    check_positions(
        &opened.playlist,
        hidden,
        "move by `sss playlist replace` instead",
    )?;
    let (start, insert_before) = move_range(&opened.playlist.name, items.len(), args)?;
    let end = start + args.count as usize;
    let to_move = items[start..end].to_vec();
    let before = match args.to {
        Destination::Before(_) => items.get(insert_before).cloned(),
        Destination::End => None,
    };
    let snapshot_id = if args.dry_run {
        None
    } else {
        let snapshot = c
            .playlist_reorder(
                &opened.id,
                args.from - 1,
                u32::try_from(insert_before).unwrap_or(u32::MAX),
                args.count,
                opened.playlist.snapshot_id.as_deref(),
            )
            .with_context(|| format!("moving items of {}", opened.playlist.id))?;
        Some(snapshot)
    };
    Ok(MoveReport {
        profile: profile.to_owned(),
        dry_run: args.dry_run,
        playlist: opened.playlist,
        pushed_as: opened.pushed_as,
        unavailable: hidden,
        from: args.from,
        count: args.count,
        to_move,
        before,
        snapshot_id,
    })
}

/// The 0-based start of the range `args` names in a playlist `name` of
/// `len` items, and the 0-based index it moves before (`len` for the end);
/// kind `input` when the range or the destination is outside the playlist
/// or the destination is inside the range.
fn move_range(name: &str, len: usize, args: &Move) -> Result<(usize, usize)> {
    let (from, count) = (args.from as usize, args.count as usize);
    let last = (from + count).saturating_sub(1);
    if from == 0 || count == 0 || last > len {
        let range = if count == 1 {
            format!("position {from}")
        } else {
            format!("positions {from} to {last}")
        };
        fail!(
            Kind::Input,
            "{range} is outside {name:?}, which holds {len} items"
        );
    }
    let insert_before = match args.to {
        Destination::End => len,
        Destination::Before(m) => {
            let m = m as usize;
            if m == 0 || m > len + 1 {
                fail!(
                    Kind::Input,
                    "--before {m} is outside {name:?}, which holds {len} items; give 1 to {}",
                    len + 1
                );
            }
            if (from..=last).contains(&m) {
                fail!(
                    Kind::Input,
                    "--before {m} is inside the range moved ({from} to {last}); give a position \
                     outside it, or --to-end"
                );
            }
            m - 1
        }
    };
    Ok((from - 1, insert_before))
}

/// What `sss playlist replace` reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReplaceReport {
    pub profile: String,
    pub dry_run: bool,
    pub playlist: Playlist,
    pub pushed_as: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<BTreeMap<String, String>>,
    /// The entries the contents do not show; the replace erases them too.
    pub unavailable: u32,
    /// How many entries the playlist holds now.
    pub before: u32,
    /// How many it holds after.
    pub after: usize,
    /// The current items whose URI is not in the new list.
    pub removed: Vec<Offered>,
    /// The URIs of the new list the playlist does not hold, each once.
    pub added: Vec<String>,
    /// Whether the items kept change their relative order.
    pub reordered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<Option<String>>,
}

/// `sss playlist replace`: the playlist's contents become exactly the
/// list, in order and duplicates included; an empty list empties it. A
/// failure after the replace landed is a
/// [`PartialWrite`](crate::error::PartialWrite).
pub fn replace<S: BorrowMut<Spotify>>(
    profile: &str,
    paths: &ProfilePaths,
    target: &str,
    items: &Items,
    input: &mut dyn BufRead,
    connect: impl FnOnce() -> Result<S>,
    err: &mut dyn Write,
) -> Result<ReplaceReport> {
    let list = targets::resolve_items(paths, PLAYLIST_ITEMS, items.args, items.from_file, input)?;
    let resolved = targets::resolved(&list);
    let uris = targets::uris(&list);
    let (ledger, parsed) = parse(paths, target)?;
    let mut client = connect()?;
    let c = client.borrow_mut();
    c.require_scopes(profile, PLAYLIST_WRITE_SCOPES)?;
    let opened = open(c, &parsed, &ledger)?;
    opened.check_contents_writable()?;
    let (contents, hidden) = opened.contents(c)?;
    let change = Replacement::of(&contents, &uris);
    let snapshot_id = if items.dry_run {
        None
    } else {
        Some(replace_items(c, &opened.id, &uris, err)?)
    };
    let shown = u32::try_from(contents.len()).unwrap_or(u32::MAX);
    Ok(ReplaceReport {
        profile: profile.to_owned(),
        dry_run: items.dry_run,
        before: opened
            .playlist
            .total
            .unwrap_or(shown.saturating_add(hidden)),
        playlist: opened.playlist,
        pushed_as: opened.pushed_as,
        resolved,
        unavailable: hidden,
        after: uris.len(),
        removed: change.removed,
        added: change.added,
        reordered: change.reordered,
        snapshot_id,
    })
}

/// How a replace changes contents.
#[derive(Debug, PartialEq)]
struct Replacement {
    removed: Vec<Offered>,
    added: Vec<String>,
    reordered: bool,
}

impl Replacement {
    /// `contents` replaced by `uris`.
    fn of(contents: &[Offered], uris: &[String]) -> Self {
        let new: HashSet<&str> = uris.iter().map(String::as_str).collect();
        let current: HashSet<&str> = contents.iter().map(|i| i.id.as_str()).collect();
        let removed = contents
            .iter()
            .filter(|i| !new.contains(i.id.as_str()))
            .cloned()
            .collect();
        let mut seen = HashSet::new();
        let added = uris
            .iter()
            .filter(|u| !current.contains(u.as_str()) && seen.insert(u.as_str()))
            .cloned()
            .collect();
        // The kept URIs in the order of their first occurrence, before and
        // after: a change of how often one occurs is not a reorder.
        let order = |seq: Vec<&str>, keep: &HashSet<&str>| -> Vec<String> {
            let mut seen = HashSet::new();
            seq.into_iter()
                .filter(|u| keep.contains(u) && seen.insert(*u))
                .map(str::to_owned)
                .collect()
        };
        let was = order(contents.iter().map(|i| i.id.as_str()).collect(), &new);
        let will = order(uris.iter().map(String::as_str).collect(), &current);
        Replacement {
            removed,
            added,
            reordered: was != will,
        }
    }
}

/// Sets playlist `id` to `uris`: one replace with the first
/// [`PLAYLIST_CHUNK`], then an append per further chunk, a progress line on
/// `err` per request landed; the last snapshot id Spotify gave.
fn replace_items(
    c: &mut Spotify,
    id: &str,
    uris: &[String],
    err: &mut dyn Write,
) -> Result<Option<String>> {
    let (head, tail) = uris.split_at(uris.len().min(PLAYLIST_CHUNK));
    let mut progress = Progress::new("wrote", uris.len(), err);
    let mut snapshot = match c.playlist_replace_items(id, head) {
        Ok(s) => s,
        Err(e) => return Err(progress.failed(e)),
    };
    if !head.is_empty() {
        progress.landed(head);
    }
    let mut landed = |chunk: &[String], s: Option<&str>| -> Result<()> {
        progress.landed(chunk);
        snapshot = s.map(str::to_owned);
        Ok(())
    };
    let result = c.playlist_add_items(id, tail, None, &mut landed);
    result.map_err(|e| progress.failed(e))?;
    Ok(snapshot)
}

/// What `sss playlist delete` did, or a dry run would.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeleteAction {
    /// The member's own playlist, removed from the account.
    Deleted,
    /// Another's playlist, no longer followed.
    Unfollowed,
}

/// What `sss playlist delete` reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeleteReport {
    pub profile: String,
    pub dry_run: bool,
    pub playlist: Playlist,
    /// The ledger key of the push that made the playlist; the ledger is
    /// not changed.
    pub pushed_as: Option<String>,
    pub action: DeleteAction,
    /// The item count Spotify gave.
    pub items: Option<u32>,
}

/// `sss playlist delete`: removes the playlist from the account — a
/// delete when the member owns it, an unfollow otherwise. A playlist the
/// account does not hold is kind `input`.
pub fn delete<S: BorrowMut<Spotify>>(
    profile: &str,
    paths: &ProfilePaths,
    target: &str,
    dry_run: bool,
    connect: impl FnOnce() -> Result<S>,
) -> Result<DeleteReport> {
    let (ledger, parsed) = parse(paths, target)?;
    let mut client = connect()?;
    let c = client.borrow_mut();
    c.require_scopes(profile, PLAYLIST_FOLLOW_SCOPES)?;
    let opened = open(c, &parsed, &ledger)?;
    let uri = vec![ContextKind::Playlist.uri(&opened.id)];
    let held = c
        .library_contains(&uri)
        .context("checking the account's playlists")?;
    if held != [true] {
        fail!(
            Kind::Input,
            "{:?} is not on the account",
            opened.playlist.name
        );
    }
    let action = if opened.owned() {
        DeleteAction::Deleted
    } else {
        DeleteAction::Unfollowed
    };
    if !dry_run {
        c.library_remove(&uri, &mut |_| Ok(()))
            .with_context(|| format!("removing {} from the account", uri[0]))?;
    }
    Ok(DeleteReport {
        profile: profile.to_owned(),
        dry_run,
        items: opened.playlist.total,
        playlist: opened.playlist,
        pushed_as: opened.pushed_as,
        action,
    })
}

/// `spotify:playlist:… "Kids Songs"`, and its ledger key when pushed.
fn named(playlist: &Playlist, pushed_as: Option<&str>) -> String {
    match pushed_as {
        Some(key) => format!("{} {:?} ({key})", playlist.id, playlist.name),
        None => format!("{} {:?}", playlist.id, playlist.name),
    }
}

impl CreateReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        match &self.created {
            Some(playlist) => writeln!(out, "created {}", playlist_line(playlist, None))?,
            None => writeln!(
                out,
                "would create {:?} — {}{}",
                self.to_create.name,
                if self.to_create.public {
                    "public"
                } else {
                    "private"
                },
                if self.to_create.collaborative {
                    ", collaborative"
                } else {
                    ""
                }
            )?,
        }
        for playlist in &self.existing {
            writeln!(out, "  (already named so: {})", playlist.id)?;
        }
        Ok(())
    }
}

impl EditReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        let verb = if self.dry_run {
            "would change"
        } else {
            "changed"
        };
        writeln!(
            out,
            "{verb} {}",
            named(&self.before, self.pushed_as.as_deref())
        )?;
        let c = &self.changes;
        if let Some(name) = &c.name {
            writeln!(out, "  name: {:?} → {name:?}", self.before.name)?;
        }
        if let Some(d) = &c.description {
            writeln!(out, "  description: {d:?}")?;
        }
        if let Some(public) = c.public {
            writeln!(out, "  {}", if public { "public" } else { "private" })?;
        }
        if let Some(collaborative) = c.collaborative {
            writeln!(
                out,
                "  {}",
                if collaborative {
                    "collaborative"
                } else {
                    "not collaborative"
                }
            )?;
        }
        Ok(())
    }
}

impl AddReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        let verb = if self.dry_run { "would add" } else { "added" };
        let at = match self.position {
            Some(n) => format!(" at {n}"),
            None => String::new(),
        };
        writeln!(
            out,
            "{verb} {} to {}{at} ({} already there)",
            self.to_add.len(),
            named(&self.playlist, self.pushed_as.as_deref()),
            self.already_present.len()
        )?;
        for uri in &self.to_add {
            writeln!(out, "  {uri}")?;
        }
        targets::write_resolved(out, self.resolved.as_ref())
    }
}

impl RemoveReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        for removal in &self.to_remove {
            match removal.occurrences {
                1 => writeln!(out, "  {}", result_line(&removal.item))?,
                n => writeln!(out, "  {} (×{n})", result_line(&removal.item))?,
            }
        }
        let verb = if self.dry_run {
            "would remove"
        } else {
            "removed"
        };
        writeln!(
            out,
            "{verb} {} from {} ({} not there)",
            self.to_remove.len(),
            named(&self.playlist, self.pushed_as.as_deref()),
            self.not_present.len()
        )?;
        targets::write_resolved(out, self.resolved.as_ref())
    }
}

impl MoveReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        let verb = if self.dry_run { "would move" } else { "moved" };
        let items = if self.count == 1 { "item" } else { "items" };
        let to = match &self.before {
            Some(item) => format!("before {}", result_line(item)),
            None => "to the end".to_owned(),
        };
        writeln!(
            out,
            "{verb} {} {items} of {} from position {} {to}",
            self.count,
            named(&self.playlist, self.pushed_as.as_deref()),
            self.from
        )?;
        for item in &self.to_move {
            writeln!(out, "  {}", result_line(item))?;
        }
        Ok(())
    }
}

impl ReplaceReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        let verb = if self.dry_run {
            "would replace"
        } else {
            "replaced"
        };
        writeln!(
            out,
            "{verb} the {} items of {} with {} ({} added, {} removed{})",
            self.before,
            named(&self.playlist, self.pushed_as.as_deref()),
            self.after,
            self.added.len(),
            self.removed.len(),
            if self.reordered { ", reordered" } else { "" }
        )?;
        for item in &self.removed {
            writeln!(out, "  - {}", result_line(item))?;
        }
        if self.unavailable > 0 {
            writeln!(out, "  and {} unavailable items", self.unavailable)?;
        }
        for uri in &self.added {
            writeln!(out, "  + {uri}")?;
        }
        targets::write_resolved(out, self.resolved.as_ref())
    }
}

impl DeleteReport {
    pub fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
        let verb = match (self.action, self.dry_run) {
            (DeleteAction::Deleted, false) => "deleted",
            (DeleteAction::Deleted, true) => "would delete",
            (DeleteAction::Unfollowed, false) => "unfollowed",
            (DeleteAction::Unfollowed, true) => "would unfollow",
        };
        writeln!(
            out,
            "{verb} {}",
            named(&self.playlist, self.pushed_as.as_deref())
        )?;
        if self.action == DeleteAction::Deleted {
            writeln!(
                out,
                "Spotify keeps a deleted playlist recoverable for 90 days at {RECOVER_PLAYLISTS_URL}"
            )?;
            if self.pushed_as.is_some() {
                writeln!(
                    out,
                    "`sss push` leaves it alone while it is off the account; recover it there \
                     to have push maintain it again"
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{self, PartialWrite};
    use crate::matching::model::{self, MatchEntry, Status};
    use crate::repo::Repo;
    use crate::spotify::auth::{LIBRARY_WRITE_SCOPES, PLAYLIST_READ_SCOPES};
    use crate::spotify::ids::IdKind;
    use crate::spotify::test_support::client;
    use httpmock::prelude::*;
    use serde_json::{Value, json};
    use std::path::Path;

    const ID: &str = "37i9dQZF1DXcBWIGoYBM5M";
    const URI: &str = "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M";
    const TRACK: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";
    const OTHER: &str = "spotify:track:6rqhFgbbKwnb9MLmUQDhG6";

    fn paths(tmp: &Path) -> ProfilePaths {
        Repo::at(tmp.join("data")).profile("alice").unwrap()
    }

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn text(write: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> String {
        let mut out = Vec::new();
        write(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn no_client() -> Result<Spotify> {
        panic!("connected before the items and the target were resolved")
    }

    fn track(n: usize) -> String {
        format!("spotify:track:{n:022}")
    }

    fn me(server: &MockServer) -> httpmock::Mock<'_> {
        server.mock(|when, then| {
            when.method(GET).path("/v1/me");
            then.status(200)
                .json_body(json!({"id": "alice-spotify", "display_name": "Alice"}));
        })
    }

    /// The details of playlist `ID`, named "Kids Songs", holding `total`.
    fn details(server: &MockServer, owner: &str, collaborative: bool, total: u32) -> Value {
        let body = json!({"uri": URI, "name": "Kids Songs", "description": "",
                          "public": false, "collaborative": collaborative,
                          "owner": {"id": owner, "display_name": "Bob"},
                          "items": {"total": total}, "snapshot_id": "snap-0"});
        let answer = body.clone();
        server.mock(move |when, then| {
            when.method(GET)
                .path(format!("/v1/playlists/{ID}"))
                .query_param_exists("fields");
            then.status(200).json_body(answer);
        });
        body
    }

    fn entry(uri: &str) -> Value {
        json!({"item": {"type": "track", "uri": uri, "name": "Song",
                        "artists": [{"name": "Artist"}], "album": {"name": "Album"},
                        "duration_ms": 225000}})
    }

    /// The contents of playlist `ID`: `entries` as the items page.
    fn contents(server: &MockServer, entries: Vec<Value>) -> httpmock::Mock<'_> {
        server.mock(move |when, then| {
            when.method(GET).path(format!("/v1/playlists/{ID}/items"));
            then.status(200)
                .json_body(json!({"items": entries, "next": null}));
        })
    }

    /// Every write to playlist `ID`'s items.
    fn item_writes(server: &MockServer, method: httpmock::Method) -> httpmock::Mock<'_> {
        server.mock(move |when, then| {
            when.method(method)
                .path(format!("/v1/playlists/{ID}/items"));
            then.status(200).json_body(json!({"snapshot_id": "snap-1"}));
        })
    }

    fn items<'a>(given: &'a [String], dry_run: bool) -> Items<'a> {
        Items {
            args: given,
            from_file: None,
            dry_run,
        }
    }

    fn add_args(given: &[String], dry_run: bool) -> Add<'_> {
        Add {
            items: items(given, dry_run),
            at: None,
            duplicates: false,
        }
    }

    fn write_ledger(paths: &ProfilePaths, playlists: Value) -> Vec<u8> {
        let ledger: Ledger = serde_json::from_value(json!({ "playlists": playlists })).unwrap();
        std::fs::create_dir_all(paths.spotify_state().parent().unwrap()).unwrap();
        ledger.save(&paths.spotify_state()).unwrap();
        std::fs::read(paths.spotify_state()).unwrap()
    }

    #[test]
    fn every_write_checks_its_scopes_before_any_request() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let server = MockServer::start();
        let any = server.mock(|when, then| {
            when.any_request();
            then.status(200);
        });
        let read_only = || Ok(client(tmp.path(), &server, PLAYLIST_READ_SCOPES));
        let given = args(&[TRACK]);
        let mut errors = vec![
            create(
                "alice",
                &Create {
                    name: "x",
                    description: None,
                    public: false,
                    collaborative: false,
                    duplicate: false,
                    dry_run: true,
                },
                read_only,
                &mut Vec::new(),
            )
            .unwrap_err(),
            edit(
                "alice",
                &paths,
                URI,
                &Edit {
                    name: Some("y"),
                    description: None,
                    public: None,
                    collaborative: None,
                    dry_run: true,
                },
                read_only,
                &mut Vec::new(),
            )
            .unwrap_err(),
        ];
        errors.push(
            add(
                "alice",
                &paths,
                URI,
                &add_args(&given, true),
                &mut io::empty(),
                read_only,
                &mut Vec::new(),
            )
            .unwrap_err(),
        );
        errors.push(
            remove(
                "alice",
                &paths,
                URI,
                &items(&given, true),
                &mut io::empty(),
                read_only,
                &mut Vec::new(),
            )
            .unwrap_err(),
        );
        for err in &errors {
            assert_eq!(error::kind(err), Kind::Auth);
            assert!(
                format!("{err:#}").contains("playlist-modify-private"),
                "{err:#}"
            );
        }
        // A delete is a library removal of the playlist's URI.
        let err = delete("alice", &paths, URI, true, || {
            Ok(client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES))
        })
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Auth);
        assert!(
            format!("{err:#}").contains("user-library-modify"),
            "{err:#}"
        );
        let err = delete("alice", &paths, URI, true, || {
            Ok(client(tmp.path(), &server, LIBRARY_WRITE_SCOPES))
        })
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Auth);
        any.assert_calls(0);
    }

    #[test]
    fn items_and_the_target_are_resolved_before_connecting() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let missing = tmp.path().join("missing.txt");
        let none: Vec<String> = Vec::new();
        let from_missing = Add {
            items: Items {
                args: &none,
                from_file: Some(&missing),
                dry_run: false,
            },
            at: None,
            duplicates: false,
        };
        let err = add(
            "alice",
            &paths,
            URI,
            &from_missing,
            &mut io::empty(),
            no_client,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Other);

        let unmatched = args(&["TR:1"]);
        let err = remove(
            "alice",
            &paths,
            URI,
            &items(&unmatched, false),
            &mut io::empty(),
            no_client,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert!(format!("{err:#}").contains("has no match entry"), "{err:#}");

        let given = args(&[TRACK]);
        let err = add(
            "alice",
            &paths,
            "ST:1",
            &add_args(&given, false),
            &mut io::empty(),
            no_client,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert_eq!(
            format!("{err:#}"),
            "ST:1 has no Spotify playlist; run `sss push`"
        );
        let err = delete("alice", &paths, "ST:1", false, no_client).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);

        let empty = Items {
            args: &none,
            from_file: Some(Path::new("-")),
            dry_run: false,
        };
        let err = remove(
            "alice",
            &paths,
            URI,
            &empty,
            &mut io::Cursor::new("# nothing\n"),
            no_client,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(format!("{err:#}"), "no items given");
        let err = create(
            "alice",
            &Create {
                name: "  ",
                description: None,
                public: false,
                collaborative: false,
                duplicate: false,
                dry_run: false,
            },
            no_client,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
    }

    #[test]
    fn add_skips_what_the_playlist_holds_and_a_dry_run_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 1);
        contents(&server, vec![entry(TRACK)]);
        let post = server.mock(|when, then| {
            when.method(POST)
                .path(format!("/v1/playlists/{ID}/items"))
                .json_body(json!({"uris": [OTHER]}));
            then.status(201).json_body(json!({"snapshot_id": "snap-1"}));
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let given = args(&[TRACK, OTHER, OTHER]);
        let paths = paths(tmp.path());

        let dry = add(
            "alice",
            &paths,
            URI,
            &add_args(&given, true),
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap();
        post.assert_calls(0);
        assert_eq!(dry.to_add, [OTHER]);
        assert_eq!(dry.already_present, [TRACK]);
        assert_eq!(dry.duplicates_dropped, 1);
        let doc = serde_json::to_value(&dry).unwrap();
        assert_eq!(doc["dry_run"], true);
        assert_eq!(doc["unavailable"], 0);
        assert_eq!(doc["position"], Value::Null);
        assert!(doc.get("snapshot_id").is_none(), "{doc}");
        assert_eq!(doc["playlist"]["id"], URI);
        assert_eq!(
            text(|o| dry.write_text(o)),
            format!("would add 1 to {URI} \"Kids Songs\" (1 already there)\n  {OTHER}\n")
        );

        let mut err = Vec::new();
        let live = add(
            "alice",
            &paths,
            URI,
            &add_args(&given, false),
            &mut io::empty(),
            || Ok(&mut c),
            &mut err,
        )
        .unwrap();
        post.assert_calls(1);
        assert_eq!(live.snapshot_id, Some(Some("snap-1".into())));
        assert_eq!(String::from_utf8(err).unwrap(), "added 1 of 1\n");
        assert_eq!(
            text(|o| live.write_text(o)),
            format!("added 1 to {URI} \"Kids Songs\" (1 already there)\n  {OTHER}\n")
        );
    }

    #[test]
    fn add_with_duplicates_and_a_position_sends_every_item_there() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 2);
        contents(&server, vec![entry(TRACK), entry(OTHER)]);
        let post = server.mock(|when, then| {
            when.method(POST)
                .path(format!("/v1/playlists/{ID}/items"))
                .json_body(json!({"uris": [TRACK], "position": 1}));
            then.status(201).json_body(json!({"snapshot_id": "snap-1"}));
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let given = args(&[TRACK]);
        let paths = paths(tmp.path());
        let report = add(
            "alice",
            &paths,
            URI,
            &Add {
                at: Some(2),
                duplicates: true,
                ..add_args(&given, false)
            },
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap();
        post.assert();
        assert_eq!(report.to_add, [TRACK]);
        assert_eq!(report.position, Some(2));

        // 0 and past the end plus one are refused before any write.
        for at in [0, 4] {
            let err = add(
                "alice",
                &paths,
                URI,
                &Add {
                    at: Some(at),
                    ..add_args(&given, false)
                },
                &mut io::empty(),
                || Ok(&mut c),
                &mut Vec::new(),
            )
            .unwrap_err();
            assert_eq!(error::kind(&err), Kind::Input);
            assert!(
                format!("{err:#}").contains(&format!("--at {at} is outside")),
                "{err:#}"
            );
            assert!(
                format!("{err:#}").contains("holds 2 items; give 1 to 3"),
                "{err:#}"
            );
        }
        post.assert_calls(1);

        // The end plus one appends at Spotify's position `total`.
        let at_end = server.mock(|when, then| {
            when.method(POST)
                .path(format!("/v1/playlists/{ID}/items"))
                .json_body(json!({"uris": [TRACK], "position": 2}));
            then.status(201).json_body(json!({"snapshot_id": "snap-2"}));
        });
        add(
            "alice",
            &paths,
            URI,
            &Add {
                at: Some(3),
                duplicates: true,
                ..add_args(&given, false)
            },
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap();
        at_end.assert();
    }

    #[test]
    fn a_position_is_refused_when_the_contents_hide_an_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 3);
        contents(
            &server,
            vec![entry(TRACK), json!({"item": null}), entry(OTHER)],
        );
        let writes = item_writes(&server, POST);
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let given = args(&[&track(1)]);
        let paths = paths(tmp.path());
        let err = add(
            "alice",
            &paths,
            URI,
            &Add {
                at: Some(1),
                ..add_args(&given, false)
            },
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert_eq!(
            format!("{err:#}"),
            "\"Kids Songs\" holds 1 item sss cannot show (removed or unreadable items), so \
             positions do not line up with `sss playlist`; add without --at, or use `sss \
             playlist replace`"
        );
        writes.assert_calls(0);
        // Appending does not depend on positions.
        let report = add(
            "alice",
            &paths,
            URI,
            &add_args(&given, true),
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(report.unavailable, 1);
    }

    #[test]
    fn another_members_playlist_is_refused_unless_collaborative() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        me(&server);
        details(&server, "bob-spotify", false, 1);
        let read = contents(&server, vec![entry(TRACK)]);
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let given = args(&[OTHER]);
        let err = remove(
            "alice",
            &paths(tmp.path()),
            URI,
            &items(&given, false),
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert_eq!(
            format!("{err:#}"),
            "\"Kids Songs\" is Bob's and not collaborative; its contents cannot be changed from \
             this account (follow or unfollow it with `sss library add|remove`)"
        );
        read.assert_calls(0);

        let server = MockServer::start();
        me(&server);
        details(&server, "bob-spotify", true, 1);
        contents(&server, vec![entry(TRACK)]);
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let report = add(
            "alice",
            &paths(tmp.path()),
            URI,
            &add_args(&given, true),
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(report.to_add, [OTHER]);
    }

    #[test]
    fn a_name_target_is_read_like_a_uri() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        me(&server);
        let list = listing(
            &server,
            vec![json!({"uri": URI, "name": "Kids Songs", "description": "",
                        "public": false, "collaborative": false,
                        "owner": {"id": "alice-spotify", "display_name": "Alice"},
                        "items": {"total": 1}, "snapshot_id": "list-snap"})],
        );
        details(&server, "alice-spotify", false, 1);
        contents(&server, vec![entry(TRACK)]);
        let delete = server.mock(|when, then| {
            when.method(DELETE)
                .path(format!("/v1/playlists/{ID}/items"))
                .json_body(json!({"items": [{"uri": TRACK}], "snapshot_id": "snap-0"}));
            then.status(200).json_body(json!({"snapshot_id": "snap-1"}));
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let given = args(&[TRACK]);
        let report = remove(
            "alice",
            &paths(tmp.path()),
            " kids songs ",
            &items(&given, false),
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap();
        list.assert();
        // The snapshot sent is the details read's, not the listing's.
        delete.assert();
        assert_eq!(report.playlist.id, URI);
        assert_eq!(report.to_remove.len(), 1);
    }

    #[test]
    fn remove_reports_occurrences_and_sends_the_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 3);
        contents(&server, vec![entry(TRACK), entry(OTHER), entry(TRACK)]);
        let delete = server.mock(|when, then| {
            when.method(DELETE)
                .path(format!("/v1/playlists/{ID}/items"))
                .json_body(json!({"items": [{"uri": TRACK}], "snapshot_id": "snap-0"}));
            then.status(200).json_body(json!({"snapshot_id": "snap-1"}));
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let missing = track(7);
        let given = args(&[TRACK, &missing]);
        let paths = paths(tmp.path());
        let dry = remove(
            "alice",
            &paths,
            URI,
            &items(&given, true),
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap();
        delete.assert_calls(0);
        assert_eq!(dry.to_remove.len(), 1);
        assert_eq!(dry.to_remove[0].occurrences, 2);
        assert_eq!(dry.not_present, std::slice::from_ref(&missing));
        let doc = serde_json::to_value(&dry).unwrap();
        assert_eq!(doc["to_remove"][0]["item"]["name"], "Song");
        assert_eq!(
            text(|o| dry.write_text(o)),
            format!(
                "  {TRACK} \"Song\" — Artist — Album (3:45) (×2)\n\
                 would remove 1 from {URI} \"Kids Songs\" (1 not there)\n"
            )
        );

        let mut err = Vec::new();
        let live = remove(
            "alice",
            &paths,
            URI,
            &items(&given, false),
            &mut io::empty(),
            || Ok(&mut c),
            &mut err,
        )
        .unwrap();
        delete.assert();
        assert_eq!(live.snapshot_id, Some(Some("snap-1".into())));
        assert_eq!(String::from_utf8(err).unwrap(), "removed 1 of 1\n");

        // Nothing present: a success with no request.
        let none = args(&[&missing]);
        let empty = remove(
            "alice",
            &paths,
            URI,
            &items(&none, false),
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap();
        delete.assert_calls(1);
        assert!(empty.to_remove.is_empty());
    }

    #[test]
    fn item_ids_resolve_and_are_echoed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        model::append(
            &model::file(&paths, IdKind::Track),
            &MatchEntry {
                status: Status::Matched,
                spotify_uri: Some(OTHER.into()),
                ..MatchEntry::manual_reject("TR:1", "2026-10-19T00:00:00Z".into())
            },
        )
        .unwrap();
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 0);
        contents(&server, vec![]);
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let given = args(&["TR:1"]);
        let report = add(
            "alice",
            &paths,
            URI,
            &add_args(&given, true),
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&report).unwrap()["resolved"],
            json!({"TR:1": OTHER})
        );
        assert!(
            text(|o| report.write_text(o)).ends_with(&format!("  (TR:1 is {OTHER})\n")),
            "resolution not shown"
        );
    }

    #[test]
    fn a_failure_after_a_chunk_landed_reports_what_landed() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 0);
        contents(&server, vec![]);
        let all: Vec<String> = (0..230).map(track).collect();
        let first = server.mock(|when, then| {
            when.method(POST).body_includes(track(0));
            then.status(201).json_body(json!({"snapshot_id": "snap-1"}));
        });
        let second = server.mock(|when, then| {
            when.method(POST).body_includes(track(100));
            then.status(400)
                .json_body(json!({"error": {"status": 400, "message": "Invalid id"}}));
        });
        let third = server.mock(|when, then| {
            when.method(POST).body_includes(track(200));
            then.status(201).json_body(json!({"snapshot_id": "snap-3"}));
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let err = add(
            "alice",
            &paths(tmp.path()),
            URI,
            &add_args(&all, false),
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap_err();
        first.assert();
        second.assert();
        third.assert_calls(0);
        let partial = err.downcast_ref::<PartialWrite>().expect("a partial write");
        assert_eq!(partial.done, all[..100]);
        assert_eq!(partial.total, 230);
        assert_eq!(error::kind(&err), Kind::Other);
        assert!(
            format!("{err:#}").starts_with("added 100 of 230 ("),
            "{err:#}"
        );
    }

    fn listing(server: &MockServer, playlists: Vec<Value>) -> httpmock::Mock<'_> {
        server.mock(move |when, then| {
            when.method(GET).path("/v1/me/playlists");
            then.status(200)
                .json_body(json!({"items": playlists, "next": null}));
        })
    }

    fn listed(id: &str, name: &str, owner: &str) -> Value {
        json!({"uri": format!("spotify:playlist:{id}"), "name": name, "public": false,
               "collaborative": false, "owner": {"id": owner, "display_name": owner},
               "items": {"total": 4}})
    }

    fn create_args(name: &str, dry_run: bool) -> Create<'_> {
        Create {
            name,
            description: None,
            public: false,
            collaborative: false,
            duplicate: false,
            dry_run,
        }
    }

    #[test]
    fn create_refuses_a_name_the_member_owns_unless_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        me(&server);
        listing(
            &server,
            vec![
                listed(ID, "Kids Songs", "alice-spotify"),
                listed("4uLU6hMCjMI75M1A2tKUQC", "Road trip", "bob-spotify"),
            ],
        );
        let post = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/me/playlists")
                .json_body(json!({"name": "kids songs", "public": true,
                                  "description": "For the car"}));
            then.status(201)
                .json_body(json!({"id": "6rqhFgbbKwnb9MLmUQDhG6", "snapshot_id": "s"}));
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let err = create(
            "alice",
            &create_args(" kids songs ", false),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert_eq!(
            format!("{err:#}"),
            format!(
                "a playlist named \"kids songs\" already exists ({URI}); pass --duplicate to \
                 make another"
            )
        );
        post.assert_calls(0);

        let args = Create {
            description: Some("For the car"),
            public: true,
            duplicate: true,
            ..create_args("kids songs", false)
        };
        let report = create("alice", &args, || Ok(&mut c), &mut Vec::new()).unwrap();
        post.assert();
        assert_eq!(report.existing.len(), 1);
        let created = report.created.as_ref().unwrap();
        assert_eq!(created.id, "spotify:playlist:6rqhFgbbKwnb9MLmUQDhG6");
        assert_eq!(created.owner.id, "alice-spotify");
        assert_eq!(report.snapshot_id, Some(Some("s".into())));
        assert_eq!(
            text(|o| report.write_text(o)),
            format!(
                "created spotify:playlist:6rqhFgbbKwnb9MLmUQDhG6 \"kids songs\" — 0 items, \
                 public\n  (already named so: {URI})\n"
            )
        );
    }

    #[test]
    fn create_ignores_a_followed_playlist_of_the_name_and_sends_collaborative() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        me(&server);
        listing(&server, vec![listed(ID, "Road trip", "bob-spotify")]);
        let post = server.mock(|when, then| {
            when.method(POST).path("/v1/me/playlists").json_body(
                json!({"name": "Road trip", "public": false, "collaborative": true,
                       "description": ""}),
            );
            then.status(201)
                .json_body(json!({"id": "6rqhFgbbKwnb9MLmUQDhG6"}));
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let args = Create {
            collaborative: true,
            ..create_args("Road trip", true)
        };
        let dry = create("alice", &args, || Ok(&mut c), &mut Vec::new()).unwrap();
        post.assert_calls(0);
        assert_eq!(dry.created, None);
        assert!(dry.existing.is_empty());
        assert_eq!(
            text(|o| dry.write_text(o)),
            "would create \"Road trip\" — private, collaborative\n"
        );
        let doc = serde_json::to_value(&dry).unwrap();
        assert_eq!(
            doc["to_create"],
            json!({"name": "Road trip", "public": false, "collaborative": true,
                   "description": ""})
        );
        let live = create(
            "alice",
            &Create {
                dry_run: false,
                ..args
            },
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap();
        post.assert();
        assert!(live.created.unwrap().collaborative);
    }

    #[test]
    fn a_long_description_is_cut_with_a_warning() {
        let mut err = Vec::new();
        let long = "é".repeat(DESCRIPTION_MAX + 5);
        assert_eq!(
            description(&long, None, &mut err).chars().count(),
            DESCRIPTION_MAX
        );
        assert!(String::from_utf8(err).unwrap().starts_with("warning: "));
        let mut err = Vec::new();
        assert_eq!(description("short", None, &mut err), "short");
        assert!(err.is_empty());
        // On a pushed playlist the marker takes its share: text that fits
        // in 300 alone is cut, with the warning, and the marker kept.
        let mut err = Vec::new();
        let text = "x".repeat(DESCRIPTION_MAX - 3);
        let sent = description(&text, Some("ST:1"), &mut err);
        assert_eq!(sent.chars().count(), DESCRIPTION_MAX);
        assert!(sent.ends_with(" sss:ST:1"));
        let warned = String::from_utf8(err).unwrap();
        assert!(warned.starts_with("warning: ") && warned.contains("sss:ST:1"));
        let mut err = Vec::new();
        assert_eq!(
            description("  short  ", Some("ST:1"), &mut err),
            "short sss:ST:1"
        );
        assert!(err.is_empty());
        // A description read back from the playlist keeps one marker; text
        // that only ends in the key's letters, or another key's marker, is
        // kept as given.
        for (given, sent) in [
            ("Kids songs sss:ST:1", "Kids songs sss:ST:1"),
            ("Kids songs sss:ST:1 sss:ST:1 ", "Kids songs sss:ST:1"),
            ("sss:ST:1", "sss:ST:1"),
            ("Kids xsss:ST:1", "Kids xsss:ST:1 sss:ST:1"),
            ("Kids sss:ST:12", "Kids sss:ST:12 sss:ST:1"),
        ] {
            assert_eq!(description(given, Some("ST:1"), &mut Vec::new()), sent);
        }
        let text = format!("{} sss:ST:1", "x".repeat(push::marker_room("ST:1")));
        let mut err = Vec::new();
        assert_eq!(description(&text, Some("ST:1"), &mut err), text);
        assert!(err.is_empty());
        assert!(playlist_name(&"x".repeat(NAME_MAX + 1)).is_err());
        assert_eq!(
            playlist_name(&"x".repeat(NAME_MAX)).unwrap().len(),
            NAME_MAX
        );
    }

    fn edit_args() -> Edit<'static> {
        Edit {
            name: None,
            description: None,
            public: None,
            collaborative: None,
            dry_run: false,
        }
    }

    #[test]
    fn edit_requires_ownership_and_a_private_collaborative_playlist() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let server = MockServer::start();
        me(&server);
        details(&server, "bob-spotify", true, 1);
        let put = server.mock(|when, then| {
            when.method(PUT).path(format!("/v1/playlists/{ID}"));
            then.status(200);
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let args = Edit {
            name: Some("y"),
            ..edit_args()
        };
        let err = edit("alice", &paths, URI, &args, || Ok(&mut c), &mut Vec::new()).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert_eq!(
            format!("{err:#}"),
            "\"Kids Songs\" is Bob's; only its owner can change a playlist's details"
        );

        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 1);
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let args = Edit {
            public: Some(true),
            collaborative: Some(true),
            ..edit_args()
        };
        let err = edit("alice", &paths, URI, &args, || Ok(&mut c), &mut Vec::new()).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert!(format!("{err:#}").contains("--private"), "{err:#}");
        put.assert_calls(0);

        // A collaborative playlist made public, with neither collaborative
        // flag.
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", true, 1);
        let put = server.mock(|when, then| {
            when.method(PUT).path(format!("/v1/playlists/{ID}"));
            then.status(200);
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let args = Edit {
            public: Some(true),
            ..edit_args()
        };
        let err = edit("alice", &paths, URI, &args, || Ok(&mut c), &mut Vec::new()).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert_eq!(
            format!("{err:#}"),
            "\"Kids Songs\" would be public and collaborative; Spotify keeps a collaborative \
             playlist private, so pass --private to have it collaborative, or \
             --no-collaborative to have it public"
        );
        put.assert_calls(0);
        let args = Edit {
            public: Some(true),
            collaborative: Some(false),
            ..edit_args()
        };
        let report = edit("alice", &paths, URI, &args, || Ok(&mut c), &mut Vec::new()).unwrap();
        put.assert();
        assert_eq!(report.playlist.public, Some(true));
        assert!(!report.playlist.collaborative);
    }

    #[test]
    fn edit_of_a_pushed_playlist_keeps_the_marker_and_renames_the_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let before = write_ledger(
            &paths,
            json!({"ST:1": {"status": "created", "spotify_id": ID, "name": "Pandora: Kids",
                            "items": [TRACK]}}),
        );
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 1);
        let put = server.mock(|when, then| {
            when.method(PUT)
                .path(format!("/v1/playlists/{ID}"))
                .json_body(json!({"name": "Kids", "description": "Songs sss:ST:1",
                                  "public": false}));
            then.status(200);
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let args = Edit {
            name: Some(" Kids "),
            description: Some("Songs"),
            public: Some(false),
            dry_run: true,
            ..edit_args()
        };
        let dry = edit(
            "alice",
            &paths,
            "ST:1",
            &args,
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap();
        put.assert_calls(0);
        assert_eq!(std::fs::read(paths.spotify_state()).unwrap(), before);
        assert_eq!(dry.pushed_as.as_deref(), Some("ST:1"));
        assert_eq!(dry.playlist.name, "Kids");
        assert_eq!(dry.before.name, "Kids Songs");
        assert_eq!(
            text(|o| dry.write_text(o)),
            format!(
                "would change {URI} \"Kids Songs\" (ST:1)\n  name: \"Kids Songs\" → \"Kids\"\n  \
                 description: \"Songs sss:ST:1\"\n  private\n"
            )
        );

        let live = edit(
            "alice",
            &paths,
            URI,
            &Edit {
                dry_run: false,
                ..args
            },
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap();
        put.assert();
        assert_eq!(live.pushed_as.as_deref(), Some("ST:1"));
        let doc = serde_json::to_value(&live).unwrap();
        assert_eq!(
            doc["changes"],
            json!({"name": "Kids", "description": "Songs sss:ST:1", "public": false})
        );
        assert_eq!(doc["snapshot_id"], Value::Null);
        let ledger = Ledger::load(&paths.spotify_state()).unwrap();
        let Some(PlaylistEntry::Created { name, items, .. }) = ledger.playlists.get("ST:1") else {
            panic!("no created entry")
        };
        assert_eq!(name.as_deref(), Some("Kids"));
        assert_eq!(items, &[TRACK]);
    }

    #[test]
    fn a_failed_rename_leaves_the_ledger_as_it_was() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let before = write_ledger(
            &paths,
            json!({"ST:1": {"status": "created", "spotify_id": ID, "name": "Pandora: Kids"}}),
        );
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 1);
        server.mock(|when, then| {
            when.method(PUT).path(format!("/v1/playlists/{ID}"));
            then.status(400)
                .json_body(json!({"error": {"status": 400, "message": "Bad name"}}));
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let args = Edit {
            name: Some("Kids"),
            ..edit_args()
        };
        let err = edit(
            "alice",
            &paths,
            "ST:1",
            &args,
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Other);
        assert_eq!(std::fs::read(paths.spotify_state()).unwrap(), before);
    }

    #[test]
    fn a_rename_keeps_what_was_saved_while_it_ran() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        write_ledger(
            &paths,
            json!({"ST:1": {"status": "created", "spotify_id": ID, "name": "Pandora: Kids"}}),
        );
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 1);
        server.mock(|when, then| {
            when.method(PUT).path(format!("/v1/playlists/{ID}"));
            then.status(200);
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let args = Edit {
            name: Some("Kids"),
            ..edit_args()
        };
        // Another writer saves an entry after the target was parsed.
        edit(
            "alice",
            &paths,
            "ST:1",
            &args,
            || {
                write_ledger(
                    &paths,
                    json!({"ST:1": {"status": "created", "spotify_id": ID,
                                    "name": "Pandora: Kids", "items": [TRACK]},
                           "ST:2": {"status": "created", "spotify_id": "6rqhFgbbKwnb9MLmUQDhG6"}}),
                );
                Ok(&mut c)
            },
            &mut Vec::new(),
        )
        .unwrap();
        let ledger = Ledger::load(&paths.spotify_state()).unwrap();
        let Some(PlaylistEntry::Created { name, items, .. }) = ledger.playlists.get("ST:1") else {
            panic!("no created entry")
        };
        assert_eq!(name.as_deref(), Some("Kids"));
        assert_eq!(items, &[TRACK]);
        assert!(ledger.playlists.contains_key("ST:2"));
    }

    #[test]
    fn a_rename_that_cannot_be_recorded_says_it_landed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        write_ledger(
            &paths,
            json!({"ST:1": {"status": "created", "spotify_id": ID, "name": "Pandora: Kids"}}),
        );
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 1);
        let put = server.mock(|when, then| {
            when.method(PUT).path(format!("/v1/playlists/{ID}"));
            then.status(200);
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let args = Edit {
            name: Some("Kids"),
            ..edit_args()
        };
        let state = paths.spotify_state();
        let err = edit(
            "alice",
            &paths,
            "ST:1",
            &args,
            || {
                // The ledger becomes unreadable once the target is parsed.
                std::fs::remove_file(&state).unwrap();
                std::fs::create_dir(&state).unwrap();
                Ok(&mut c)
            },
            &mut Vec::new(),
        )
        .unwrap_err();
        put.assert();
        assert!(
            format!("{err:#}").starts_with(&format!(
                "renamed {URI} in Spotify, but recording the name in {} failed: ",
                state.display()
            )),
            "{err:#}"
        );
    }

    fn contains<'a>(server: &'a MockServer, answer: bool) -> httpmock::Mock<'a> {
        server.mock(move |when, then| {
            when.method(GET)
                .path("/v1/me/library/contains")
                .query_param("uris", URI);
            then.status(200).json_body(json!([answer]));
        })
    }

    #[test]
    fn delete_of_a_pushed_playlist_leaves_the_ledger_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let before = write_ledger(
            &paths,
            json!({"ST:1": {"status": "created", "spotify_id": ID, "items": [TRACK]}}),
        );
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 4);
        contains(&server, true);
        let gone = server.mock(|when, then| {
            when.method(DELETE)
                .path("/v1/me/library")
                .query_param("uris", URI);
            then.status(200);
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_FOLLOW_SCOPES);
        let dry = delete("alice", &paths, URI, true, || Ok(&mut c)).unwrap();
        gone.assert_calls(0);
        assert_eq!(dry.action, DeleteAction::Deleted);
        let report = delete("alice", &paths, URI, false, || Ok(&mut c)).unwrap();
        gone.assert();
        assert_eq!(std::fs::read(paths.spotify_state()).unwrap(), before);
        let doc = serde_json::to_value(&report).unwrap();
        assert_eq!(doc["action"], "deleted");
        assert_eq!(doc["pushed_as"], "ST:1");
        assert_eq!(doc["items"], 4);
        assert_eq!(
            text(|o| report.write_text(o)),
            format!(
                "deleted {URI} \"Kids Songs\" (ST:1)\n\
                 Spotify keeps a deleted playlist recoverable for 90 days at {RECOVER_PLAYLISTS_URL}\n\
                 `sss push` leaves it alone while it is off the account; recover it there to \
                 have push maintain it again\n"
            )
        );
    }

    #[test]
    fn delete_unfollows_another_members_playlist_and_refuses_one_not_held() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let server = MockServer::start();
        me(&server);
        details(&server, "bob-spotify", false, 4);
        let mut held = contains(&server, true);
        server.mock(|when, then| {
            when.method(DELETE).path("/v1/me/library");
            then.status(200);
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_FOLLOW_SCOPES);
        let report = delete("alice", &paths, URI, false, || Ok(&mut c)).unwrap();
        assert_eq!(report.action, DeleteAction::Unfollowed);
        assert_eq!(report.pushed_as, None);
        assert_eq!(
            text(|o| report.write_text(o)),
            format!("unfollowed {URI} \"Kids Songs\"\n")
        );

        held.delete();
        contains(&server, false);
        let err = delete("alice", &paths, URI, false, || Ok(&mut c)).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert_eq!(format!("{err:#}"), "\"Kids Songs\" is not on the account");
    }

    fn move_args(from: u32, count: u32, to: Destination, dry_run: bool) -> Move {
        Move {
            from,
            count,
            to,
            dry_run,
        }
    }

    /// The [`Offered`] the contents give for `entry(uri)`.
    fn offered(uri: &str) -> Offered {
        Offered {
            artists: vec!["Artist".into()],
            album: Some("Album".into()),
            duration_ms: Some(225000),
            ..Offered::named(uri, "Song")
        }
    }

    #[test]
    fn move_and_replace_check_scopes_and_resolve_before_connecting() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(tmp.path());
        let server = MockServer::start();
        let any = server.mock(|when, then| {
            when.any_request();
            then.status(200);
        });
        let read_only = || Ok(client(tmp.path(), &server, PLAYLIST_READ_SCOPES));
        let to_end = move_args(1, 1, Destination::End, true);
        let err = move_items("alice", &paths, URI, &to_end, read_only).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Auth);
        let none: Vec<String> = Vec::new();
        let err = replace(
            "alice",
            &paths,
            URI,
            &items(&none, true),
            &mut io::empty(),
            read_only,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Auth);
        assert!(
            format!("{err:#}").contains("playlist-modify-private"),
            "{err:#}"
        );
        any.assert_calls(0);

        let err = move_items("alice", &paths, "ST:1", &to_end, no_client).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "ST:1 has no Spotify playlist; run `sss push`"
        );
        let unmatched = args(&[TRACK, "TR:1"]);
        let err = replace(
            "alice",
            &paths,
            URI,
            &items(&unmatched, false),
            &mut io::empty(),
            no_client,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert!(format!("{err:#}").contains("has no match entry"), "{err:#}");
    }

    #[test]
    fn move_sends_the_zero_based_reorder_with_the_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 4);
        let held: Vec<String> = (0..4).map(track).collect();
        contents(&server, held.iter().map(|u| entry(u)).collect());
        let before = server.mock(|when, then| {
            when.method(PUT)
                .path(format!("/v1/playlists/{ID}/items"))
                .json_body(
                    json!({"range_start": 0, "insert_before": 3, "range_length": 2,
                                  "snapshot_id": "snap-0"}),
                );
            then.status(200).json_body(json!({"snapshot_id": "snap-1"}));
        });
        let to_end = server.mock(|when, then| {
            when.method(PUT)
                .path(format!("/v1/playlists/{ID}/items"))
                .json_body(
                    json!({"range_start": 1, "insert_before": 4, "range_length": 1,
                                  "snapshot_id": "snap-0"}),
                );
            then.status(200).json_body(json!({"snapshot_id": "snap-2"}));
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let paths = paths(tmp.path());

        let dry = move_items(
            "alice",
            &paths,
            URI,
            &move_args(1, 2, Destination::Before(4), true),
            || Ok(&mut c),
        )
        .unwrap();
        before.assert_calls(0);
        assert_eq!(dry.to_move, [offered(&held[0]), offered(&held[1])]);
        assert_eq!(dry.before, Some(offered(&held[3])));
        let doc = serde_json::to_value(&dry).unwrap();
        assert_eq!(doc["unavailable"], 0);
        assert_eq!(doc["from"], 1);
        assert_eq!(doc["count"], 2);
        assert!(doc.get("snapshot_id").is_none(), "{doc}");
        assert_eq!(
            text(|o| dry.write_text(o)),
            format!(
                "would move 2 items of {URI} \"Kids Songs\" from position 1 before {}\n  {}\n  {}\n",
                result_line(&offered(&held[3])),
                result_line(&offered(&held[0])),
                result_line(&offered(&held[1]))
            )
        );

        let live = move_items(
            "alice",
            &paths,
            URI,
            &move_args(1, 2, Destination::Before(4), false),
            || Ok(&mut c),
        )
        .unwrap();
        before.assert();
        assert_eq!(live.snapshot_id, Some(Some("snap-1".into())));

        let end = move_items(
            "alice",
            &paths,
            URI,
            &move_args(2, 1, Destination::End, false),
            || Ok(&mut c),
        )
        .unwrap();
        to_end.assert();
        assert_eq!(end.before, None);
        assert_eq!(
            text(|o| end.write_text(o)).lines().next().unwrap(),
            format!("moved 1 item of {URI} \"Kids Songs\" from position 2 to the end")
        );
    }

    #[test]
    fn a_move_outside_the_playlist_or_into_its_own_range_is_refused() {
        let at = |from, count, to| move_range("Kids Songs", 4, &move_args(from, count, to, false));
        assert_eq!(at(1, 2, Destination::Before(4)).unwrap(), (0, 3));
        assert_eq!(at(4, 1, Destination::Before(1)).unwrap(), (3, 0));
        assert_eq!(at(1, 1, Destination::Before(5)).unwrap(), (0, 4));
        assert_eq!(at(3, 2, Destination::End).unwrap(), (2, 4));
        for (from, count, to, message) in [
            (
                0,
                1,
                Destination::End,
                "position 0 is outside \"Kids Songs\", which holds 4 items",
            ),
            (
                5,
                1,
                Destination::End,
                "position 5 is outside \"Kids Songs\", which holds 4 items",
            ),
            (
                3,
                3,
                Destination::End,
                "positions 3 to 5 is outside \"Kids Songs\", which holds 4 items",
            ),
            (
                1,
                1,
                Destination::Before(6),
                "--before 6 is outside \"Kids Songs\", which holds 4 items; give 1 to 5",
            ),
            (
                2,
                2,
                Destination::Before(3),
                "--before 3 is inside the range moved (2 to 3); give a position outside it, \
                 or --to-end",
            ),
            (
                2,
                2,
                Destination::Before(2),
                "--before 2 is inside the range moved (2 to 3); give a position outside it, \
                 or --to-end",
            ),
        ] {
            let err = at(from, count, to).unwrap_err();
            assert_eq!(error::kind(&err), Kind::Input);
            assert_eq!(format!("{err:#}"), message);
        }
        let err = move_range("Empty", 0, &move_args(1, 1, Destination::End, false)).unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
    }

    #[test]
    fn a_move_is_refused_when_the_contents_hide_an_entry_and_never_resent() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 3);
        contents(
            &server,
            vec![entry(TRACK), json!({"item": null}), entry(OTHER)],
        );
        let writes = item_writes(&server, PUT);
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let paths = paths(tmp.path());
        let err = move_items(
            "alice",
            &paths,
            URI,
            &move_args(1, 1, Destination::End, false),
            || Ok(&mut c),
        )
        .unwrap_err();
        assert_eq!(error::kind(&err), Kind::Input);
        assert_eq!(
            format!("{err:#}"),
            "\"Kids Songs\" holds 1 item sss cannot show (removed or unreadable items), so \
             positions do not line up with `sss playlist`; move by `sss playlist replace` \
             instead"
        );
        writes.assert_calls(0);

        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 2);
        contents(&server, vec![entry(TRACK), entry(OTHER)]);
        let reorder = server.mock(|when, then| {
            when.method(PUT).path(format!("/v1/playlists/{ID}/items"));
            then.status(503);
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let err = move_items(
            "alice",
            &paths,
            URI,
            &move_args(1, 1, Destination::End, false),
            || Ok(&mut c),
        )
        .unwrap_err();
        reorder.assert_calls(1);
        assert_eq!(error::kind(&err), Kind::Unavailable);
    }

    #[test]
    fn replace_reports_what_goes_what_comes_and_the_order() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 4);
        let t: Vec<String> = (0..4).map(track).collect();
        contents(
            &server,
            vec![
                entry(&t[0]),
                entry(&t[1]),
                json!({"item": null}),
                entry(&t[2]),
            ],
        );
        let new = args(&[&t[2], &t[0], &t[3], &t[3]]);
        let put = server.mock(|when, then| {
            when.method(PUT)
                .path(format!("/v1/playlists/{ID}/items"))
                .json_body(json!({"uris": new}));
            then.status(200).json_body(json!({"snapshot_id": "snap-1"}));
        });
        let post = item_writes(&server, POST);
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let paths = paths(tmp.path());

        let dry = replace(
            "alice",
            &paths,
            URI,
            &items(&new, true),
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap();
        put.assert_calls(0);
        assert_eq!(dry.before, 4);
        assert_eq!(dry.after, 4);
        assert_eq!(dry.unavailable, 1);
        assert_eq!(dry.removed, [offered(&t[1])]);
        assert_eq!(dry.added, [t[3].clone()]);
        assert!(dry.reordered);
        assert!(
            serde_json::to_value(&dry)
                .unwrap()
                .get("resolved")
                .is_none()
        );
        assert_eq!(
            text(|o| dry.write_text(o)),
            format!(
                "would replace the 4 items of {URI} \"Kids Songs\" with 4 (1 added, 1 removed, \
                 reordered)\n  - {}\n  and 1 unavailable items\n  + {}\n",
                result_line(&offered(&t[1])),
                t[3]
            )
        );

        let mut err = Vec::new();
        let live = replace(
            "alice",
            &paths,
            URI,
            &items(&new, false),
            &mut io::empty(),
            || Ok(&mut c),
            &mut err,
        )
        .unwrap();
        put.assert();
        post.assert_calls(0);
        assert_eq!(live.snapshot_id, Some(Some("snap-1".into())));
        assert_eq!(String::from_utf8(err).unwrap(), "wrote 4 of 4\n");
    }

    #[test]
    fn replace_with_nothing_empties_the_playlist() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 2);
        contents(&server, vec![entry(TRACK), entry(OTHER)]);
        let put = server.mock(|when, then| {
            when.method(PUT)
                .path(format!("/v1/playlists/{ID}/items"))
                .json_body(json!({"uris": []}));
            then.status(200).json_body(json!({"snapshot_id": "snap-1"}));
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let none: Vec<String> = Vec::new();
        let mut err = Vec::new();
        let report = replace(
            "alice",
            &paths(tmp.path()),
            URI,
            &items(&none, false),
            &mut io::empty(),
            || Ok(&mut c),
            &mut err,
        )
        .unwrap();
        put.assert();
        assert_eq!(report.removed, [offered(TRACK), offered(OTHER)]);
        assert_eq!(report.after, 0);
        assert!(!report.reordered);
        assert!(err.is_empty());
    }

    #[test]
    fn a_long_replace_is_one_put_then_appends_and_reports_what_landed() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 0);
        contents(&server, vec![]);
        let all: Vec<String> = (0..230).map(track).collect();
        let put = server.mock(|when, then| {
            when.method(PUT)
                .path(format!("/v1/playlists/{ID}/items"))
                .json_body(json!({"uris": all[..100]}));
            then.status(200).json_body(json!({"snapshot_id": "snap-1"}));
        });
        let second = server.mock(|when, then| {
            when.method(POST).json_body(json!({"uris": all[100..200]}));
            then.status(201).json_body(json!({"snapshot_id": "snap-2"}));
        });
        let third = server.mock(|when, then| {
            when.method(POST).json_body(json!({"uris": all[200..]}));
            then.status(201).json_body(json!({"snapshot_id": "snap-3"}));
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let paths = paths(tmp.path());
        let mut err = Vec::new();
        let report = replace(
            "alice",
            &paths,
            URI,
            &items(&all, false),
            &mut io::empty(),
            || Ok(&mut c),
            &mut err,
        )
        .unwrap();
        put.assert();
        second.assert();
        third.assert();
        assert_eq!(report.snapshot_id, Some(Some("snap-3".into())));
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "wrote 100 of 230\nwrote 200 of 230\nwrote 230 of 230\n"
        );

        // The replace counts as the first chunk landed.
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 0);
        contents(&server, vec![]);
        item_writes(&server, PUT);
        server.mock(|when, then| {
            when.method(POST);
            then.status(400)
                .json_body(json!({"error": {"status": 400, "message": "Invalid id"}}));
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let err = replace(
            "alice",
            &paths,
            URI,
            &items(&all, false),
            &mut io::empty(),
            || Ok(&mut c),
            &mut Vec::new(),
        )
        .unwrap_err();
        let partial = err.downcast_ref::<PartialWrite>().expect("a partial write");
        assert_eq!(partial.done, all[..100]);
        assert_eq!(error::kind(&err), Kind::Other);
        assert!(
            format!("{err:#}").starts_with("wrote 100 of 230 ("),
            "{err:#}"
        );

        // A replace that never landed changed nothing: the plain error, no append.
        let server = MockServer::start();
        me(&server);
        details(&server, "alice-spotify", false, 0);
        contents(&server, vec![]);
        let put = server.mock(|when, then| {
            when.method(PUT).path(format!("/v1/playlists/{ID}/items"));
            then.status(503);
        });
        let append = server.mock(|when, then| {
            when.method(POST);
            then.status(201).json_body(json!({"snapshot_id": "snap-2"}));
        });
        let mut c = client(tmp.path(), &server, PLAYLIST_WRITE_SCOPES);
        let mut progress = Vec::new();
        let err = replace(
            "alice",
            &paths,
            URI,
            &items(&all, false),
            &mut io::empty(),
            || Ok(&mut c),
            &mut progress,
        )
        .unwrap_err();
        assert!(put.calls() > 0);
        append.assert_calls(0);
        assert!(err.downcast_ref::<PartialWrite>().is_none(), "{err:#}");
        assert_eq!(error::kind(&err), Kind::Unavailable);
        assert!(progress.is_empty());
    }

    #[test]
    fn a_replacement_keeps_duplicates_and_tells_a_reorder_from_a_dedupe() {
        let (a, b, c) = (track(1), track(2), track(3));
        let current = [offered(&a), offered(&a), offered(&b)];
        let dedupe = Replacement::of(&current, &[a.clone(), b.clone()]);
        assert_eq!(
            dedupe,
            Replacement {
                removed: vec![],
                added: vec![],
                reordered: false,
            }
        );
        let swapped = Replacement::of(&current, &[b.clone(), a.clone(), c.clone(), c.clone()]);
        assert!(swapped.reordered);
        assert_eq!(swapped.added, [c]);
        let dropped = Replacement::of(&current, std::slice::from_ref(&b));
        assert_eq!(dropped.removed, [offered(&a), offered(&a)]);
        assert!(!dropped.reordered);
    }
}
