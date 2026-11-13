//! banto's own data directory (`<data_local>/banto`) and everything banto keeps
//! in it: the database, the per-member MCP configs and the Goinkyo's consultation
//! requests under `mcp/`, and the opt-in message logs under `logs/`.
//!
//! Every path under it is derived here, from one [`DataDir`], so that the banto
//! that decides where the directory is (`banto::startup`) and every banto it
//! starts can use the same one.
//!
//! The per-member configs and the consultation requests are working files, and
//! [`sweep`] deletes the ones nothing needs any more. It is deliberately timid. A
//! file is deleted only when **both** hold, each checked on its own:
//!
//! - the store has no member that owns it ([`Facts`]); and
//! - the file itself was last written at least [`GRACE`] ago, read just before the
//!   deletion ([`is_garbage`] is the pure form of this pair).
//!
//! Only names banto writes are ever candidates (see [`parse_name`]), only in
//! `mcp/` and `mcp/goinkyo/`, and only if those two are real directories. The
//! message logs, the database and anything else are never touched.
//!
//! What this does **not** protect against, stated so nobody has to find it out:
//!
//! - a wall clock that jumps forward by more than [`GRACE`], or a file restored
//!   with an old modification time: either looks "old" to the age test;
//! - a process suspended for longer than [`GRACE`] between writing a consultation
//!   request and registering the Goinkyo it belongs to (the request is written
//!   first, so that a row always has a file to read);
//! - the instant between the last look at a file and its deletion — nothing here
//!   can delete "only if unchanged";
//! - another banto using a *different* database in the same data directory: the
//!   caller must only sweep a data directory whose own database it has open
//!   (`DataDir::is_database`), because the store is the whole of the authority.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use banto_core::model::{BrigadeId, DIRECTOR_TOKEN, GOINKYO_TOKEN};

use crate::store::MAX_WORKER_NUMBER;

/// How old a working file must be before it can be swept, whatever the store says.
///
/// This is what closes the two windows the store cannot: a consultation request
/// exists *before* its Goinkyo row, and a file whose name is reused (the
/// Goinkyo's token is fixed) can be rewritten between the sweep's reading of the
/// store and its deleting. banto rewrites a member's config every time it opens
/// the member, so a file about to be used is always younger than this.
pub const GRACE: Duration = Duration::from_secs(30 * 60);

/// The directory itself. Absolute by the time anything holds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir(PathBuf);

impl DataDir {
    pub fn new(root: PathBuf) -> Self {
        Self(root)
    }

    /// The platform's location, `dirs::data_local_dir()/banto`.
    pub fn default_root() -> Option<PathBuf> {
        dirs::data_local_dir().map(|dir| dir.join("banto"))
    }

    pub fn root(&self) -> &Path {
        &self.0
    }

    /// `<root>/banto.db`, the default database.
    pub fn db_path(&self) -> PathBuf {
        self.0.join("banto.db")
    }

    /// Whether `db` is this directory's own database: the same absolute path as
    /// [`Self::db_path`]. Compared as written, not canonicalised, so two spellings
    /// of one file count as different — the safe direction for the one caller
    /// (the sweep), which only acts on a yes.
    pub fn is_database(&self, db: &Path) -> bool {
        self.0.is_absolute() && db.is_absolute() && db == self.db_path()
    }

    /// `<root>/mcp`: the per-member MCP config files.
    pub fn mcp_dir(&self) -> PathBuf {
        self.0.join("mcp")
    }

    /// `<root>/mcp/<brigade>-<token>.json`, for a member banto can have issued:
    /// `None` for any other token or brigade (see [`parse_name`]), so that nothing
    /// is ever written under a name the sweep would not recognise.
    pub fn member_config_path(&self, brigade: BrigadeId, token: &str) -> Option<PathBuf> {
        Some(self.mcp_dir().join(config_name(brigade, token)?))
    }

    /// `<root>/mcp/goinkyo`: the Goinkyo's consultation requests.
    pub fn goinkyo_dir(&self) -> PathBuf {
        self.mcp_dir().join("goinkyo")
    }

