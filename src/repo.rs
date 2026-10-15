//! The local data repository: its location, its path layout, and the file
//! primitives everything in it is written with — atomic replacement,
//! deterministic JSON, and append-only JSONL read back latest-wins.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::{DeserializeOwned, IgnoredAny};

use crate::error::Kind;
use crate::fail;

/// Directory name under `$XDG_DATA_HOME` for the default repository.
pub const APP_DIR: &str = "streaming-service-sync";

/// The repository's configuration file, relative to its root.
pub const CONFIG_FILE: &str = "config.toml";

/// The repository's own README, relative to its root.
pub const README_FILE: &str = "README.md";

/// Directory holding one subdirectory per profile.
pub const PROFILES_DIR: &str = "profiles";

/// The data repository rooted at a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    root: PathBuf,
}

impl Repo {
    /// The repository at `explicit` when given, else at
    /// `$XDG_DATA_HOME/streaming-service-sync`.
    pub fn locate(explicit: Option<&Path>) -> Result<Self> {
        let root = match explicit {
            Some(p) => p.to_path_buf(),
            None => match xdg_dir("XDG_DATA_HOME", ".local/share", |v| std::env::var_os(v)) {
                Some(dir) => dir.join(APP_DIR),
                None => fail!(
                    Kind::Config,
                    "cannot determine $XDG_DATA_HOME or $HOME; pass --repo DIR"
                ),
            },
        };
        Ok(Self { root })
    }

    /// The repository rooted at `root`.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join(CONFIG_FILE)
    }

    pub fn readme_path(&self) -> PathBuf {
        self.root.join(README_FILE)
    }

    pub fn profiles_dir(&self) -> PathBuf {
        self.root.join(PROFILES_DIR)
    }

    /// The paths of one profile. The name is validated, so it can never
    /// address anything outside `profiles/`.
    pub fn profile(&self, name: &str) -> Result<ProfilePaths> {
        validate_profile_name(name)?;
        Ok(ProfilePaths {
            dir: self.profiles_dir().join(name),
        })
    }

    /// Names of the profiles that have a directory under `profiles/`,
    /// sorted. Entries whose names are not valid profile names are ignored.
    pub fn profiles(&self) -> Result<Vec<String>> {
        profile_entries(&self.profiles_dir(), |entry| {
            Ok(entry.file_type()?.is_dir())
        })
    }
}

/// Names of the entries of `dir` that are valid profile names and that
/// `keep` accepts, sorted. A missing `dir` is an empty list.
pub fn profile_entries(
    dir: &Path,
    keep: impl Fn(&fs::DirEntry) -> io::Result<bool>,
) -> Result<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if validate_profile_name(&name).is_ok()
            && keep(&entry).with_context(|| format!("reading {}", entry.path().display()))?
        {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// An XDG base directory: `$var` when it is set to an absolute path, else
/// `$HOME/<home_relative>`. `None` when neither yields an absolute path.
pub fn xdg_dir(
    var: &str,
    home_relative: &str,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let absolute = |v: &str| env(v).map(PathBuf::from).filter(|p| p.is_absolute());
    absolute(var).or_else(|| absolute("HOME").map(|home| home.join(home_relative)))
}

/// Profile names are lowercase ASCII letters, digits, `-` and `_`, starting
/// with a letter or digit, at most 64 bytes.
pub fn validate_profile_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if !ok {
        fail!(
            Kind::Config,
            "invalid profile name {name:?}: use lowercase letters, digits, '-' and '_', \
             starting with a letter or digit"
        );
    }
    Ok(())
}

/// The layout of one profile's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfilePaths {
    dir: PathBuf,
}

impl ProfilePaths {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn pandora_dir(&self) -> PathBuf {
        self.dir.join("pandora")
    }

    /// Directory holding every capture run.
    pub fn raw_dir(&self) -> PathBuf {
        self.pandora_dir().join("raw")
    }

    /// Directory of one capture run's raw responses and manifest.
    pub fn raw_run_dir(&self, run_id: &str) -> PathBuf {
        self.raw_dir().join(run_id)
    }

    pub fn stations(&self) -> PathBuf {
        self.pandora_dir().join("stations.json")
    }

    pub fn playlists(&self) -> PathBuf {
        self.pandora_dir().join("playlists.json")
    }

    pub fn collection(&self) -> PathBuf {
        self.pandora_dir().join("collection.json")
    }

    pub fn bookmarks(&self) -> PathBuf {
        self.pandora_dir().join("bookmarks.json")
    }

