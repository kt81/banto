//! Memoized transcript parsing for [`super::claude_code`], keyed by
//! `(path, mtime, size)`.
//!
//! Every reload re-walks and re-parses every session file to learn that
//! almost none of them moved. Measured on this machine by sampling every 12
//! seconds — near that day's median gap between watcher fires — a mean of
//! 1.13 of the 137 `.jsonl` files under `~/.claude/projects` had changed
//! between one sample and the next. (137 is a raw file count; discovery's own
//! walk returned 81 sessions when that was separately measured, at a
//! different moment.) Walking and stat-ing is not what the reload costs —
//! parsing the JSON head is the single largest phase of discovery, in release
//! as well as debug — so the walk stays exactly as it was and only the parse
//! is memoized. Discovery by watcher path was considered and rejected in the
//! same design: an `Event` from `notify` is an alarm clock, the walk is the
//! truth, and promoting the event stream to truth would put two computations
//! of one fact where nothing could compare them.
//!
//! ## What the key claims, and what defeats it
//!
//! `(mtime, size)` stands in for "the same bytes". It is an approximation and
//! not a proof, so here is exactly where it ends:
//!
//! - A writer that **appends** always moves `size`. Claude Code appends, and
//!   that is the only writer these files have.
//! - A writer that rewrites in place at the same length is caught only by
//!   `mtime`, which on NTFS advances in 100ns FILETIME ticks — enough to rule
//!   out an accidental collision, not a deliberate one.
//! - A writer that **restores** the timestamp it found (a backup restore, a
//!   metadata-preserving copy or sync client, an explicit `SetFileTime`) can
//!   change the bytes and leave the key untouched. Nothing here would notice,
//!   and [`tests::a_same_length_rewrite_under_the_same_mtime_is_not_noticed`]
//!   pins that as accepted behaviour rather than leaving it to be discovered.
//!
//! This is the whole risk: a memo cannot be wrong about a file it re-parses,
//! only about one it decides not to re-parse.
//!
//! ## Only what was read is remembered
//!
//! Parsing is not a pure function of the file's bytes — the path names the
//! session and becomes `source_path`, and the key's own `mtime`/`size` are
//! copied into the result — but everything it depends on is either in the key
//! or is the key. What is *not* in the key is whether the read succeeded, and
//! that is where the first version of this went wrong: a transcript that was
//! momentarily unopenable (a lock, an ACL, a scanner) parsed to nothing, and
//! remembering that under an unchanged key hid a perfectly good session until
//! something wrote to it again. So a parse says which of the two it is
//! ([`Parsed`]), and only an answer the bytes decided is kept.
//!
//! The stat and the read are still two separate moments, as they were before
//! any of this. A write landing between them is a walk that reports content
//! its own key does not describe — and here the memo does change something,
//! though not for long. An uncached walk reads after the stat and so tends to
//! see the *newer* bytes under the older key; a hit answers with the bytes
//! that key was built from, which are the older ones. Neither survives the
//! next walk: the write moved `mtime`/`size`, so the next stat misses and
//! parses. What a memo does not do is carry a torn read forward, which was
//! the thing worth checking.
//!
//! One way that could stop being true has not been measured here: a
//! filesystem that lets a writer hold a file open and delays publishing the
//! new `mtime` until it closes would let two different sets of bytes present
//! the same key across walks. `size` is the guard that would have to fail
//! too, and appends move it.
//!
//! ## What may be memoized and what may never be
//!
//! Only transcript discovery comes through here: a file's own recorded past.
//! What a process is doing *right now* (`sessions/<pid>.json`,
//! `crate::status`) is never cached at any layer, and nothing in this module
//! can reach it.
//!
//! This is a `banto-io` concern in full. The core never sees a cache, a hit
//! or a miss (`docs/DISCIPLINE.md` §2): discovery still produces the same
//! `SessionMeta` values it always did, so record/replay cannot tell whether
//! one was parsed or remembered.

use std::cell::{Cell, RefCell, RefMut};
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use banto_core::model::SessionMeta;