    /// `<root>/mcp/goinkyo/<brigade>.txt`.
    pub fn goinkyo_request_path(&self, brigade: BrigadeId) -> Option<PathBuf> {
        Self::request_path_in(&self.goinkyo_dir(), brigade)
    }

    /// `<dir>/<brigade>.txt` for a consultation-request directory `dir` — the one
    /// place that file name is written, for a caller that holds only the
    /// directory (the MCP server). `None` for a brigade id banto does not issue.
    pub fn request_path_in(dir: &Path, brigade: BrigadeId) -> Option<PathBuf> {
        Some(dir.join(request_name(brigade)?))
    }

    /// `<root>/logs`: the opt-in message logs (never swept).
    pub fn logs_dir(&self) -> PathBuf {
        self.0.join("logs")
    }
}

// --- the names banto writes --------------------------------------------------
//
// One family: the writers (`config_name`, `request_name`) are the only code that
// spells a name, and the parser accepts a name exactly when the writer would have
// written it from what it parsed. They cannot drift apart because the parser has
// no grammar of its own — it splits, then asks the writer.

/// A brigade id the store can have issued: its ids are `AUTOINCREMENT`, from 1.
fn is_issued_brigade(brigade: BrigadeId) -> bool {
    brigade >= 1
}

/// The number in a Worker token the store issued (`worker-3`): decimal digits,
/// no leading zero, no more than the store hands out.
fn issued_worker_number(token: &str) -> Option<i64> {
    let digits = token.strip_prefix("worker-")?;
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number: i64 = digits.parse().ok()?;
    (number <= MAX_WORKER_NUMBER).then_some(number)
}

/// A member token banto issues: the Director's, the Goinkyo's, or a numbered
/// Worker's. Any other token (a hand-edited store can hold anything) gets no file.
fn is_issued_token(token: &str) -> bool {
    token == DIRECTOR_TOKEN || token == GOINKYO_TOKEN || issued_worker_number(token).is_some()
}

fn config_name(brigade: BrigadeId, token: &str) -> Option<String> {
    (is_issued_brigade(brigade) && is_issued_token(token))
        .then(|| format!("{brigade}-{token}.json"))
}

fn request_name(brigade: BrigadeId) -> Option<String> {
    is_issued_brigade(brigade).then(|| format!("{brigade}.txt"))
}

/// A file banto writes under `mcp/`, as its name says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedFile {
    /// `mcp/<brigade>-<token>.json`.
    MemberConfig { brigade: BrigadeId, token: String },
    /// `mcp/goinkyo/<brigade>.txt`.
    GoinkyoRequest { brigade: BrigadeId },
}

/// Which directory a name was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Where {
    Mcp,
    Goinkyo,
}

/// Whether `name`, found in `place`, is exactly a name banto writes there. A name
/// that is merely close — another extension or case, a leading zero, a token
/// banto does not issue — is not.
pub fn parse_name(place: Where, name: &str) -> Option<OwnedFile> {
    let parsed = match place {
        Where::Mcp => {
            let (brigade, token) = name.strip_suffix(".json")?.split_once('-')?;
            OwnedFile::MemberConfig {
                brigade: brigade.parse().ok()?,
                token: token.to_string(),
            }
        }
        Where::Goinkyo => OwnedFile::GoinkyoRequest {
            brigade: name.strip_suffix(".txt")?.parse().ok()?,
        },
    };
    // The writer decides: `+7`, `007` and `Worker-1` parse above and are refused here.
    let written = match &parsed {
        OwnedFile::MemberConfig { brigade, token } => config_name(*brigade, token)?,
        OwnedFile::GoinkyoRequest { brigade } => request_name(*brigade)?,
    };
    (written == name).then_some(parsed)
}

// --- what the store says -------------------------------------------------------

/// What the store says is alive: the `(brigade, token)` of every member row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    members: HashSet<(BrigadeId, String)>,
}