    pub fn tracks(&self) -> PathBuf {
        self.pandora_dir().join("tracks.json")
    }

    pub fn matches_dir(&self) -> PathBuf {
        self.dir.join("matches")
    }

    pub fn track_matches(&self) -> PathBuf {
        self.matches_dir().join("tracks.jsonl")
    }

    pub fn artist_matches(&self) -> PathBuf {
        self.matches_dir().join("artists.jsonl")
    }

    pub fn album_matches(&self) -> PathBuf {
        self.matches_dir().join("albums.jsonl")
    }

    /// Audit record of one LLM batch.
    pub fn llm_session(&self, session_id: &str) -> PathBuf {
        self.matches_dir()
            .join("llm")
            .join(format!("{session_id}.json"))
    }

    pub fn spotify_state(&self) -> PathBuf {
        self.dir.join("spotify").join("state.json")
    }

    pub fn report(&self) -> PathBuf {
        self.dir.join("report.md")
    }
}

/// Replaces `path` with `contents` atomically: a reader sees the old file or
/// the new one, never a mix. Parent directories are created.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    replace_with(stage_beside(path, contents, None)?, path)
}

/// [`write_atomic`] for a file only its owner may read: the staged file is
/// mode 0600 before anything is written to it.
pub fn write_atomic_private(path: &Path, contents: &[u8]) -> Result<()> {
    replace_with(stage_beside(path, contents, Some(0o600))?, path)
}

fn replace_with(tmp: tempfile::NamedTempFile, path: &Path) -> Result<()> {
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Creates `path` with `contents` atomically unless it already exists.
/// Returns whether the file was written.
pub fn write_new_atomic(path: &Path, contents: &[u8]) -> Result<bool> {
    match stage_beside(path, contents, None)?.persist_noclobber(path) {
        Ok(_) => Ok(true),
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e.error).with_context(|| format!("creating {}", path.display())),
    }
}

/// Writes `contents` to a synced temporary file in `path`'s directory,
/// creating the directory, ready to be renamed onto `path`. With `mode`, the
/// file gets that mode before `contents` is written.
fn stage_beside(
    path: &Path,
    contents: &[u8],
    mode: Option<u32>,
) -> Result<tempfile::NamedTempFile> {
    let dir = parent_dir(path);
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    mode.map_or(Ok(()), |m| {
        tmp.as_file().set_permissions(fs::Permissions::from_mode(m))
    })
    .and_then(|()| tmp.write_all(contents))
    .and_then(|()| tmp.as_file().sync_all())
    .with_context(|| format!("writing {}", path.display()))?;
    Ok(tmp)
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// The current time, RFC 3339 UTC (`2026-10-13T12:34:56.789Z`).
pub fn now_rfc3339() -> Result<String> {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .context("formatting the current time")
}

/// The repository's JSON form of `value`: two-space indented, one trailing
/// newline. Object keys come out in declaration order for structs and in
/// sorted order for maps, so equal values always serialize to equal bytes.
pub fn to_json(value: &impl Serialize) -> Result<String> {
    let mut s = serde_json::to_string_pretty(value).context("serializing JSON")?;
    s.push('\n');
    Ok(s)
}

/// Writes `value` to `path` atomically in the repository's JSON form.
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    write_atomic(path, to_json(value)?.as_bytes())
}

/// Reads a JSON file written by [`write_json`].
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// [`read_json`], `None` when `path` does not exist.
pub fn read_json_opt<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))
        .map(Some)
}

/// Appends `value` to the JSONL file at `path` as one line. A torn tail (see
/// [`split_torn_tail`]) is an error, the file left unchanged; a complete
/// final record missing its newline gets one.
pub fn append_jsonl(path: &Path, value: &impl Serialize) -> Result<()> {
    let record = serde_json::to_string(value).context("serializing JSON")?;
    let dir = parent_dir(path);
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut line = String::with_capacity(record.len() + 2);
    if !ends_terminated(&mut file).with_context(|| format!("reading {}", path.display()))? {
        // Only a file whose last write was interrupted or that was edited by
        // hand gets here, so reading it whole is rare.
        let mut text = Vec::new();
        file.seek(SeekFrom::Start(0))
            .and_then(|_| file.read_to_end(&mut text))
            .with_context(|| format!("reading {}", path.display()))?;
        let (body, torn) = split_torn_tail(&text);
        if !torn.is_empty() {
            bail!(torn_tail_error(path, body, torn));
        }
        if !body.is_empty() && !body.ends_with(b"\n") {
            line.push('\n');
        }
    }
    line.push_str(&record);
    line.push('\n');
    file.write_all(line.as_bytes())
        .and_then(|()| file.sync_data())
        .with_context(|| format!("appending to {}", path.display()))?;
    Ok(())
}