/// What one parse produced, and whether a memo may keep it.
pub enum Parsed {
    /// The file's bytes decided this, so the key describes it: the same key
    /// would produce it again. `None` here is a fact too — these bytes name
    /// no session, and they will go on not naming one.
    Settled(Option<SessionMeta>),
    /// Use this answer now and ask again next time: something other than the
    /// bytes had a say — a read that failed, a part of the file that could
    /// not be reached — and that can change without the key changing.
    Provisional(Option<SessionMeta>),
}

impl Parsed {
    fn answer(&self) -> &Option<SessionMeta> {
        match self {
            Parsed::Settled(meta) | Parsed::Provisional(meta) => meta,
        }
    }

    /// The answer, for a caller with no cache at all.
    pub fn into_answer(self) -> Option<SessionMeta> {
        match self {
            Parsed::Settled(meta) | Parsed::Provisional(meta) => meta,
        }
    }
}

/// A provider-local memo of parsed transcript metadata, held by whoever runs
/// a reload loop and handed to [`super::claude_code::ClaudeCodeProvider`] for
/// the length of one walk.
///
/// Two loops may each hold their own — the concern that two caches could
/// disagree does not apply to a memo, which can differ from another only in
/// how often it had to compute.
pub struct MetaCache {
    entries: RefCell<HashMap<PathBuf, Entry>>,
    /// Which walk an entry was last seen by, so the walk itself can evict —
    /// see [`Self::walk`].
    generation: Cell<u64>,
    /// Divergence log for the verification mode — see [`Self::new`].
    verify: Option<RefCell<File>>,
}

struct Entry {
    mtime: SystemTime,
    size: u64,
    meta: Option<SessionMeta>,
    seen: u64,
}

impl MetaCache {
    /// An empty cache.
    ///
    /// `BANTO_VERIFY_CACHE` turns on the verification mode and names the file
    /// its divergence log is appended to. In that mode every hit is parsed
    /// again anyway, the two answers are compared, and the *fresh* one is
    /// returned — the mode measures whether the memo agrees, and deliberately
    /// does not let it decide anything while measuring. Turning "trust this"
    /// into "this machine's own agreement over a day of real use" is the
    /// whole purpose; it is a diagnostic under `docs/DISCIPLINE.md` §6.2 and
    /// nothing reads the file back.
    pub fn new() -> Self {
        let verify = std::env::var_os("BANTO_VERIFY_CACHE").and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
                .map(RefCell::new)
        });
        Self {
            entries: RefCell::new(HashMap::new()),
            generation: Cell::new(0),
            verify,
        }
    }

    /// Begin a walk, or `None` if one is already in flight on this cache.
    ///
    /// Entries are stamped with the walk that touched them and the rest are
    /// dropped when it ends, so the directory walk is what keeps a memo
    /// alive and a deleted session cannot linger. That bookkeeping is only
    /// coherent for one walk at a time, so a second overlapping one is
    /// refused rather than allowed to interleave: its caller parses
    /// everything, which costs a walk instead of quietly discarding the other
    /// walk's entries. Nothing does this today — the refusal is here so that
    /// nothing has to keep not doing it.
    pub fn walk(&self) -> Option<Walk<'_>> {
        let entries = self.entries.try_borrow_mut().ok()?;
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        Some(Walk {
            cache: self,
            entries,
            generation,
            hits: 0,
            misses: 0,
        })
    }

    fn log(&self, message: &str) {
        use std::io::Write as _;
        if let Some(file) = &self.verify {
            let ms = SystemTime::UNIX_EPOCH
                .elapsed()
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let _ = writeln!(file.borrow_mut(), "{ms} cache: {message}");
        }
    }
}

impl Default for MetaCache {
    fn default() -> Self {
        Self::new()
    }
}

/// One walk's view of a [`MetaCache`]: a lookup table being consulted and
/// re-stamped at the same time.
pub struct Walk<'a> {
    cache: &'a MetaCache,
    entries: RefMut<'a, HashMap<PathBuf, Entry>>,
    generation: u64,
    hits: u64,
    misses: u64,
}