impl Facts {
    /// From every member row of the store, whatever else is in the row: only the
    /// bare pair is compared, so a row this banto does not fully understand still
    /// owns its files.
    pub fn from_member_rows(rows: impl IntoIterator<Item = (BrigadeId, String)>) -> Self {
        Self {
            members: rows.into_iter().collect(),
        }
    }

    /// Whether the store has a member that needs `file`: the member itself for its
    /// config, the brigade's Goinkyo for a consultation request.
    fn owns(&self, file: &OwnedFile) -> bool {
        let (brigade, token) = match file {
            OwnedFile::MemberConfig { brigade, token } => (*brigade, token.as_str()),
            OwnedFile::GoinkyoRequest { brigade } => (*brigade, GOINKYO_TOKEN),
        };
        self.members.contains(&(brigade, token.to_string()))
    }
}

/// Whether a file last written `age` ago has been left alone long enough. `None`
/// — an unreadable time, a time in the future, a file that is not a regular
/// file — is not old.
fn is_old(age: Option<Duration>) -> bool {
    age.is_some_and(|age| age >= GRACE)
}

/// Whether the file has outlived what it belonged to: the store has no member
/// that owns it **and** it is old (see [`GRACE`]). Pure.
pub fn is_garbage(file: &OwnedFile, age: Option<Duration>, facts: &Facts) -> bool {
    !facts.owns(file) && is_old(age)
}

// --- the sweep ---------------------------------------------------------------

/// What one sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub deleted: Vec<PathBuf>,
    /// Files that were garbage and could not be deleted (in use, no permission,
    /// already gone). Left for the next sweep; not an error.
    pub failed: usize,
}

/// Whether `path` is a directory that is not a link of any kind. A symbolic
/// link, a junction or any other reparse point is not: deleting "inside" one
/// would delete in whatever it points to.
fn is_real_directory(path: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_dir() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

/// The banto-named entries of `dir`, by name only (their age is read later, just
/// before any deletion). Nothing at all when `dir` is not a real directory.
fn list(dir: &Path, place: Where) -> Vec<(PathBuf, OwnedFile)> {
    if !is_real_directory(dir) {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            Some((entry.path(), parse_name(place, &name)?))
        })
        .collect()
}

/// How long ago `path` was last written, if it is a regular file (not a link, not
/// a directory) now and its time is in the past.
fn age_of(path: &Path, now: SystemTime) -> Option<Duration> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    now.duration_since(meta.modified().ok()?).ok()
}