/// Whether `file` is empty or ends in a newline, read from its last byte.
fn ends_terminated(file: &mut fs::File) -> io::Result<bool> {
    if file.metadata()?.len() == 0 {
        return Ok(true);
    }
    let mut last = [0u8];
    file.seek(SeekFrom::End(-1))?;
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

/// Splits JSONL bytes into `(body, torn)`. `torn` is a final line with no
/// terminating newline that is not valid JSON — the remains of an
/// interrupted append, never a complete record — and is empty otherwise. A
/// final unterminated line that is valid JSON is a complete record and stays
/// in `body`.
fn split_torn_tail(text: &[u8]) -> (&[u8], &[u8]) {
    let start = text.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let tail = &text[start..];
    if tail.is_empty() || serde_json::from_slice::<IgnoredAny>(tail).is_ok() {
        (text, &[])
    } else {
        text.split_at(start)
    }
}

/// The error refusing a torn tail: the file, the tail's 1-based line number
/// and its bytes, quoted in full so a hand-edited line cut by mistake can be
/// restored.
fn torn_tail_error(path: &Path, body: &[u8], torn: &[u8]) -> String {
    let line = body.iter().filter(|&&b| b == b'\n').count() + 1;
    format!(
        "{} line {line}: the final line is incomplete ({} bytes, not valid JSON): {:?}; \
         remove it by hand, then re-run",
        path.display(),
        torn.len(),
        String::from_utf8_lossy(torn)
    )
}

/// Reads every record of the JSONL file at `path`, in file order. A missing
/// file has no records. Blank lines are skipped; a torn tail (see
/// [`split_torn_tail`]) or any other unparseable line is an error naming its
/// line number.
pub fn read_jsonl<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let (body, torn) = split_torn_tail(&bytes);
    if !torn.is_empty() {
        bail!(torn_tail_error(path, body, torn));
    }
    let text = std::str::from_utf8(body).with_context(|| format!("reading {}", path.display()))?;
    let mut records = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record = serde_json::from_str(line)
            .with_context(|| format!("parsing {} line {}", path.display(), i + 1))?;
        records.push(record);
    }
    Ok(records)
}