impl Walk<'_> {
    /// The memoized parse: `parse` runs only when nothing is remembered for
    /// this exact `(path, mtime, size)`, and its answer is kept only if it
    /// came back [`Parsed::Settled`].
    pub fn parsed<F>(
        &mut self,
        path: &Path,
        mtime: SystemTime,
        size: u64,
        parse: F,
    ) -> Option<SessionMeta>
    where
        F: FnOnce() -> Parsed,
    {
        let generation = self.generation;
        let remembered = self
            .entries
            .get_mut(path)
            .filter(|entry| entry.mtime == mtime && entry.size == size)
            .map(|entry| {
                entry.seen = generation;
                entry.meta.clone()
            });

        let Some(remembered) = remembered else {
            self.misses += 1;
            let parsed = parse();
            let answer = parsed.answer().clone();
            if let Parsed::Settled(meta) = parsed {
                self.entries.insert(
                    path.to_path_buf(),
                    Entry {
                        mtime,
                        size,
                        meta,
                        seen: generation,
                    },
                );
            } else {
                // Not remembered, and any older answer for this path goes
                // with it: keeping one would mean answering from a memo the
                // next walk was told not to trust.
                self.entries.remove(path);
            }
            return answer;
        };

        self.hits += 1;
        if self.cache.verify.is_none() {
            return remembered;
        }

        // A parse that could not read the file has no opinion about whether
        // the memo is right, so it is neither compared nor kept — only
        // answered with, exactly as an uncached walk would have answered.
        // Without this the verification mode would be the one place that
        // still writes an unread answer into the memo.
        let fresh = match parse() {
            Parsed::Provisional(meta) => return meta,
            Parsed::Settled(meta) => meta,
        };
        if fresh != remembered {
            // The path and the key, never the parsed values: a title or a
            // preview is real session content, and repo invariant 2 keeps
            // that out of any file this repository produces. A path names
            // which session to go and look at, which is what an investigation
            // needs from a log line.
            self.cache.log(&format!(
                "diverged {} mtime={:?} size={size}",
                path.display(),
                mtime.duration_since(SystemTime::UNIX_EPOCH).ok()
            ));
            if let Some(entry) = self.entries.get_mut(path) {
                entry.meta = fresh.clone();
            }
        }
        fresh
    }

    /// Hits and misses so far this walk. In the verification mode a hit is
    /// still counted as a hit even though it was parsed again — the number
    /// says what the memo would have saved, not what it did save.
    pub fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }
}