/// Delete the files under `data_dir/mcp` that no member owns any more and that
/// have not been written for [`GRACE`].
///
/// The order is the protection:
///
/// 1. list the names (and require `mcp/` and `mcp/goinkyo/` to be real
///    directories);
/// 2. only then ask the store (`read_facts`): banto records a member in the store
///    before it writes that member's config, so a config already in the listing
///    has its member visible to a read that comes after it;
/// 3. for each file the store does not own, read its age **now** and delete it
///    only if it is regular and old. A name reused between steps 1 and 3 (the
///    Goinkyo's, say, rewritten for a new consultation) is fresh by then.
///
/// `read_facts` returning `None` (the store could not be read) deletes nothing. A
/// deletion that fails is counted and forgotten: nothing is reported, nothing
/// stops, and the file is simply a candidate again at the next sweep.
pub fn sweep(
    data_dir: &DataDir,
    now: SystemTime,
    read_facts: impl FnOnce() -> Option<Facts>,
) -> SweepReport {
    let mut listed = list(&data_dir.mcp_dir(), Where::Mcp);
    listed.extend(list(&data_dir.goinkyo_dir(), Where::Goinkyo));
    let mut report = SweepReport::default();
    if listed.is_empty() {
        return report;
    }
    let Some(facts) = read_facts() else {
        return report;
    };
    for (path, file) in listed {
        if !is_garbage(&file, age_of(&path, now), &facts) {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => report.deleted.push(path),
            Err(_) => report.failed += 1,
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(rows: &[(BrigadeId, &str)]) -> Facts {
        Facts::from_member_rows(
            rows.iter()
                .map(|(brigade, token)| (*brigade, token.to_string())),
        )
    }

    const OLD: Option<Duration> = Some(Duration::from_secs(24 * 3600));

    fn config(brigade: BrigadeId, token: &str) -> OwnedFile {
        OwnedFile::MemberConfig {
            brigade,
            token: token.to_string(),
        }
    }

    #[test]
    fn every_path_hangs_off_the_one_directory() {
        let dir = DataDir::new(PathBuf::from("/data/banto"));
        assert_eq!(dir.db_path(), PathBuf::from("/data/banto/banto.db"));
        assert_eq!(dir.mcp_dir(), PathBuf::from("/data/banto/mcp"));
        assert_eq!(
            dir.member_config_path(7, "worker-1"),
            Some(PathBuf::from("/data/banto/mcp/7-worker-1.json"))
        );
        assert_eq!(
            dir.goinkyo_request_path(7),
            Some(PathBuf::from("/data/banto/mcp/goinkyo/7.txt"))
        );
        assert_eq!(dir.logs_dir(), PathBuf::from("/data/banto/logs"));
    }

    #[test]
    fn a_database_is_the_directorys_own_only_as_the_same_absolute_path() {
        let base = std::env::temp_dir().join("data");
        let dir = DataDir::new(base.join("banto"));
        assert!(dir.is_database(&base.join("banto").join("banto.db")));
        assert!(!dir.is_database(&base.join("banto").join("other.db")));
        assert!(!dir.is_database(&base.join("elsewhere").join("banto.db")));
        assert!(!dir.is_database(&base.join("banto").join("..").join("banto").join("banto.db")));
        assert!(!dir.is_database(Path::new("banto.db")));
        // A directory that is not absolute owns nothing.
        let relative = DataDir::new(PathBuf::from("banto"));
        assert!(!relative.is_database(Path::new("banto/banto.db")));
    }

    #[test]
    fn what_the_writer_writes_the_parser_reads_back_and_only_that() {
        let dir = DataDir::new(PathBuf::from("/d"));
        for (brigade, token) in [
            (1, "director"),
            (19, "director"),
            (3, "goinkyo"),
            (7, "worker-1"),
            (7, "worker-12"),
            (i64::MAX, "worker-999999999"),
        ] {
            let path = dir.member_config_path(brigade, token).unwrap();
            let name = path.file_name().unwrap().to_str().unwrap();
            assert_eq!(
                parse_name(Where::Mcp, name),
                Some(config(brigade, token)),
                "{name}"
            );
        }
        let request = dir.goinkyo_request_path(12).unwrap();
        assert_eq!(
            parse_name(
                Where::Goinkyo,
                request.file_name().unwrap().to_str().unwrap()
            ),
            Some(OwnedFile::GoinkyoRequest { brigade: 12 })
        );
    }

    #[test]
    fn the_writer_refuses_what_the_parser_would_not_recognise() {
        let dir = DataDir::new(PathBuf::from("/d"));
        for token in [
            "",
            "evil",
            "Worker-1",
            "worker-",
            "worker-0",
            "worker-01",
            "worker-+1",
            "worker-1 ",
            "worker-1000000000",
            "worker-1/../x",
            "we ird/tok",
            "director2",
            "Goinkyo",
            "worker-1.json",
        ] {
            assert_eq!(dir.member_config_path(7, token), None, "{token:?}");
        }
        for brigade in [0, -1, i64::MIN] {
            assert_eq!(dir.member_config_path(brigade, "director"), None);
            assert_eq!(dir.goinkyo_request_path(brigade), None);
        }
    }

    #[test]
    fn near_misses_are_not_banto_names() {
        for name in [
            "7-evil.json",
            "07-worker-1.json",
            "7-worker-01.json",
            "7-Worker-1.json",
            "7-worker-1.JSON",
            "7-worker-0.json",
            "7-worker-1.json.bak",
            "7-worker-1.txt",
            "+7-worker-1.json",
            "-7-worker-1.json",
            "-worker-1.json",
            "7-.json",
            "7.json",
            "7-director.json.old",
            "7-Director.json",
            "7-goinkyo2.json",
            "x-worker-1.json",
            "7-wor ker.json",
            "7-wörker.json",
            "notes.json",
            ".json",
            "7-worker-1",
            "7-worker-1000000000.json",
            "99999999999999999999999-worker-1.json",
        ] {
            assert_eq!(parse_name(Where::Mcp, name), None, "{name}");
        }
        for name in [
            "07.txt",
            "7.TXT",
            "7.txt.old",
            "x.txt",
            "7-1.txt",
            ".txt",
            "7",
            "7.json",
            "0.txt",
            "-7.txt",
            "+7.txt",
        ] {
            assert_eq!(parse_name(Where::Goinkyo, name), None, "{name}");
        }
    }

    #[test]
    fn a_member_config_is_owned_by_exactly_its_member_row() {
        let facts = facts(&[(7, "director"), (7, "worker-2")]);
        assert!(!is_garbage(&config(7, "director"), OLD, &facts));
        assert!(!is_garbage(&config(7, "worker-2"), OLD, &facts));
        // A dismissed member (the token is never reused), another brigade's
        // member, a brigade the store no longer has.
        assert!(is_garbage(&config(7, "worker-1"), OLD, &facts));
        assert!(is_garbage(&config(8, "director"), OLD, &facts));
    }

    #[test]
    fn a_request_is_owned_by_its_brigades_goinkyo_whatever_that_rows_role_says() {
        let request = OwnedFile::GoinkyoRequest { brigade: 7 };
        assert!(!is_garbage(&request, OLD, &facts(&[(7, "goinkyo")])));
        assert!(is_garbage(&request, OLD, &facts(&[(7, "director")])));
        // Another brigade's Goinkyo is not this brigade's.
        assert!(is_garbage(&request, OLD, &facts(&[(8, "goinkyo")])));
    }

    #[test]
    fn nothing_is_garbage_until_it_is_old_enough_whatever_the_store_says() {
        let nobody = facts(&[]);
        for file in [
            config(7, "worker-1"),
            OwnedFile::GoinkyoRequest { brigade: 7 },
        ] {
            assert!(!is_garbage(&file, Some(Duration::ZERO), &nobody));
            assert!(!is_garbage(
                &file,
                Some(GRACE - Duration::from_secs(1)),
                &nobody
            ));
            assert!(is_garbage(&file, Some(GRACE), &nobody));
            assert!(is_garbage(&file, OLD, &nobody));
            // An age that cannot be known is not "old".
            assert!(!is_garbage(&file, None, &nobody));
        }
    }

    // --- the sweep, against a real directory -----------------------------------

    use tempfile::TempDir;

    fn touch(path: &Path, age: Duration) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = fs::File::create(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut found: Vec<String> = fs::read_dir(dir)
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default();
        found.sort();
        found
    }

    fn world() -> (TempDir, DataDir) {
        let tmp = TempDir::new().unwrap();
        let dir = DataDir::new(tmp.path().to_path_buf());
        (tmp, dir)
    }

    fn cfg(dir: &DataDir, brigade: BrigadeId, token: &str) -> PathBuf {
        dir.member_config_path(brigade, token).unwrap()
    }

    fn request(dir: &DataDir, brigade: BrigadeId) -> PathBuf {
        dir.goinkyo_request_path(brigade).unwrap()
    }

    const A_DAY: Duration = Duration::from_secs(24 * 3600);

    #[test]
    fn a_sweep_deletes_old_files_no_member_owns_and_keeps_everything_else() {
        let (_tmp, dir) = world();
        // Owned: a Director, a Worker, the Goinkyo and its request.
        for token in ["director", "worker-2", "goinkyo"] {
            touch(&cfg(&dir, 7, token), A_DAY);
        }
        touch(&request(&dir, 7), A_DAY);
        // Old orphans: a dismissed Worker, a whole brigade that is gone, its request.
        touch(&cfg(&dir, 7, "worker-1"), A_DAY);
        touch(&cfg(&dir, 3, "director"), A_DAY);
        touch(&cfg(&dir, 3, "worker-1"), A_DAY);
        touch(&request(&dir, 3), A_DAY);
        // Not banto's: foreign files, near misses, a message log, the database.
        touch(&dir.mcp_dir().join("notes.txt"), A_DAY);
        touch(&dir.mcp_dir().join("7-evil.json"), A_DAY);
        touch(&dir.mcp_dir().join("07-worker-1.json"), A_DAY);
        touch(&dir.mcp_dir().join("7-worker-01.json"), A_DAY);
        // Case variants of a name that is not otherwise present (on a
        // case-insensitive filesystem they would be the same file as one that is).
        touch(&dir.mcp_dir().join("9-Worker-1.json"), A_DAY);
        touch(&dir.mcp_dir().join("8-worker-1.JSON"), A_DAY);
        touch(&dir.goinkyo_dir().join("keep.me"), A_DAY);
        touch(&dir.goinkyo_dir().join("9.txt.old"), A_DAY);
        touch(&dir.logs_dir().join("brigade-3.log"), A_DAY);
        touch(&dir.db_path(), A_DAY);

        let report = sweep(&dir, SystemTime::now(), || {
            Some(facts(&[(7, "director"), (7, "worker-2"), (7, "goinkyo")]))
        });

        assert_eq!(report.deleted.len(), 4, "{report:?}");
        assert_eq!(report.failed, 0);
        assert_eq!(
            names(&dir.mcp_dir()),
            [
                "07-worker-1.json",
                "7-director.json",
                "7-evil.json",
                "7-goinkyo.json",
                "7-worker-01.json",
                "7-worker-2.json",
                "8-worker-1.JSON",
                "9-Worker-1.json",
                "goinkyo",
                "notes.txt"
            ]
        );
        assert_eq!(names(&dir.goinkyo_dir()), ["7.txt", "9.txt.old", "keep.me"]);
        assert_eq!(names(&dir.logs_dir()), ["brigade-3.log"]);
        assert!(dir.db_path().exists());
    }

    #[test]
    fn an_old_orphan_with_no_row_is_removed() {
        let (_tmp, dir) = world();
        touch(&cfg(&dir, 7, "worker-1"), GRACE);
        touch(&request(&dir, 7), GRACE);
        let report = sweep(&dir, SystemTime::now(), || Some(facts(&[])));
        assert_eq!(report.deleted.len(), 2, "{report:?}");
        assert!(!cfg(&dir, 7, "worker-1").exists());
        assert!(!request(&dir, 7).exists());
    }

    #[test]
    fn a_file_written_inside_the_grace_survives_with_no_row() {
        // Both kinds: a request is written before its row, and a config's name
        // can be reused by a member that is about to start.
        let (_tmp, dir) = world();
        let young = GRACE - Duration::from_secs(60);
        touch(&cfg(&dir, 7, "worker-1"), young);
        touch(&request(&dir, 7), young);
        touch(&cfg(&dir, 8, "goinkyo"), Duration::ZERO);
        let report = sweep(&dir, SystemTime::now(), || Some(facts(&[])));
        assert_eq!(report, SweepReport::default());
        assert!(cfg(&dir, 7, "worker-1").exists());
        assert!(request(&dir, 7).exists());
        assert!(cfg(&dir, 8, "goinkyo").exists());
    }

    #[test]
    fn a_file_dated_in_the_future_is_not_old() {
        let (_tmp, dir) = world();
        let path = cfg(&dir, 7, "worker-1");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = fs::File::create(&path).unwrap();
        file.set_modified(SystemTime::now() + Duration::from_secs(3 * 3600))
            .unwrap();
        drop(file);
        let report = sweep(&dir, SystemTime::now(), || Some(facts(&[])));
        assert_eq!(report, SweepReport::default());
        assert!(path.exists());
    }

    #[test]
    fn a_stale_name_rewritten_after_the_listing_is_not_swept() {
        // The interleaving: a Goinkyo's config from a dismissed consultation is in
        // the listing (the token, so the name, is fixed); the store is read and has
        // no Goinkyo; before the deletion another process registers a new one and
        // rewrites the file for a Claude that is starting. Listing and store alone
        // would delete it. Its age, read at the moment of deletion, does not.
        let (_tmp, dir) = world();
        let stale_config = cfg(&dir, 7, "goinkyo");
        let stale_request = request(&dir, 7);
        touch(&stale_config, A_DAY);
        touch(&stale_request, A_DAY);
        let report = sweep(&dir, SystemTime::now(), || {
            // The facts are as they were before the new row: nobody owns either file.
            touch(&stale_config, Duration::ZERO);
            touch(&stale_request, Duration::ZERO);
            Some(facts(&[]))
        });
        assert_eq!(report, SweepReport::default());
        assert!(stale_config.exists());
        assert!(stale_request.exists());
    }

    #[test]
    fn a_config_that_appears_while_the_store_is_read_is_never_swept() {
        // Not in the listing at all: the listing comes first.
        let (_tmp, dir) = world();
        touch(&cfg(&dir, 7, "worker-1"), A_DAY);
        let racing = cfg(&dir, 7, "worker-2");
        let report = sweep(&dir, SystemTime::now(), || {
            touch(&racing, A_DAY);
            Some(facts(&[(7, "director")]))
        });
        assert_eq!(report.deleted, [cfg(&dir, 7, "worker-1")]);
        assert!(racing.exists());
    }

    #[test]
    fn nothing_is_deleted_when_the_store_cannot_be_read() {
        let (_tmp, dir) = world();
        touch(&cfg(&dir, 7, "worker-1"), A_DAY);
        touch(&request(&dir, 7), 7 * A_DAY);
        let report = sweep(&dir, SystemTime::now(), || None);
        assert_eq!(report, SweepReport::default());
        assert!(cfg(&dir, 7, "worker-1").exists());
        assert!(request(&dir, 7).exists());
    }

    #[test]
    fn a_file_that_vanishes_before_its_deletion_is_skipped_and_the_sweep_goes_on() {
        let (_tmp, dir) = world();
        let vanishing = cfg(&dir, 7, "worker-1");
        touch(&vanishing, A_DAY);
        touch(&cfg(&dir, 7, "worker-3"), A_DAY);
        let report = sweep(&dir, SystemTime::now(), || {
            fs::remove_file(&vanishing).unwrap();
            Some(facts(&[(7, "director")]))
        });
        assert_eq!(report.deleted, [cfg(&dir, 7, "worker-3")]);
        assert_eq!(report.failed, 0);
    }

    /// Held open without delete sharing, as another program (a scanner, an editor)
    /// can: the deletion fails, is counted, and the next file is still swept.
    #[cfg(windows)]
    #[test]
    fn a_file_that_cannot_be_deleted_is_counted_and_the_sweep_goes_on() {
        use std::os::windows::fs::OpenOptionsExt as _;
        let (_tmp, dir) = world();
        let held = cfg(&dir, 7, "worker-1");
        touch(&held, A_DAY);
        touch(&cfg(&dir, 7, "worker-3"), A_DAY);
        let _open = fs::OpenOptions::new()
            .read(true)
            .share_mode(1) // FILE_SHARE_READ only: no delete
            .open(&held)
            .unwrap();
        let report = sweep(&dir, SystemTime::now(), || Some(facts(&[(7, "director")])));
        assert_eq!(report.failed, 1, "{report:?}");
        assert_eq!(report.deleted, [cfg(&dir, 7, "worker-3")]);
        assert!(held.exists());
    }

    #[test]
    fn a_sweep_of_a_directory_that_does_not_exist_creates_nothing() {
        let (tmp, dir) = world();
        let report = sweep(&dir, SystemTime::now(), || panic!("nothing was listed"));
        assert_eq!(report, SweepReport::default());
        assert_eq!(names(tmp.path()), Vec::<String>::new());
    }

    #[test]
    fn a_directory_with_a_banto_name_is_left_alone() {
        let (_tmp, dir) = world();
        let impostor = cfg(&dir, 7, "worker-1");
        fs::create_dir_all(&impostor).unwrap();
        let report = sweep(&dir, SystemTime::now(), || Some(Facts::default()));
        assert_eq!(report, SweepReport::default());
        assert!(impostor.is_dir());
    }

    // --- links ---------------------------------------------------------------
    //
    // A test that needs a link the platform will not let it create says so on
    // stderr and returns: Windows needs Developer Mode or elevation for symbolic
    // links, which a CI runner or another machine may not have.

    fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(target, link)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(std::io::Error::other("no symbolic links here"))
        }
    }

    fn symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(std::io::Error::other("no symbolic links here"))
        }
    }

    macro_rules! linked_or_skip {
        ($made:expr) => {
            if let Err(err) = $made {
                eprintln!("SKIPPED (cannot create the link here: {err})");
                return;
            }
        };
    }

    /// A data directory whose `mcp` (or `mcp/goinkyo`) is a link to `elsewhere`,
    /// where banto-named old orphans sit that the sweep must not touch.
    #[test]
    fn an_mcp_directory_that_is_a_symlink_is_not_swept() {
        let (tmp, dir) = world();
        let elsewhere = tmp.path().join("elsewhere");
        touch(&elsewhere.join("7-worker-1.json"), A_DAY);
        fs::create_dir_all(dir.root()).unwrap();
        linked_or_skip!(symlink_dir(&elsewhere, &dir.mcp_dir()));
        let report = sweep(&dir, SystemTime::now(), || Some(facts(&[])));
        assert_eq!(report, SweepReport::default());
        assert!(elsewhere.join("7-worker-1.json").exists());
    }

    #[test]
    fn a_goinkyo_directory_that_is_a_symlink_is_not_swept_but_mcp_still_is() {
        let (tmp, dir) = world();
        let elsewhere = tmp.path().join("elsewhere");
        touch(&elsewhere.join("7.txt"), A_DAY);
        touch(&cfg(&dir, 7, "worker-1"), A_DAY);
        linked_or_skip!(symlink_dir(&elsewhere, &dir.goinkyo_dir()));
        let report = sweep(&dir, SystemTime::now(), || Some(facts(&[])));
        assert_eq!(report.deleted, [cfg(&dir, 7, "worker-1")]);
        assert!(elsewhere.join("7.txt").exists());
    }

    #[test]
    fn a_file_that_is_a_symlink_is_not_deleted_and_neither_is_what_it_points_to() {
        let (tmp, dir) = world();
        let precious = tmp.path().join("precious.json");
        touch(&precious, A_DAY);
        fs::create_dir_all(dir.mcp_dir()).unwrap();
        let link = cfg(&dir, 7, "worker-1");
        linked_or_skip!(symlink_file(&precious, &link));
        let report = sweep(&dir, SystemTime::now(), || Some(facts(&[])));
        assert_eq!(report, SweepReport::default());
        assert!(fs::symlink_metadata(&link).is_ok());
        assert!(precious.exists());
    }

    /// A junction is not a symbolic link to Windows' API (it needs no privilege),
    /// so it gets its own case; it is made with `mklink /J`, which is how an
    /// operator would.
    #[cfg(windows)]
    #[test]
    fn an_mcp_directory_that_is_a_junction_is_not_swept() {
        let (tmp, dir) = world();
        let elsewhere = tmp.path().join("elsewhere");
        touch(&elsewhere.join("7-worker-1.json"), A_DAY);
        fs::create_dir_all(dir.root()).unwrap();
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(dir.mcp_dir())
            .arg(&elsewhere)
            .output();
        match made {
            Ok(out) if out.status.success() => {}
            other => {
                eprintln!("SKIPPED (cannot create a junction here: {other:?})");
                return;
            }
        }
        assert!(!is_real_directory(&dir.mcp_dir()));
        let report = sweep(&dir, SystemTime::now(), || Some(facts(&[])));
        assert_eq!(report, SweepReport::default());
        assert!(elsewhere.join("7-worker-1.json").exists());
    }
}