/// Reads the JSONL file at `path` and keeps, for each key, the last record
/// carrying it.
pub fn read_jsonl_latest<T, K, F>(path: &Path, key: F) -> Result<BTreeMap<K, T>>
where
    T: DeserializeOwned,
    K: Ord,
    F: Fn(&T) -> K,
{
    let mut latest = BTreeMap::new();
    for record in read_jsonl::<T>(path)? {
        latest.insert(key(&record), record);
    }
    Ok(latest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::collections::BTreeMap;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Entry {
        pandora_id: String,
        status: String,
    }

    fn entry(id: &str, status: &str) -> Entry {
        Entry {
            pandora_id: id.into(),
            status: status.into(),
        }
    }

    #[test]
    fn profile_names() {
        for good in ["alice", "bob-2", "a", "x_y", "0day"] {
            validate_profile_name(good).unwrap_or_else(|e| panic!("{good}: {e}"));
        }
        let long = "a".repeat(65);
        for bad in [
            "",
            "Alice",
            "../x",
            "a/b",
            "-a",
            "_a",
            ".",
            "a b",
            long.as_str(),
        ] {
            assert!(validate_profile_name(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn profile_paths_follow_the_layout() {
        let repo = Repo::at("/r");
        let p = repo.profile("alice").unwrap();
        assert_eq!(p.dir(), Path::new("/r/profiles/alice"));
        assert_eq!(
            p.tracks(),
            Path::new("/r/profiles/alice/pandora/tracks.json")
        );
        assert_eq!(
            p.raw_run_dir("run1"),
            Path::new("/r/profiles/alice/pandora/raw/run1")
        );
        assert_eq!(
            p.track_matches(),
            Path::new("/r/profiles/alice/matches/tracks.jsonl")
        );
        assert_eq!(
            p.llm_session("s1"),
            Path::new("/r/profiles/alice/matches/llm/s1.json")
        );
        assert_eq!(
            p.spotify_state(),
            Path::new("/r/profiles/alice/spotify/state.json")
        );
        assert_eq!(p.report(), Path::new("/r/profiles/alice/report.md"));
        assert!(repo.profile("../etc").is_err());
    }

    #[test]
    fn xdg_dir_prefers_an_absolute_variable_then_home() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |v: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == v)
                    .map(|(_, val)| std::ffi::OsString::from(val))
            }
        };
        let both = env(&[("XDG_DATA_HOME", "/xdg"), ("HOME", "/home/u")]);
        assert_eq!(
            xdg_dir("XDG_DATA_HOME", ".local/share", both),
            Some(PathBuf::from("/xdg"))
        );
        let relative = env(&[("XDG_DATA_HOME", "rel"), ("HOME", "/home/u")]);
        assert_eq!(
            xdg_dir("XDG_DATA_HOME", ".local/share", relative),
            Some(PathBuf::from("/home/u/.local/share"))
        );
        let home_only = env(&[("HOME", "/home/u")]);
        assert_eq!(
            xdg_dir("XDG_STATE_HOME", ".local/state", home_only),
            Some(PathBuf::from("/home/u/.local/state"))
        );
        assert_eq!(xdg_dir("XDG_DATA_HOME", ".local/share", env(&[])), None);
        assert_eq!(
            xdg_dir("XDG_DATA_HOME", ".local/share", env(&[("HOME", "")])),
            None
        );
    }

    #[test]
    fn explicit_repo_wins() {
        let repo = Repo::locate(Some(Path::new("/some/dir"))).unwrap();
        assert_eq!(repo.root(), Path::new("/some/dir"));
        assert_eq!(repo.config_path(), Path::new("/some/dir/config.toml"));
    }

    #[test]
    fn profiles_lists_valid_directories_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::at(tmp.path());
        assert!(repo.profiles().unwrap().is_empty());
        for d in ["carol", "alice", "Not-Valid", ".hidden"] {
            fs::create_dir_all(repo.profiles_dir().join(d)).unwrap();
        }
        fs::write(repo.profiles_dir().join("bob"), "a file").unwrap();
        assert_eq!(repo.profiles().unwrap(), ["alice", "carol"]);
    }

    #[test]
    fn write_atomic_creates_and_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a/b/file.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["file.json"]);
    }

    #[test]
    fn write_new_atomic_never_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f");
        assert!(write_new_atomic(&path, b"first").unwrap());
        assert!(!write_new_atomic(&path, b"second").unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"first");
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn json_is_deterministic_and_round_trips() {
        let mut a = BTreeMap::new();
        a.insert("zeta", 1);
        a.insert("alpha", 2);
        let mut b = BTreeMap::new();
        b.insert("alpha", 2);
        b.insert("zeta", 1);
        let text = to_json(&a).unwrap();
        assert_eq!(text, to_json(&b).unwrap());
        assert_eq!(text, "{\n  \"alpha\": 2,\n  \"zeta\": 1\n}\n");

        // Arbitrary JSON values come out with sorted keys whatever the input order.
        let v: serde_json::Value = serde_json::from_str(r#"{"b":1,"a":{"d":2,"c":3}}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"a":{"c":3,"d":2},"b":1}"#
        );

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("e.json");
        let e = entry("TR:1", "matched");
        write_json(&path, &e).unwrap();
        assert_eq!(read_json::<Entry>(&path).unwrap(), e);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\n  \"pandora_id\": \"TR:1\",\n  \"status\": \"matched\"\n}\n"
        );
    }

    #[test]
    fn optional_json_is_none_only_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("e.json");
        assert_eq!(read_json_opt::<Entry>(&path).unwrap(), None);
        write_json(&path, &entry("TR:1", "matched")).unwrap();
        assert_eq!(
            read_json_opt(&path).unwrap(),
            Some(entry("TR:1", "matched"))
        );
        fs::write(&path, "{").unwrap();
        let err = read_json_opt::<Entry>(&path).unwrap_err();
        assert!(format!("{err:#}").starts_with("parsing "), "{err:#}");
    }

    #[test]
    fn private_atomic_write_is_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("d").join("secret");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic_private(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn jsonl_latest_entry_per_key_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("m/tracks.jsonl");
        assert!(read_jsonl::<Entry>(&path).unwrap().is_empty());
        append_jsonl(&path, &entry("TR:1", "ambiguous")).unwrap();
        append_jsonl(&path, &entry("TR:2", "unmatched")).unwrap();
        append_jsonl(&path, &entry("TR:1", "matched")).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"pandora_id\":\"TR:1\",\"status\":\"ambiguous\"}\n\
             {\"pandora_id\":\"TR:2\",\"status\":\"unmatched\"}\n\
             {\"pandora_id\":\"TR:1\",\"status\":\"matched\"}\n"
        );
        assert_eq!(read_jsonl::<Entry>(&path).unwrap().len(), 3);
        let latest = read_jsonl_latest(&path, |e: &Entry| e.pandora_id.clone()).unwrap();
        assert_eq!(latest.len(), 2);
        assert_eq!(latest["TR:1"].status, "matched");
        assert_eq!(latest["TR:2"].status, "unmatched");
    }

    /// Asserts that reading and appending both refuse `path`'s torn tail
    /// `torn` on line `line`, and that the file is left as it was.
    fn assert_torn_tail_refused(path: &Path, line: usize, torn: &[u8]) {
        let before = fs::read(path).unwrap();
        let expected = format!(
            "{} line {line}: the final line is incomplete ({} bytes, not valid JSON): {:?}; \
             remove it by hand, then re-run",
            path.display(),
            torn.len(),
            String::from_utf8_lossy(torn)
        );
        let err = read_jsonl::<Entry>(path).unwrap_err();
        assert_eq!(format!("{err:#}"), expected);
        assert_eq!(crate::error::kind(&err), Kind::Other);
        let err = append_jsonl(path, &entry("TR:2", "matched")).unwrap_err();
        assert_eq!(format!("{err:#}"), expected);
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn jsonl_torn_tail_is_refused_on_read_and_append() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        fs::write(
            &path,
            "{\"pandora_id\":\"TR:1\",\"status\":\"matched\"}\n{\"pandora_id\":\"TR:",
        )
        .unwrap();
        assert_torn_tail_refused(&path, 2, b"{\"pandora_id\":\"TR:");
    }

    #[test]
    fn jsonl_complete_unterminated_tail_is_kept_on_append() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        fs::write(&path, "{\"pandora_id\":\"TR:1\",\"status\":\"manual\"}").unwrap();
        assert_eq!(
            read_jsonl::<Entry>(&path).unwrap(),
            [entry("TR:1", "manual")]
        );
        append_jsonl(&path, &entry("TR:2", "matched")).unwrap();
        assert_eq!(
            read_jsonl::<Entry>(&path).unwrap(),
            [entry("TR:1", "manual"), entry("TR:2", "matched")]
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"pandora_id\":\"TR:1\",\"status\":\"manual\"}\n\
             {\"pandora_id\":\"TR:2\",\"status\":\"matched\"}\n"
        );
    }

    #[test]
    fn jsonl_torn_tail_cut_mid_character_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let mut bytes = b"{\"pandora_id\":\"TR:1\",\"status\":\"matched\"}\n\n{\"x\":\"".to_vec();
        bytes.push("é".as_bytes()[0]);
        fs::write(&path, &bytes).unwrap();
        assert_torn_tail_refused(&path, 3, &bytes[bytes.len() - 7..]);
    }

    #[test]
    fn jsonl_unterminated_valid_json_of_wrong_shape_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        fs::write(
            &path,
            "{\"pandora_id\":\"TR:1\",\"status\":\"matched\"}\n{\"other\":1}",
        )
        .unwrap();
        let err = format!("{:#}", read_jsonl::<Entry>(&path).unwrap_err());
        assert!(err.contains("line 2"), "{err}");
    }

    #[test]
    fn jsonl_hand_edited_unterminated_tail_is_quoted_in_the_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let typo = "{\"pandora_id\":\"TR:2\",\"status\":\"manual\",}";
        fs::write(
            &path,
            format!("{{\"pandora_id\":\"TR:1\",\"status\":\"matched\"}}\n{typo}"),
        )
        .unwrap();
        let err = format!("{:#}", read_jsonl::<Entry>(&path).unwrap_err());
        assert!(err.contains(&format!("{typo:?}")), "{err}");
        assert_torn_tail_refused(&path, 2, typo.as_bytes());
    }

    #[test]
    fn jsonl_corrupt_inner_line_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        fs::write(
            &path,
            "{\"pandora_id\":\"TR:1\",\"status\":\"matched\"}\nnot json\n\n",
        )
        .unwrap();
        let err = format!("{:#}", read_jsonl::<Entry>(&path).unwrap_err());
        assert!(err.contains("line 2"), "{err}");
        // A terminated final line is a complete record and must parse.
        fs::write(&path, "not json\n").unwrap();
        assert!(read_jsonl::<Entry>(&path).is_err());
    }
}