impl Drop for Walk<'_> {
    /// Evicting here rather than in a `finish()` the caller must remember: a
    /// walk that returns early mid-directory should leave behind what it
    /// learned, and the worst an interrupted walk can do is evict entries it
    /// never reached — a hit-rate loss on the next reload, never a wrong
    /// answer.
    fn drop(&mut self) {
        let generation = self.generation;
        self.entries.retain(|_, entry| entry.seen == generation);
        let (hits, misses) = (self.hits, self.misses);
        self.cache.log(&format!("walk hits={hits} misses={misses}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use banto_core::model::{AgentKind, SessionId};

    fn meta(id: &str, size: u64) -> SessionMeta {
        SessionMeta {
            id: SessionId(id.to_string()),
            agent: AgentKind::ClaudeCode,
            title: Some(id.to_string()),
            cwd: None,
            source_path: PathBuf::from(format!("{id}.jsonl")),
            mtime: SystemTime::UNIX_EPOCH,
            size,
            is_agent: false,
            preview: None,
            continuation_of_uuid: None,
            source_archived: false,
        }
    }

    fn settled(id: &str, size: u64) -> Parsed {
        Parsed::Settled(Some(meta(id, size)))
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }

    fn verifying(log: &Path) -> MetaCache {
        MetaCache {
            entries: RefCell::new(HashMap::new()),
            generation: Cell::new(0),
            verify: Some(RefCell::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log)
                    .unwrap(),
            )),
        }
    }

    #[test]
    fn an_unchanged_key_is_answered_without_parsing() {
        let cache = MetaCache::new();
        let path = Path::new("a.jsonl");

        let mut walk = cache.walk().unwrap();
        assert_eq!(
            walk.parsed(path, at(1), 10, || settled("a", 10)),
            Some(meta("a", 10))
        );
        assert_eq!(walk.stats(), (0, 1));
        drop(walk);

        let mut walk = cache.walk().unwrap();
        let answer = walk.parsed(path, at(1), 10, || panic!("must not parse again"));
        assert_eq!(answer, Some(meta("a", 10)));
        assert_eq!(walk.stats(), (1, 0));
    }

    #[test]
    fn a_moved_mtime_or_size_parses_again() {
        let cache = MetaCache::new();
        let path = Path::new("a.jsonl");

        cache
            .walk()
            .unwrap()
            .parsed(path, at(1), 10, || settled("a", 10));

        let mut walk = cache.walk().unwrap();
        assert_eq!(
            walk.parsed(path, at(2), 10, || settled("rewritten", 10)),
            Some(meta("rewritten", 10))
        );
        assert_eq!(
            walk.parsed(Path::new("b.jsonl"), at(1), 10, || settled("b", 10)),
            Some(meta("b", 10))
        );
        assert_eq!(walk.stats(), (0, 2));
        drop(walk);

        let mut walk = cache.walk().unwrap();
        assert_eq!(
            walk.parsed(path, at(2), 11, || settled("appended", 11)),
            Some(meta("appended", 11))
        );
        assert_eq!(walk.stats(), (0, 1));
    }

    /// The accepted approximation, pinned as behaviour rather than left to be
    /// discovered: a rewrite that keeps both the length and the mtime is
    /// invisible to the key, and the memo answers with what it has. See this
    /// module's doc for which writers can do that and why banto accepts it.
    #[test]
    fn a_same_length_rewrite_under_the_same_mtime_is_not_noticed() {
        let cache = MetaCache::new();
        let path = Path::new("a.jsonl");

        cache
            .walk()
            .unwrap()
            .parsed(path, at(1), 10, || settled("before", 10));

        let mut walk = cache.walk().unwrap();
        let answer = walk.parsed(path, at(1), 10, || settled("after", 10));
        assert_eq!(answer, Some(meta("before", 10)));
    }

    #[test]
    fn a_parse_that_settled_on_nothing_is_remembered_too() {
        let cache = MetaCache::new();
        let path = Path::new("a.jsonl");

        assert_eq!(
            cache
                .walk()
                .unwrap()
                .parsed(path, at(1), 10, || Parsed::Settled(None)),
            None
        );

        let mut walk = cache.walk().unwrap();
        assert_eq!(
            walk.parsed(path, at(1), 10, || panic!("must not parse again")),
            None
        );
        assert_eq!(walk.stats(), (1, 0));
    }

    /// The bug the [`Parsed`] distinction exists for: a transcript that could
    /// not be opened this time parses to nothing, and remembering that would
    /// hide a live session until something wrote to it again — the key cannot
    /// change when the file did not.
    #[test]
    fn a_provisional_answer_is_not_remembered() {
        let cache = MetaCache::new();
        let path = Path::new("a.jsonl");

        assert_eq!(
            cache
                .walk()
                .unwrap()
                .parsed(path, at(1), 10, || Parsed::Provisional(None)),
            None
        );

        let mut walk = cache.walk().unwrap();
        assert_eq!(
            walk.parsed(path, at(1), 10, || settled("readable now", 10)),
            Some(meta("readable now", 10))
        );
        assert_eq!(walk.stats(), (0, 1), "the unreadable walk must not answer");
    }

    /// Where the provisional rule actually bites: the file moved, so the memo
    /// holds nothing usable, *and* this walk could not read the new bytes.
    /// What it could not read must not become the remembered answer.
    ///
    /// A file that has *not* moved never reaches this — it is answered from
    /// the memo without opening anything, so a lock that arrives after banto
    /// has already read those bytes cannot take the row away. That is a
    /// change from the uncached walk, and in the right direction.
    #[test]
    fn a_provisional_answer_after_a_key_change_leaves_nothing_remembered() {
        let cache = MetaCache::new();
        let path = Path::new("a.jsonl");

        cache
            .walk()
            .unwrap()
            .parsed(path, at(1), 10, || settled("a", 10));
        assert_eq!(
            cache
                .walk()
                .unwrap()
                .parsed(path, at(2), 11, || Parsed::Provisional(None)),
            None
        );

        let mut walk = cache.walk().unwrap();
        assert_eq!(
            walk.parsed(path, at(2), 11, || settled("a", 11)),
            Some(meta("a", 11))
        );
        assert_eq!(walk.stats(), (0, 1));
    }

    #[test]
    fn a_path_the_walk_did_not_visit_is_evicted() {
        let cache = MetaCache::new();
        let (a, b) = (Path::new("a.jsonl"), Path::new("b.jsonl"));

        let mut walk = cache.walk().unwrap();
        walk.parsed(a, at(1), 10, || settled("a", 10));
        walk.parsed(b, at(1), 10, || settled("b", 10));
        drop(walk);

        // Second walk sees only `a` — `b` was deleted, archived, or moved out
        // from under banto.
        let mut walk = cache.walk().unwrap();
        walk.parsed(a, at(1), 10, || settled("a", 10));
        drop(walk);

        let mut walk = cache.walk().unwrap();
        walk.parsed(b, at(1), 10, || settled("b", 10));
        assert_eq!(walk.stats(), (0, 1), "b should have been evicted");
    }

    #[test]
    fn a_second_overlapping_walk_is_refused_rather_than_interleaved() {
        let cache = MetaCache::new();
        let path = Path::new("a.jsonl");

        let mut first = cache.walk().unwrap();
        first.parsed(path, at(1), 10, || settled("a", 10));
        assert!(cache.walk().is_none(), "one walk at a time");
        drop(first);

        let mut walk = cache.walk().unwrap();
        assert_eq!(
            walk.parsed(path, at(1), 10, || panic!("must not parse again")),
            Some(meta("a", 10)),
            "the refused walk must not have cost the first one its entries"
        );
    }

    #[test]
    fn verification_mode_returns_the_fresh_answer_and_logs_the_divergence() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("divergence.log");
        let cache = verifying(&log);
        let path = Path::new("a.jsonl");

        cache
            .walk()
            .unwrap()
            .parsed(path, at(1), 10, || settled("before", 10));

        let mut walk = cache.walk().unwrap();
        let answer = walk.parsed(path, at(1), 10, || settled("after", 10));
        assert_eq!(answer, Some(meta("after", 10)), "the fresh answer decides");
        assert_eq!(walk.stats(), (1, 0));
        drop(walk);

        // ...and it is the fresh one the next walk compares against, so one
        // divergence is reported once rather than every walk after it.
        let mut walk = cache.walk().unwrap();
        walk.parsed(path, at(1), 10, || settled("after", 10));
        drop(walk);

        let written = std::fs::read_to_string(&log).unwrap();
        assert_eq!(written.matches("diverged a.jsonl").count(), 1, "{written}");
        assert!(written.contains("walk hits=1 misses=0"), "{written}");
        assert!(
            !written.contains("before") && !written.contains("after"),
            "parsed values must never reach the log: {written}"
        );
    }

    /// The verification mode is the only place a hit re-parses, so it is the
    /// only place a hit could write an answer nobody read into the memo.
    #[test]
    fn verification_mode_neither_reports_nor_remembers_what_it_could_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("divergence.log");
        let cache = verifying(&log);
        let path = Path::new("a.jsonl");

        cache
            .walk()
            .unwrap()
            .parsed(path, at(1), 10, || settled("a", 10));
        assert_eq!(
            cache
                .walk()
                .unwrap()
                .parsed(path, at(1), 10, || Parsed::Provisional(None)),
            None,
            "an uncached walk would have had nothing to show either"
        );

        let mut walk = cache.walk().unwrap();
        assert_eq!(
            walk.parsed(path, at(1), 10, || settled("a", 10)),
            Some(meta("a", 10)),
            "the answer read from bytes must still be there"
        );
        assert_eq!(walk.stats(), (1, 0));
        drop(walk);

        let written = std::fs::read_to_string(&log).unwrap();
        assert!(
            !written.contains("diverged"),
            "a parse that read nothing disagrees with nothing: {written}"
        );
    }

    #[test]
    fn verification_mode_stays_quiet_when_the_memo_agrees() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("divergence.log");
        let cache = verifying(&log);
        let path = Path::new("a.jsonl");

        cache
            .walk()
            .unwrap()
            .parsed(path, at(1), 10, || settled("a", 10));
        cache
            .walk()
            .unwrap()
            .parsed(path, at(1), 10, || settled("a", 10));

        let written = std::fs::read_to_string(&log).unwrap();
        assert!(!written.contains("diverged"), "{written}");
    }
}
