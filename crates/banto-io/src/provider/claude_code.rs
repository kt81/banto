//! Claude Code CLI session provider.
//!
//! Discovers `<claude_home>/projects/<encoded-cwd>/<uuid>.jsonl` files and
//! extracts metadata (title, cwd, mtime, is_agent) by reading only the head
//! of each file. See docs/REQUIREMENTS.md "Data sources" for the observed
//! format, including the `agent-setting` record that marks a session run by
//! a spawned agent rather than started interactively.
//!
//! Parsing is lenient by contract: broken lines are skipped, unknown record
//! types and fields are ignored, and per-file I/O problems never abort
//! discovery.

use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use super::cache::{MetaCache, Parsed};
use super::{ProviderError, SessionProvider};
use crate::claude_home::ClaudeHome;
use crate::file_identity::DirectorySpellings;
use banto_core::model::{AgentKind, SessionId, SessionMeta};

/// Maximum number of bytes read from the head of each session file.
///
/// The cwd and a `custom-title` sit in the first few records, but an
/// `ai-title` does not any more. Measured over 62 real transcripts that had
/// one: in the 14 from Claude Code 2.1.202 through 2.1.209 it was within the
/// first 32153 bytes; the one from 2.1.227 had it at byte 421969; of the 47
/// from 2.1.318 on, 13 had it inside this cap (the earliest at byte 455), the
/// median sat at byte 283781 and the latest at byte 1057871, behind
/// `attachment` and `api-request-blob` records. Nothing was measured between
/// 2.1.227 and 2.1.318, so where it moved is not known. A head of 512 KiB
/// would cost about half again as much parse time per file and still miss that
/// last one, which is why the part of the file past this cap is searched for
/// the title by [`scan_for_ai_title`] instead of by widening this.
const HEAD_CAP_BYTES: u64 = 256 * 1024;

/// How far past the head's kept text [`scan_for_ai_title`] looks for the first
/// `ai-title` record: a line is considered when it *starts* within this many
/// bytes, and the read is cut off at this plus [`AI_TITLE_LINE_MAX_BYTES`] and
/// one, so the last such line can be read whole. The latest first offset in the
/// measurement behind [`HEAD_CAP_BYTES`] was 1057871 bytes into the file, so
/// this leaves room for one that lands well beyond it.
const AI_TITLE_SCAN_CAP_BYTES: u64 = 2 * 1024 * 1024;

/// Buffer size of that scan's reader. The default 8 KiB would make a 2 MiB
/// window cost a few hundred reads instead of a few.
const AI_TITLE_SCAN_BUFFER_BYTES: usize = 256 * 1024;

/// Longest line [`scan_for_ai_title`] will parse, newline not counted. A real
/// `ai-title` record is a few hundred bytes. Of a longer line only this many
/// bytes and one more are copied into memory, the rest is passed over where it
/// lies, and it is not parsed, whatever it begins with.
const AI_TITLE_LINE_MAX_BYTES: usize = 64 * 1024;

/// What an `ai-title` record's line begins with. Tested on a whole line, never
/// searched for inside a buffer: a raw newline cannot occur inside a JSON
/// string (it is written `\n`), so text that quotes such a record inside
/// another record's content can never be at the start of a line.
const AI_TITLE_LINE_START: &[u8] = b"{\"type\":\"ai-title\"";

/// Maximum number of bytes read from the tail of a session file larger than
/// [`HEAD_CAP_BYTES`], to catch a `custom-title` record a rename (`/rename`)
/// appended well past the head. Claude Code re-asserts the current title
/// periodically (not only at rename time), so in practice this window
/// catches a rename even in a very large file soon after it happens.
const TAIL_CAP_BYTES: u64 = 64 * 1024;

/// Maximum title length in characters (not bytes). Also used to cap
/// `SessionMeta.preview` (see [`HeadFields::preview`]).
const TITLE_MAX_CHARS: usize = 200;

/// Session provider for the Claude Code CLI.
///
/// Reads session `.jsonl` files under `<claude_home>/projects/`. Everything
/// under `claude_home` is treated as strictly read-only.
pub struct ClaudeCodeProvider {
    claude_home: ClaudeHome,
}

impl ClaudeCodeProvider {
    /// Create a provider rooted at `claude_home`, the directory that
    /// contains `projects/` (normally `~/.claude`).
    pub fn new(claude_home: ClaudeHome) -> Self {
        Self { claude_home }
    }

    /// [`SessionProvider::discover`], with the parse of each unchanged file
    /// answered from `cache` instead of repeated. Same walk, same stats, same
    /// results — see [`cache`] for why a memo cannot change the answer.
    ///
    /// Separate from the trait method rather than a field on the provider
    /// because a provider is built per call at the discovery entry point,
    /// while the cache has to outlive every reload; the caller that owns the
    /// loop owns the cache, and hands it in.
    pub fn discover_cached(&self, cache: &MetaCache) -> Result<Vec<SessionMeta>, ProviderError> {
        self.walk(Some(cache))
    }

    fn walk(&self, cache: Option<&MetaCache>) -> Result<Vec<SessionMeta>, ProviderError> {
        let projects = self.claude_home.projects_dir();
        let entries = match fs::read_dir(&projects) {
            Ok(entries) => entries,
            // A Claude home without a projects dir simply has no sessions.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(ProviderError::Io(err)),
        };

        let mut memo = cache.and_then(MetaCache::walk);
        let mut sessions = Vec::new();
        for project in entries {
            let Ok(project) = project else { continue };
            let project_path = project.path();
            // One level of project directories; stray files are ignored.
            if !project_path.is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(&project_path) else {
                continue;
            };
            for file in files {
                let Ok(file) = file else { continue };
                let path = file.path();
                let Some((mtime, size)) = stat_session(&path) else {
                    continue;
                };
                let meta = match &mut memo {
                    Some(memo) => {
                        memo.parsed(&path, mtime, size, || parse_session(&path, mtime, size))
                    }
                    None => parse_session(&path, mtime, size).into_answer(),
                };
                if let Some(meta) = meta {
                    sessions.push(meta);
                }
            }
        }
        Ok(sessions)
    }
}

impl SessionProvider for ClaudeCodeProvider {
    fn name(&self) -> AgentKind {
        AgentKind::ClaudeCode
    }

    fn discover(&self) -> Result<Vec<SessionMeta>, ProviderError> {
        self.walk(None)
    }

    fn find_new_sessions_in(
        &self,
        spawned: &DirectorySpellings,
        since: SystemTime,
    ) -> Vec<SessionId> {
        let projects = self.claude_home.projects_dir();
        let Ok(entries) = fs::read_dir(&projects) else {
            return Vec::new();
        };

        // Claude records the directory it resolved (a symlink followed), in the
        // case it was typed: a spelling `spawned` lists, so the comparison is
        // text, with nothing opened for a transcript's recorded cwd.
        let mut matches: Vec<(SystemTime, SessionId)> = Vec::new();
        for project in entries {
            let Ok(project) = project else { continue };
            let project_path = project.path();
            if !project_path.is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(&project_path) else {
                continue;
            };
            for file in files {
                let Ok(file) = file else { continue };
                let path = file.path();
                if !path.extension().is_some_and(|ext| ext == "jsonl") {
                    continue;
                }
                let Ok(metadata) = fs::metadata(&path) else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                let Ok(mtime) = metadata.modified() else {
                    continue;
                };
                if mtime < since {
                    continue;
                }
                let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Ok((head, _)) = read_head(&path, HEAD_CAP_BYTES) else {
                    continue;
                };
                let fields = extract_head_fields(&head);
                if !fields
                    .cwd
                    .as_deref()
                    .is_some_and(|reported| spawned.names(reported))
                {
                    continue;
                }
                matches.push((mtime, SessionId(id.to_string())));
            }
        }
        matches.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.0.cmp(&b.1.0)));
        matches.into_iter().map(|(_, id)| id).collect()
    }
}

/// The half of session reading a `stat` can decide: whether this entry is a
/// non-empty `.jsonl` file at all, and the `(mtime, size)` that [`cache`]
/// keys a parse by. Cheap — the whole walk's worth of these is a few
/// milliseconds, against tens for the parsing they gate.
///
/// Says nothing about whether the file can be *opened*: that is not a fact
/// about the file's content and must not end up in the key (see [`cache`]'s
/// "only what was read is remembered").
fn stat_session(path: &Path) -> Option<(SystemTime, u64)> {
    if !path.extension().is_some_and(|ext| ext == "jsonl") {
        return None;
    }
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() == 0 {
        return None;
    }
    Some((metadata.modified().ok()?, metadata.len()))
}

/// Build a [`SessionMeta`] for one candidate path already accepted by
/// [`stat_session`], and say whether the answer is one a memo may keep.
///
/// Everything the result depends on is the path or the key: the path names
/// the session and becomes `source_path`, and `mtime`/`size` are the stat's
/// own, passed in rather than read again so the stored values are the ones
/// the key describes. The exception is whether the file could be read at all,
/// which is why this returns [`Parsed`] instead of an `Option` — an
/// unreadable transcript and one that says nothing are the same answer today
/// and different answers tomorrow.
fn parse_session(path: &Path, mtime: SystemTime, size: u64) -> Parsed {
    // A name banto cannot turn into a session id is a fact about the path,
    // and the path is part of the key.
    let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return Parsed::Settled(None);
    };
    let Ok((head, head_kept)) = read_head(path, HEAD_CAP_BYTES) else {
        return Parsed::Provisional(None);
    };
    let mut fields = extract_head_fields(&head);
    // A rename can append a fresh custom-title record well past the head; a
    // bounded tail read catches the latest one without scanning the whole
    // file. Skipped when the head read already covered the whole file.
    let mut settled = true;
    if size > HEAD_CAP_BYTES {
        match read_tail(path, TAIL_CAP_BYTES) {
            Ok(tail) => {
                if let Some(latest) = latest_custom_title(&tail) {
                    fields.custom_title = Some(latest);
                }
            }
            // The head still yields a usable row, and that is what this
            // returns — but a title the tail would have overridden is a title
            // this parse did not get to see, so it is not one to remember.
            Err(_) => settled = false,
        }
        // Claude Code's own title now usually sits past the head (see
        // `HEAD_CAP_BYTES`). Only worth looking for when the title would
        // otherwise be the first user line: a usable custom-title wins over
        // it wherever it was found, and a usable one already seen in the head
        // makes the search pointless. "Usable" is what `title_and_source`
        // accepts, so a blank `customTitle` (an empty one, or a rename that
        // cleared the title) does not suppress the search. The ai-title is a
        // different record from the one the tail looks for, so the tail
        // finding no custom-title says nothing about it.
        if !fields.has_title_record() {
            match scan_for_ai_title(path, head_kept, AI_TITLE_SCAN_CAP_BYTES) {
                Ok(found) => fields.ai_title = found,
                // Same reason as the tail's: a title this parse failed to look
                // for must not be remembered as "this file has none".
                Err(_) => settled = false,
            }
        }
    }
    let (title, title_is_custom) = fields.title_and_source();
    let preview = fields.preview();
    let meta = Some(SessionMeta {
        id: SessionId(id.to_string()),
        agent: AgentKind::ClaudeCode,
        title,
        title_is_custom,
        cwd: fields.cwd,
        source_path: path.to_path_buf(),
        mtime,
        size,
        is_agent: fields.is_agent,
        preview,
        // No equivalent signal exists yet — see SessionMeta::source_archived's
        // doc for why false, not a guess, is the correct value here.
        source_archived: false,
    });
    if settled {
        Parsed::Settled(meta)
    } else {
        Parsed::Provisional(meta)
    }
}

/// Read at most `cap` bytes from the head of `path` as lossy UTF-8, and say
/// how many bytes of the file that text stands for.
///
/// If the cap is hit, everything after the last newline is dropped so that
/// a truncated final line is never parsed. The byte count is taken after that
/// cut, so the next unread byte is always the start of a line (or the start of
/// the file, when no newline fell inside the cap): that is where
/// [`scan_for_ai_title`] must begin. Counted in bytes of the file, not of the
/// returned `String`, which lossy decoding can make longer.
fn read_head(path: &Path, cap: u64) -> std::io::Result<(String, u64)> {
    let file = File::open(path)?;
    let mut buf = Vec::new();
    file.take(cap).read_to_end(&mut buf)?;
    if buf.len() as u64 == cap {
        match buf.iter().rposition(|&b| b == b'\n') {
            Some(pos) => buf.truncate(pos + 1),
            None => buf.clear(),
        }
    }
    let kept = buf.len() as u64;
    Ok((String::from_utf8_lossy(&buf).into_owned(), kept))
}

/// The value of the first `ai-title` record starting within `cap` bytes of
/// byte offset `from`, which must be the start of a line (what [`read_head`]
/// returns). `Ok(None)` when there is none in that window or the record found
/// cannot be read; an `Err` means the file could not be.
///
/// Line by line, copying and parsing only a line that begins like an
/// `ai-title` record: the lines in front of it are the large `attachment` and
/// `api-request-blob` records, and parsing them is what makes a bigger head
/// expensive. A line that begins like one but does not parse as one is skipped
/// and the scan goes on, as is a last line with no newline (its writer has not
/// finished it).
///
/// The line ends are found by `read_until` and `skip_until`, not by a search
/// of ours: std's own memchr is precompiled, so it matches the `memchr` crate
/// in a release build and beats it in a dev build, where this crate is not
/// optimised (measured on 31 real transcripts and 2 MiB of synthetic lines
/// with no hit: 8.0 against 8.1 ms and 0.4 against 1.0 ms in release, 10.4
/// against 11.4 ms and 0.5 against 1.2 ms in dev). A hand-written search over a
/// buffer was 5 to 30 times slower in dev. Do not swap it back for either.
fn scan_for_ai_title(path: &Path, from: u64, cap: u64) -> std::io::Result<Option<String>> {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(from))?;
    // A line that starts inside the window is read to its end, so the file is
    // cut off at the longest one worth parsing past it. That also bounds how
    // far `skip_until` can run over a line that never ends.
    let mut lines = BufReader::with_capacity(
        AI_TITLE_SCAN_BUFFER_BYTES,
        file.take(cap + AI_TITLE_LINE_MAX_BYTES as u64 + 1),
    );
    let mut line = Vec::new();
    // Offset of the line about to be read, from `from`.
    let mut start = 0u64;
    while start < cap {
        line.clear();
        // One byte more than the longest line worth parsing, so that a line
        // that long cannot be told from one that is cut short, and the rest of
        // it is not copied.
        let n = lines
            .by_ref()
            .take(AI_TITLE_LINE_MAX_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        start += n as u64;
        if line.last() != Some(&b'\n') {
            // Too long to be the record, or the unfinished last line, where
            // the skip finds nothing more.
            start += lines.skip_until(b'\n')? as u64;
        } else if line.starts_with(AI_TITLE_LINE_START)
            && let Some(title) = ai_title_of(&line)
        {
            return Ok(Some(title));
        }
    }
    Ok(None)
}

/// The `aiTitle` of one line, if it is an `ai-title` record that has one.
fn ai_title_of(line: &[u8]) -> Option<String> {
    let record: Value = serde_json::from_slice(line).ok()?;
    if record.get("type").and_then(Value::as_str) != Some("ai-title") {
        return None;
    }
    record
        .get("aiTitle")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Read at most `cap` bytes from the tail of `path` as lossy UTF-8.
///
/// The seek point almost never lands on a line boundary, so unless it lands
/// at the very start of the file (nothing to skip: the whole file was read),
/// everything up to and including the first newline is dropped so a
/// truncated leading line is never parsed.
fn read_tail(path: &Path, cap: u64) -> std::io::Result<String> {
    use std::io::{Seek, SeekFrom};

    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(cap);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    if start > 0 {
        buf = match buf.iter().position(|&b| b == b'\n') {
            Some(pos) => buf.split_off(pos + 1),
            None => Vec::new(),
        };
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Scan `tail` for the value of the *last* `custom-title` record in it (the
/// newest one, since `tail` comes from the end of the file), leniently
/// skipping broken lines and unrelated record types. `None` if the tail
/// window contains no custom-title record.
fn latest_custom_title(tail: &str) -> Option<String> {
    let mut latest = None;
    for line in tail.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if record.get("type").and_then(Value::as_str) == Some("custom-title")
            && let Some(value) = record.get("customTitle").and_then(Value::as_str)
        {
            latest = Some(value.to_string());
        }
    }
    latest
}

/// Fields collected from the head records of a session file.
#[derive(Default)]
struct HeadFields {
    custom_title: Option<String>,
    ai_title: Option<String>,
    user_text: Option<String>,
    cwd: Option<PathBuf>,
    is_agent: bool,
}

impl HeadFields {
    /// Pick the title by priority: custom-title > ai-title > first user
    /// message. Candidates that normalize to an empty string are skipped.
    ///
    /// Also says whether it came from a `custom-title` record: the
    /// operator's own `/rename`, the one source banto's own naming must not
    /// override (`SessionMeta::title_is_custom`). A custom title that normalizes
    /// to nothing is skipped like any other, so it does not make the next
    /// source count as custom.
    fn title_and_source(&self) -> (Option<String>, bool) {
        if let Some(title) = self
            .custom_title
            .as_deref()
            .and_then(|raw| normalize_single_line(raw, TITLE_MAX_CHARS))
        {
            return (Some(title), true);
        }
        let title = [&self.ai_title, &self.user_text]
            .into_iter()
            .flatten()
            .find_map(|raw| normalize_single_line(raw, TITLE_MAX_CHARS));
        (title, false)
    }

    /// Whether [`HeadFields::title_and_source`] would take the title from a
    /// `custom-title` or an `ai-title` record, that is, from one that is not
    /// blank, rather than fall back to the first user message.
    fn has_title_record(&self) -> bool {
        [&self.custom_title, &self.ai_title]
            .into_iter()
            .flatten()
            .any(|raw| normalize_single_line(raw, TITLE_MAX_CHARS).is_some())
    }

    /// The first user message, normalized for the summary panel.
    /// Independent of `title`: unlike title, this never falls back to a
    /// custom/ai title, since those are already shown as the title itself.
    fn preview(&self) -> Option<String> {
        self.user_text
            .as_deref()
            .and_then(|raw| normalize_single_line(raw, TITLE_MAX_CHARS))
    }
}

/// Scan head lines and collect title candidates and the first cwd.
/// Broken lines and unknown record types are silently skipped.
fn extract_head_fields(head: &str) -> HeadFields {
    let mut fields = HeadFields::default();
    for line in head.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if fields.cwd.is_none()
            && let Some(cwd) = record.get("cwd").and_then(Value::as_str)
        {
            fields.cwd = Some(PathBuf::from(cwd));
        }
        match record.get("type").and_then(Value::as_str) {
            // A spawned agent (subagent / Agent-Teams teammate) session opens
            // with this record instead of the interactive `mode` record; it
            // always precedes any title/user record, so it is never missed
            // by the early-break below.
            Some("agent-setting") => fields.is_agent = true,
            Some("custom-title") if fields.custom_title.is_none() => {
                fields.custom_title = record
                    .get("customTitle")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            Some("ai-title") if fields.ai_title.is_none() => {
                fields.ai_title = record
                    .get("aiTitle")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            Some("user") if fields.user_text.is_none() && !is_local_command_record(&record) => {
                fields.user_text = message_text(&record).filter(|text| !is_command_wrapper(text));
            }
            _ => {}
        }
        // custom-title has top priority, so once it and cwd are known
        // nothing later in the file can change the result.
        if fields.custom_title.is_some() && fields.cwd.is_some() {
            break;
        }
    }
    fields
}

/// A `user` record Claude Code writes for a slash command the operator ran
/// (`/clear`, `/model`, `/help`, ...), not for something the operator said to
/// the model. `/clear` opens its new conversation with one flagged `isMeta`
/// (`<local-command-caveat>...`), then a wrapper record
/// (`<command-name>/clear</command-name>...`); a command that prints something
/// adds a `<local-command-stdout>` one. Read as the first user message they gave
/// the list a title of `<local-command-caveat...`, and made a Goinkyo whose
/// operator had run `/clear` before its kickoff look as if it had been spoken to.
fn is_local_command_record(record: &Value) -> bool {
    record.get("isMeta").and_then(Value::as_bool) == Some(true)
}

/// The text of a slash command's wrapper record, which is not flagged `isMeta`
/// (see [`is_local_command_record`]) and is told by its tags alone.
fn is_command_wrapper(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("<local-command-") || text.starts_with("<command-name>")
}

/// Extract the text of a user record's `message.content`, which is either
/// a plain string or an array of content blocks (text blocks are joined,
/// non-text blocks are skipped). Returns `None` for empty/absent text.
fn message_text(record: &Value) -> Option<String> {
    let content = record.get("message")?.get("content")?;
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Collapse all whitespace runs (including newlines) into single spaces and
/// cap the result at `max_chars` characters, respecting char boundaries.
/// Returns `None` if nothing remains.
fn normalize_single_line(raw: &str, max_chars: usize) -> Option<String> {
    let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    Some(normalized.chars().take(max_chars).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    fn provider(root: &TempDir) -> ClaudeCodeProvider {
        ClaudeCodeProvider::new(ClaudeHome::new(root.path().to_path_buf()))
    }

    /// Set a file's mtime deterministically, matching `banto::session`'s
    /// `filetime_set` helper.
    fn set_mtime(path: &Path, time: SystemTime) {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }

    fn write_session(root: &TempDir, project: &str, file: &str, content: &str) -> PathBuf {
        let dir = root.path().join("projects").join(project);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file);
        fs::write(&path, content).unwrap();
        path
    }

    fn discover_sorted(root: &TempDir) -> Vec<SessionMeta> {
        let mut sessions = provider(root).discover().unwrap();
        sessions.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        sessions
    }

    #[test]
    fn missing_projects_dir_yields_empty() {
        let root = TempDir::new().unwrap();
        assert!(discover_sorted(&root).is_empty());
    }

    #[test]
    fn only_a_custom_title_is_marked_as_given() {
        // The operator's `/rename` is a name someone gave; the other two sources
        // are derived. The flag is what lets banto's own naming of a Goinkyo's
        // session leave the first alone and replace the others.
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "custom.jsonl",
            r#"{"type":"ai-title","aiTitle":"AI title"}
{"type":"custom-title","customTitle":"Custom title"}
"#,
        );
        write_session(
            &root,
            "proj",
            "ai.jsonl",
            r#"{"type":"ai-title","aiTitle":"AI title"}
{"type":"user","message":{"content":"first line"}}
"#,
        );
        write_session(
            &root,
            "proj",
            "first.jsonl",
            r#"{"type":"user","message":{"content":"first line"}}
"#,
        );
        write_session(&root, "proj", "none.jsonl", "{\"type\":\"mode\"}\n");

        let given: Vec<(String, Option<String>, bool)> = discover_sorted(&root)
            .into_iter()
            .map(|meta| (meta.id.0, meta.title, meta.title_is_custom))
            .collect();

        assert_eq!(
            given,
            [
                ("ai".to_string(), Some("AI title".to_string()), false),
                ("custom".to_string(), Some("Custom title".to_string()), true),
                ("first".to_string(), Some("first line".to_string()), false),
                ("none".to_string(), None, false),
            ]
        );
    }

    #[test]
    fn a_custom_title_with_nothing_in_it_does_not_make_the_next_source_custom() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"custom-title","customTitle":"  "}
{"type":"ai-title","aiTitle":"AI title"}
"#,
        );

        let sessions = discover_sorted(&root);

        assert_eq!(sessions[0].title.as_deref(), Some("AI title"));
        assert!(!sessions[0].title_is_custom);
    }

    #[test]
    fn a_rename_past_the_head_is_marked_as_given_too() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            &padded_past_head_cap(
                "{\"type\":\"custom-title\",\"customTitle\":\"renamed later\"}\n",
            ),
        );

        let sessions = discover_sorted(&root);

        assert_eq!(sessions[0].title.as_deref(), Some("renamed later"));
        assert!(sessions[0].title_is_custom);
    }

    #[test]
    fn custom_title_wins_over_ai_title() {
        let root = TempDir::new().unwrap();
        // custom-title appears after ai-title but still wins by priority.
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"ai-title","aiTitle":"AI title"}
{"type":"custom-title","customTitle":"Custom title"}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title.as_deref(), Some("Custom title"));
    }

    #[test]
    fn ai_title_alone() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"ai-title","aiTitle":"AI generated"}
{"type":"user","message":{"role":"user","content":"hello"}}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].title.as_deref(), Some("AI generated"));
    }

    #[test]
    fn preview_comes_from_the_first_user_message_even_when_a_title_wins() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"custom-title","customTitle":"Custom title"}
{"type":"user","message":{"content":"  the actual first message \n text  "}}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].title.as_deref(), Some("Custom title"));
        assert_eq!(
            sessions[0].preview.as_deref(),
            Some("the actual first message text")
        );
    }

    // The record shapes below were measured on a real Claude Code; the ids,
    // paths and text here are invented.

    #[test]
    fn a_typed_first_line_is_the_title_and_the_preview() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"mode","mode":"default","cwd":"/tmp/proj"}
{"type":"user","message":{"role":"user","content":"fix the parser"}}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].title.as_deref(), Some("fix the parser"));
        assert_eq!(sessions[0].preview.as_deref(), Some("fix the parser"));
    }

    #[test]
    fn clears_own_records_are_not_the_first_message() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"user","isMeta":true,"cwd":"/tmp/proj","message":{"role":"user","content":"<local-command-caveat>Caveat: the messages below were generated by the user while running local commands.</local-command-caveat>"}}
{"type":"user","cwd":"/tmp/proj","message":{"role":"user","content":"<command-name>/clear</command-name>
<command-message>clear</command-message>
<command-args></command-args>"}}
{"type":"user","cwd":"/tmp/proj","message":{"role":"user","content":"<local-command-stdout></local-command-stdout>"}}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].preview, None);
        assert_eq!(sessions[0].title, None);
    }

    #[test]
    fn what_is_typed_after_a_clear_is_the_first_message() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"user","isMeta":true,"cwd":"/tmp/proj","message":{"role":"user","content":"<local-command-caveat>Caveat: generated while running local commands.</local-command-caveat>"}}
{"type":"user","cwd":"/tmp/proj","message":{"role":"user","content":"<command-name>/clear</command-name>
<command-message>clear</command-message>"}}
{"type":"user","cwd":"/tmp/proj","message":{"role":"user","content":"<local-command-stdout></local-command-stdout>"}}
{"type":"user","cwd":"/tmp/proj","message":{"role":"user","content":"now the real question"}}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].title.as_deref(), Some("now the real question"));
        assert_eq!(
            sessions[0].preview.as_deref(),
            Some("now the real question")
        );
    }

    #[test]
    fn a_model_command_before_the_first_message_is_skipped_too() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"user","isMeta":true,"cwd":"/tmp/proj","message":{"role":"user","content":"<local-command-caveat>Caveat: generated while running local commands.</local-command-caveat>"}}
{"type":"user","cwd":"/tmp/proj","message":{"role":"user","content":"<command-name>/model</command-name>
<command-message>model</command-message>
<command-args>sonnet</command-args>"}}
{"type":"user","cwd":"/tmp/proj","message":{"role":"user","content":"<local-command-stdout>Set model to sonnet</local-command-stdout>"}}
{"type":"user","cwd":"/tmp/proj","message":{"role":"user","content":[{"type":"text","text":"  review this diff "}]}}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].preview.as_deref(), Some("review this diff"));
    }

    #[test]
    fn a_branched_head_copies_the_parents_first_message() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"user","forkedFrom":{"sessionId":"parent-1","messageUuid":"u1"},"cwd":"/tmp/proj","message":{"role":"user","content":"the parent's first line"}}
{"type":"assistant","forkedFrom":{"sessionId":"parent-1","messageUuid":"a1"},"message":{"role":"assistant","content":[{"type":"text","text":"ok"}]}}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(
            sessions[0].preview.as_deref(),
            Some("the parent's first line")
        );
    }

    #[test]
    fn a_custom_title_still_ends_the_scan_past_slash_command_records() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"custom-title","customTitle":"Named by the operator"}
{"type":"user","isMeta":true,"cwd":"/tmp/proj","message":{"role":"user","content":"<local-command-caveat>x</local-command-caveat>"}}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].title.as_deref(), Some("Named by the operator"));
        assert_eq!(sessions[0].preview, None);
    }

    #[test]
    fn preview_is_none_without_a_user_message() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"ai-title","aiTitle":"AI generated"}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].preview, None);
    }

    #[test]
    fn preview_is_capped_at_the_same_length_as_title() {
        let root = TempDir::new().unwrap();
        let long = "a".repeat(400);
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            &format!(r#"{{"type":"user","message":{{"content":"{long}"}}}}"#),
        );
        let sessions = discover_sorted(&root);
        let preview = sessions[0].preview.as_deref().unwrap();
        assert_eq!(preview.chars().count(), TITLE_MAX_CHARS);
        assert_eq!(
            sessions[0].title.as_deref().unwrap().chars().count(),
            TITLE_MAX_CHARS
        );
    }

    #[test]
    fn falls_back_to_first_user_message_string() {
        let root = TempDir::new().unwrap();
        // Whitespace (including newlines) is normalized to single spaces.
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            "{\"type\":\"summary\",\"summary\":\"ignored\"}\n{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"  fix the \\n  login bug \"}}\n",
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].title.as_deref(), Some("fix the login bug"));
    }

    #[test]
    fn falls_back_to_first_user_message_content_blocks() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ignored"},{"type":"text","text":"first"},{"type":"image","source":{}},{"type":"text","text":"second"}]}}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].title.as_deref(), Some("first second"));
    }

    #[test]
    fn agent_setting_record_marks_the_session_as_agent_run() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"agent-setting","agentSetting":"claude","sessionId":"s1"}
{"type":"custom-title","customTitle":"Review PR 42"}
"#,
        );
        let sessions = discover_sorted(&root);
        assert!(sessions[0].is_agent);
    }

    #[test]
    fn sessions_without_agent_setting_are_not_flagged() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"mode","mode":"interactive"}
{"type":"ai-title","aiTitle":"Ordinary session"}
"#,
        );
        let sessions = discover_sorted(&root);
        assert!(!sessions[0].is_agent);
    }

    #[test]
    fn agent_setting_deep_in_the_head_after_broken_lines_still_marks_agent() {
        let root = TempDir::new().unwrap();
        // Garbage and an unrelated record type precede agent-setting; neither
        // sets custom_title/cwd, so the early-break can't have skipped past
        // it before it was read.
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"this is not json
{"type":"unknown-record"}
{"type":"agent-setting","agentSetting":"claude","sessionId":"s1"}
{"type":"ai-title","aiTitle":"Deep marker"}
"#,
        );
        let sessions = discover_sorted(&root);
        assert!(sessions[0].is_agent);
    }

    #[test]
    fn broken_lines_are_skipped() {
        let root = TempDir::new().unwrap();
        // Non-JSON garbage, a truncated record, and a title record without
        // its payload field are all skipped without aborting the parse.
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"this is not json
{"type":"ai-title","aiTitle":"truncated
{"type":"custom-title"}
{"type":"user","message":{"content":"real title"},"cwd":"/work/proj"}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title.as_deref(), Some("real title"));
        assert_eq!(sessions[0].cwd.as_deref(), Some(Path::new("/work/proj")));
    }

    #[test]
    fn empty_file_is_skipped() {
        let root = TempDir::new().unwrap();
        write_session(&root, "proj", "empty.jsonl", "");
        write_session(
            &root,
            "proj",
            "good.jsonl",
            r#"{"type":"ai-title","aiTitle":"kept"}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id.0, "good");
    }

    #[test]
    fn non_jsonl_entries_are_ignored() {
        let root = TempDir::new().unwrap();
        write_session(&root, "proj", "notes.txt", "not a session");
        write_session(&root, "proj", "state.json", r#"{"type":"user"}"#);
        // A directory whose name ends in .jsonl must also be ignored.
        fs::create_dir_all(root.path().join("projects/proj/trap.jsonl")).unwrap();
        write_session(
            &root,
            "proj",
            "real.jsonl",
            r#"{"type":"ai-title","aiTitle":"only one"}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id.0, "real");
    }

    #[test]
    fn unicode_title_is_capped_at_200_chars() {
        let root = TempDir::new().unwrap();
        let long = "\u{3042}".repeat(250); // 250 x Japanese hiragana A
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            &format!("{{\"type\":\"custom-title\",\"customTitle\":\"{long}\"}}\n"),
        );
        let sessions = discover_sorted(&root);
        let title = sessions[0].title.as_deref().unwrap();
        assert_eq!(title.chars().count(), 200);
        assert_eq!(title, "\u{3042}".repeat(200));
    }

    #[test]
    fn cwd_comes_from_first_record_with_a_string_cwd() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"summary","cwd":12345}
{"type":"user","cwd":"/home/kt81/work","message":{"content":"hi"}}
{"type":"assistant","cwd":"/somewhere/else"}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(
            sessions[0].cwd.as_deref(),
            Some(Path::new("/home/kt81/work"))
        );
    }

    #[test]
    fn meta_fields_come_from_fs_metadata() {
        let root = TempDir::new().unwrap();
        let content = "{\"type\":\"ai-title\",\"aiTitle\":\"meta\"}\n";
        let path = write_session(&root, "proj", "abcd-1234.jsonl", content);
        let sessions = discover_sorted(&root);
        assert_eq!(sessions.len(), 1);
        let meta = &sessions[0];
        assert_eq!(meta.id.0, "abcd-1234");
        assert_eq!(meta.agent, AgentKind::ClaudeCode);
        assert_eq!(meta.source_path, path);
        assert_eq!(meta.size, content.len() as u64);
        assert_eq!(meta.mtime, fs::metadata(&path).unwrap().modified().unwrap());
    }

    #[test]
    fn records_beyond_head_cap_are_not_parsed() {
        let root = TempDir::new().unwrap();
        // One unterminated garbage line larger than the head cap, followed
        // by a user record: what it says is out of reach (only an `ai-title`
        // is searched for past the head, see `scan_for_ai_title`), but the
        // session is still listed with its fs metadata.
        let mut content = "x".repeat(300 * 1024);
        content.push_str(
            "\n{\"type\":\"user\",\"cwd\":\"/late\",\"message\":{\"content\":\"unreachable\"}}\n",
        );
        write_session(&root, "proj", "big.jsonl", &content);
        let sessions = discover_sorted(&root);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, None);
        assert_eq!(sessions[0].cwd, None);
        assert_eq!(sessions[0].size, content.len() as u64);
    }

    #[test]
    fn sessions_from_multiple_project_dirs_are_merged() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj-a",
            "a.jsonl",
            r#"{"type":"ai-title","aiTitle":"A"}
"#,
        );
        write_session(
            &root,
            "proj-b",
            "b.jsonl",
            r#"{"type":"ai-title","aiTitle":"B"}
"#,
        );
        // A stray file directly under projects/ is ignored.
        fs::write(root.path().join("projects/stray.jsonl"), "{}").unwrap();
        let sessions = discover_sorted(&root);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id.0, "a");
        assert_eq!(sessions[1].id.0, "b");
    }

    #[test]
    fn find_new_session_matches_a_fresh_file_with_matching_cwd() {
        let root = TempDir::new().unwrap();
        let since = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let path = write_session(
            &root,
            "proj",
            "new.jsonl",
            r#"{"type":"user","cwd":"/work/proj","message":{"content":"hi"}}
"#,
        );
        set_mtime(&path, since + Duration::from_secs(1));

        let found = provider(&root).find_new_session(Path::new("/work/proj"), since);
        assert_eq!(found, Some(SessionId("new".to_string())));
    }

    #[test]
    fn find_new_session_ignores_a_different_cwd() {
        let root = TempDir::new().unwrap();
        let since = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let path = write_session(
            &root,
            "proj",
            "new.jsonl",
            r#"{"type":"user","cwd":"/work/other","message":{"content":"hi"}}
"#,
        );
        set_mtime(&path, since + Duration::from_secs(1));

        let found = provider(&root).find_new_session(Path::new("/work/proj"), since);
        assert_eq!(found, None);
    }

    #[test]
    fn find_new_session_ignores_a_file_older_than_since_even_with_matching_cwd() {
        // Guards against re-finding a pre-existing session that happens to
        // share the target cwd but predates the launch we're tracking.
        let root = TempDir::new().unwrap();
        let since = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let path = write_session(
            &root,
            "proj",
            "preexisting.jsonl",
            r#"{"type":"user","cwd":"/work/proj","message":{"content":"hi"}}
"#,
        );
        set_mtime(&path, since - Duration::from_secs(1));

        let found = provider(&root).find_new_session(Path::new("/work/proj"), since);
        assert_eq!(found, None);
    }

    #[test]
    fn find_new_session_picks_the_newest_mtime_among_matches() {
        let root = TempDir::new().unwrap();
        let since = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let earlier = write_session(
            &root,
            "proj",
            "earlier.jsonl",
            r#"{"type":"user","cwd":"/work/proj","message":{"content":"hi"}}
"#,
        );
        set_mtime(&earlier, since + Duration::from_secs(1));
        let later = write_session(
            &root,
            "proj",
            "later.jsonl",
            r#"{"type":"user","cwd":"/work/proj","message":{"content":"hi"}}
"#,
        );
        set_mtime(&later, since + Duration::from_secs(2));

        let found = provider(&root).find_new_session(Path::new("/work/proj"), since);
        assert_eq!(found, Some(SessionId("later".to_string())));
    }

    #[test]
    fn find_new_session_breaks_a_tied_mtime_deterministically_by_id() {
        // `find_new_session` is a default method built on `find_new_sessions`
        // (which already breaks ties by id) rather than its own independent
        // walk that used to keep whichever candidate `read_dir` happened to
        // yield last on a tie — i.e. nondeterministically. The
        // lexicographically greatest id must now win regardless of
        // creation/write order.
        let root = TempDir::new().unwrap();
        let since = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let tie_time = since + Duration::from_secs(1);

        // Write the lexicographically *later* id first, so an
        // iteration/creation-order-dependent implementation would be likely
        // to pick the wrong one.
        let later = write_session(
            &root,
            "proj",
            "zzz.jsonl",
            r#"{"type":"user","cwd":"/work/proj","message":{"content":"hi"}}
"#,
        );
        set_mtime(&later, tie_time);
        let earlier = write_session(
            &root,
            "proj",
            "aaa.jsonl",
            r#"{"type":"user","cwd":"/work/proj","message":{"content":"hi"}}
"#,
        );
        set_mtime(&earlier, tie_time);

        let found = provider(&root).find_new_session(Path::new("/work/proj"), since);
        assert_eq!(found, Some(SessionId("zzz".to_string())));
    }

    #[test]
    fn find_new_sessions_returns_every_match_oldest_first() {
        // The scenario find_new_session alone can't handle: several new
        // sessions launched into the same cwd around the same instant (e.g.
        // auto-spawned brigade Workers).
        let root = TempDir::new().unwrap();
        let since = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let first = write_session(
            &root,
            "proj",
            "worker-1.jsonl",
            r#"{"type":"user","cwd":"/work/proj","message":{"content":"hi"}}
"#,
        );
        set_mtime(&first, since + Duration::from_secs(1));
        let second = write_session(
            &root,
            "proj",
            "worker-2.jsonl",
            r#"{"type":"user","cwd":"/work/proj","message":{"content":"hi"}}
"#,
        );
        set_mtime(&second, since + Duration::from_secs(2));

        let found = provider(&root).find_new_sessions(Path::new("/work/proj"), since);
        assert_eq!(
            found,
            vec![
                SessionId("worker-1".to_string()),
                SessionId("worker-2".to_string())
            ]
        );
    }

    #[test]
    fn find_new_sessions_ignores_a_different_cwd_and_a_file_older_than_since() {
        let root = TempDir::new().unwrap();
        let since = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let other_cwd = write_session(
            &root,
            "proj",
            "other.jsonl",
            r#"{"type":"user","cwd":"/somewhere/else","message":{"content":"hi"}}
"#,
        );
        set_mtime(&other_cwd, since + Duration::from_secs(1));
        let too_old = write_session(
            &root,
            "proj",
            "preexisting.jsonl",
            r#"{"type":"user","cwd":"/work/proj","message":{"content":"hi"}}
"#,
        );
        set_mtime(&too_old, since - Duration::from_secs(1));

        assert!(
            provider(&root)
                .find_new_sessions(Path::new("/work/proj"), since)
                .is_empty()
        );
    }

    /// A transcript head naming `cwd`, with the path JSON-escaped the way Claude
    /// writes it (backslashes on Windows).
    fn head_naming(cwd: &Path) -> String {
        format!(
            "{}
",
            serde_json::json!({"type": "user", "cwd": cwd, "message": {"content": "hi"}})
        )
    }

    /// Claude resolves a symlink before it records the cwd, so a pane banto
    /// started through one reports the target.
    #[test]
    fn find_new_sessions_matches_a_cwd_reached_through_a_symlink() {
        let root = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        let real = work.path().join("real");
        let link = work.path().join("link");
        fs::create_dir(&real).unwrap();
        fs::create_dir(work.path().join("elsewhere")).unwrap();
        if !crate::file_identity::test_support::symlink_dir(&real, &link) {
            return;
        }
        let since = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        // What Claude writes is the resolved path, which is not `real` where
        // the temp dir itself is reached through a link or a short name.
        let resolved = real.canonicalize().unwrap();
        let path = write_session(&root, "proj", "new.jsonl", &head_naming(&resolved));
        set_mtime(&path, since + Duration::from_secs(1));

        assert_eq!(
            provider(&root).find_new_sessions(&link, since),
            vec![SessionId("new".to_string())]
        );
        assert!(
            provider(&root)
                .find_new_sessions(&work.path().join("elsewhere"), since)
                .is_empty()
        );
    }

    /// Claude records the cwd in the case it was typed.
    #[test]
    fn find_new_sessions_matches_a_cwd_in_another_letter_case() {
        let root = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        let real = work.path().join("Proj");
        fs::create_dir(&real).unwrap();
        let Some(recased) = crate::file_identity::test_support::recased(&real) else {
            return;
        };
        let since = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let path = write_session(&root, "proj", "new.jsonl", &head_naming(&recased));
        set_mtime(&path, since + Duration::from_secs(1));

        assert_eq!(
            provider(&root).find_new_sessions(&real, since),
            vec![SessionId("new".to_string())]
        );
    }

    #[test]
    fn find_new_sessions_with_missing_projects_dir_yields_empty() {
        let root = TempDir::new().unwrap();
        assert!(
            provider(&root)
                .find_new_sessions(Path::new("/work/proj"), SystemTime::now())
                .is_empty()
        );
    }

    #[test]
    fn find_new_session_with_missing_projects_dir_yields_none() {
        let root = TempDir::new().unwrap();
        let found = provider(&root).find_new_session(Path::new("/work/proj"), SystemTime::now());
        assert_eq!(found, None);
    }

    #[test]
    fn find_new_session_skips_a_broken_file_among_candidates() {
        let root = TempDir::new().unwrap();
        let since = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let broken = write_session(&root, "proj", "broken.jsonl", "not json at all\n");
        set_mtime(&broken, since + Duration::from_secs(1));
        let good = write_session(
            &root,
            "proj",
            "good.jsonl",
            r#"{"type":"user","cwd":"/work/proj","message":{"content":"hi"}}
"#,
        );
        set_mtime(&good, since + Duration::from_secs(2));

        let found = provider(&root).find_new_session(Path::new("/work/proj"), since);
        assert_eq!(found, Some(SessionId("good".to_string())));
    }

    /// Pads `content` with a filler `user` record whose message is a long
    /// run of 'x's, large enough to push the file past `HEAD_CAP_BYTES` (so
    /// the tail-read path engages), followed by `content`.
    fn padded_past_head_cap(content: &str) -> String {
        let filler_chars = HEAD_CAP_BYTES as usize + 4096;
        format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":\"{}\"}}}}\n{}",
            "x".repeat(filler_chars),
            content
        )
    }

    #[test]
    fn title_at_head_only_is_unaffected_by_the_tail_read() {
        // A small file never engages the tail-read path at all; a rename
        // captured in the head is unaffected.
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            "{\"type\":\"custom-title\",\"customTitle\":\"head title\"}\n",
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].title.as_deref(), Some("head title"));
    }

    #[test]
    fn a_rename_appended_beyond_the_head_chunk_boundary_still_wins() {
        // The bug this fixes: a mid-session rename appends its custom-title
        // record far past the head cap (this mirrors what was found in a
        // real session file: a rename ~4.8MB into a 17MB session).
        let root = TempDir::new().unwrap();
        let content =
            padded_past_head_cap("{\"type\":\"custom-title\",\"customTitle\":\"renamed later\"}\n");
        assert!(
            content.len() as u64 > HEAD_CAP_BYTES,
            "fixture must exceed the head cap"
        );
        write_session(&root, "proj", "s1.jsonl", &content);

        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].title.as_deref(), Some("renamed later"));
    }

    #[test]
    fn multiple_custom_titles_past_the_head_cap_the_latest_wins() {
        let root = TempDir::new().unwrap();
        let content = padded_past_head_cap(
            "{\"type\":\"custom-title\",\"customTitle\":\"first rename\"}\n\
             {\"type\":\"custom-title\",\"customTitle\":\"second rename\"}\n",
        );
        write_session(&root, "proj", "s1.jsonl", &content);

        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].title.as_deref(), Some("second rename"));
    }

    #[test]
    fn garbage_lines_past_the_head_cap_are_skipped_when_finding_the_latest_title() {
        let root = TempDir::new().unwrap();
        let content = padded_past_head_cap(
            "not json at all\n\
             {\"type\":\"custom-title\"}\n\
             {\"type\":\"custom-title\",\"customTitle\":\"survives the garbage\"}\n\
             {truncated\n",
        );
        write_session(&root, "proj", "s1.jsonl", &content);

        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].title.as_deref(), Some("survives the garbage"));
    }

    #[test]
    fn a_large_file_without_a_later_rename_keeps_the_head_title() {
        // The tail read finding nothing (no custom-title record within its
        // window) must not clobber a title already found in the head.
        let root = TempDir::new().unwrap();
        let filler_chars = HEAD_CAP_BYTES as usize + 4096;
        let content = format!(
            "{{\"type\":\"custom-title\",\"customTitle\":\"only title\"}}\n\
             {{\"type\":\"user\",\"message\":{{\"content\":\"{}\"}}}}\n",
            "x".repeat(filler_chars)
        );
        assert!(
            content.len() as u64 > HEAD_CAP_BYTES,
            "fixture must exceed the head cap"
        );
        write_session(&root, "proj", "s1.jsonl", &content);

        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].title.as_deref(), Some("only title"));
    }

    // --- an ai-title past the head (see `HEAD_CAP_BYTES`) -------------------

    const FIRST_LINE: &str = "{\"type\":\"user\",\"message\":{\"content\":\"first line\"}}\n";

    /// One record line of exactly `len` bytes, its newline included, of the
    /// kind (`attachment`) that sits between a session's start and its title.
    fn filler_line(len: usize) -> String {
        let frame = "{\"type\":\"attachment\",\"data\":\"\"}\n".len();
        format!(
            "{{\"type\":\"attachment\",\"data\":\"{}\"}}\n",
            "x".repeat(len - frame)
        )
    }

    fn ai_title_record(title: &str) -> String {
        format!("{{\"type\":\"ai-title\",\"aiTitle\":\"{title}\",\"sessionId\":\"s1\"}}\n")
    }

    /// The first user line, one filler record `gap` bytes long, then `after`.
    /// With `gap` over the head cap the head keeps only the first line, so the
    /// record that follows the filler starts exactly `gap` bytes past where the
    /// scan begins.
    fn session_with_gap(gap: usize, after: &str) -> String {
        format!("{FIRST_LINE}{}{after}", filler_line(gap))
    }

    fn only_session(root: &TempDir, content: &str) -> SessionMeta {
        write_session(root, "proj", "s1.jsonl", content);
        let mut sessions = discover_sorted(root);
        assert_eq!(sessions.len(), 1);
        sessions.remove(0)
    }

    #[test]
    fn an_ai_title_past_the_head_is_found() {
        let root = TempDir::new().unwrap();
        let content = session_with_gap(
            HEAD_CAP_BYTES as usize + 4096,
            &format!(
                "{}{}",
                ai_title_record("Claude's title"),
                filler_line(70 * 1024)
            ),
        );
        let meta = only_session(&root, &content);

        assert_eq!(meta.title.as_deref(), Some("Claude's title"));
        assert!(
            !meta.title_is_custom,
            "a title Claude derived is not one the operator gave"
        );
        // The first user line is still the preview, whatever names the row.
        assert_eq!(meta.preview.as_deref(), Some("first line"));
    }

    #[test]
    fn an_ai_title_starting_inside_the_scan_cap_is_found_and_one_at_the_cap_is_not() {
        // The head keeps `FIRST_LINE` alone (the filler is one line longer than
        // the cap), so the record after the filler starts exactly `gap` bytes
        // into the window.
        let cap = AI_TITLE_SCAN_CAP_BYTES as usize;
        let inside = only_session(
            &TempDir::new().unwrap(),
            &session_with_gap(cap - 1, &ai_title_record("inside")),
        );
        assert_eq!(inside.title.as_deref(), Some("inside"));

        let at_cap = only_session(
            &TempDir::new().unwrap(),
            &session_with_gap(cap, &ai_title_record("beyond")),
        );
        assert_eq!(
            at_cap.title.as_deref(),
            Some("first line"),
            "a record starting at the cap is outside the window"
        );
    }

    #[test]
    fn an_ai_title_far_past_the_scan_cap_is_not_looked_for() {
        let root = TempDir::new().unwrap();
        let meta = only_session(
            &root,
            &session_with_gap(
                AI_TITLE_SCAN_CAP_BYTES as usize + 1024 * 1024,
                &ai_title_record("too far"),
            ),
        );
        assert_eq!(meta.title.as_deref(), Some("first line"));
    }

    #[test]
    fn a_custom_title_still_wins_over_an_ai_title_past_the_head() {
        // Renamed later: the custom-title is in the tail, the ai-title between.
        let root = TempDir::new().unwrap();
        let later = only_session(
            &root,
            &session_with_gap(
                HEAD_CAP_BYTES as usize + 4096,
                &format!(
                    "{}{}",
                    ai_title_record("derived"),
                    "{\"type\":\"custom-title\",\"customTitle\":\"given\"}\n"
                ),
            ),
        );
        assert_eq!(later.title.as_deref(), Some("given"));
        assert!(later.title_is_custom);

        // Renamed at the start: the head has it and nothing is scanned for.
        let early = only_session(
            &TempDir::new().unwrap(),
            &format!(
                "{{\"type\":\"custom-title\",\"customTitle\":\"given early\"}}\n{}",
                session_with_gap(HEAD_CAP_BYTES as usize + 4096, &ai_title_record("derived"))
            ),
        );
        assert_eq!(early.title.as_deref(), Some("given early"));
        assert!(early.title_is_custom);
    }

    #[test]
    fn a_large_file_with_no_ai_title_falls_back_to_the_first_user_line() {
        let root = TempDir::new().unwrap();
        let meta = only_session(
            &root,
            &session_with_gap(HEAD_CAP_BYTES as usize + 4096, &filler_line(4096)),
        );
        assert_eq!(meta.title.as_deref(), Some("first line"));
        assert!(!meta.title_is_custom);
    }

    #[test]
    fn the_head_is_cut_on_a_line_boundary_and_says_how_much_it_kept() {
        let root = TempDir::new().unwrap();
        // 100-byte lines whose 100th does not fit the cap evenly, so the cut
        // falls inside one.
        let line = filler_line(100);
        let content = line.repeat(10) + &filler_line(500);
        let path = write_session(&root, "proj", "cut.jsonl", &content);

        let (head, kept) = read_head(&path, 1050).unwrap();
        assert_eq!(
            kept, 1000,
            "the 11th line is cut by the cap, so it is dropped"
        );
        assert_eq!(head.len() as u64, kept);
        assert!(head.ends_with('\n'));
        assert_eq!(
            content.as_bytes()[kept as usize],
            b'{',
            "the next byte starts a record"
        );

        // A file the cap does not reach is kept whole.
        let (head, kept) = read_head(&path, 1 << 20).unwrap();
        assert_eq!(
            (head.len() as u64, kept),
            (content.len() as u64, content.len() as u64)
        );

        // No newline inside the cap: nothing is kept, and the scan starts at 0.
        let (head, kept) = read_head(&path, 50).unwrap();
        assert_eq!((head.as_str(), kept), ("", 0));
    }

    #[test]
    fn a_record_straddling_the_head_cap_is_found_whole_by_the_scan() {
        // The head cuts this record in two, so the head parse never sees it;
        // the scan must begin where the head's text ended, i.e. at the start of
        // this very record, and read it whole. Padded lines of 100 bytes put
        // its start 50 bytes inside the cap and its end past it.
        let root = TempDir::new().unwrap();
        let before = filler_line(100).repeat((HEAD_CAP_BYTES as usize - 50) / 100);
        let straddling = format!(
            "{{\"type\":\"ai-title\",\"aiTitle\":\"straddles the cap\",\"pad\":\"{}\"}}\n",
            "p".repeat(200)
        );
        let content = format!("{before}{straddling}{}", filler_line(4096));
        let start = before.len() as u64;
        assert!(start < HEAD_CAP_BYTES && start + straddling.len() as u64 > HEAD_CAP_BYTES);
        let meta = only_session(&root, &content);

        assert_eq!(meta.title.as_deref(), Some("straddles the cap"));
    }

    #[test]
    fn text_that_quotes_an_ai_title_record_inside_a_json_string_never_matches() {
        // A newline inside a JSON string is the two characters `\n`, never a
        // raw line break, so a record's text quoted by another record cannot
        // begin a line. Three shapes of it, all past the head: a well-formed
        // string, a line broken by an unescaped quote, and a quotation cut
        // short.
        let root = TempDir::new().unwrap();
        let quoted = concat!(
            "{\"type\":\"user\",\"message\":{\"content\":\"x\\n{\\\"type\\\":\\\"ai-title\\\",\\\"aiTitle\\\":\\\"fake one\\\"}\"}}\n",
            "{\"type\":\"user\",\"message\":\"x\\n{\"type\":\"ai-title\",\"aiTitle\":\"fake two\"}\n",
            "{\"type\":\"attachment\",\"data\":\"a\\n{\\\"type\\\":\\\"ai-title\\\"\"}\n",
        );
        let meta = only_session(
            &root,
            &session_with_gap(HEAD_CAP_BYTES as usize + 4096, quoted),
        );
        assert_eq!(meta.title.as_deref(), Some("first line"));

        // And the real record after them is still the one found.
        let meta = only_session(
            &TempDir::new().unwrap(),
            &session_with_gap(
                HEAD_CAP_BYTES as usize + 4096,
                &format!("{quoted}{}", ai_title_record("the real one")),
            ),
        );
        assert_eq!(meta.title.as_deref(), Some("the real one"));
    }

    #[test]
    fn a_line_that_begins_like_an_ai_title_but_is_broken_is_skipped() {
        let root = TempDir::new().unwrap();
        let meta = only_session(
            &root,
            &session_with_gap(
                HEAD_CAP_BYTES as usize + 4096,
                &format!(
                    "{{\"type\":\"ai-title\",\"aiTitle\":\n{{\"type\":\"ai-title\"}}\n{}",
                    ai_title_record("after the broken ones")
                ),
            ),
        );
        assert_eq!(meta.title.as_deref(), Some("after the broken ones"));
    }

    #[test]
    fn a_last_line_still_being_written_does_not_make_a_title() {
        // The file ends inside an ai-title record, as a writer's append would
        // leave it for a moment; the next walk, with a changed size, reads it.
        let root = TempDir::new().unwrap();
        let unfinished = "{\"type\":\"ai-title\",\"aiTitle\":\"half";
        let meta = only_session(
            &root,
            &session_with_gap(HEAD_CAP_BYTES as usize + 4096, unfinished),
        );
        assert_eq!(meta.title.as_deref(), Some("first line"));
    }

    #[test]
    fn a_record_that_begins_like_an_ai_title_but_runs_on_for_ever_is_not_followed() {
        let root = TempDir::new().unwrap();
        let huge = format!(
            "{{\"type\":\"ai-title\",\"aiTitle\":\"too long a line\",\"pad\":\"{}\"}}\n",
            "p".repeat(AI_TITLE_LINE_MAX_BYTES + 1)
        );
        let meta = only_session(
            &root,
            &session_with_gap(HEAD_CAP_BYTES as usize + 4096, &huge),
        );
        assert_eq!(meta.title.as_deref(), Some("first line"));
    }

    #[test]
    fn a_line_over_the_limit_is_skipped_and_still_counts_toward_the_window() {
        // The skip over the rest of such a line must add what it passed to the
        // offset, or a record behind it would be judged against the wrong end
        // of the window.
        let cap = AI_TITLE_SCAN_CAP_BYTES as usize;
        let gap = HEAD_CAP_BYTES as usize + 4096;
        let giant = |len: usize| {
            let (open, close) = ("{\"type\":\"ai-title\",\"pad\":\"", "\"}\n");
            format!(
                "{open}{}{close}",
                "p".repeat(len - open.len() - close.len())
            )
        };
        for (record_start, expected) in [(cap - 1, "found"), (cap, "first line")] {
            let content = session_with_gap(
                gap,
                &format!("{}{}", giant(record_start - gap), ai_title_record("found")),
            );
            let meta = only_session(&TempDir::new().unwrap(), &content);
            assert_eq!(meta.title.as_deref(), Some(expected), "at {record_start}");
        }
    }

    // `title_and_source` skips a custom-title that normalizes to nothing, so
    // the ai-title past the head is the title and must be looked for.

    #[test]
    fn a_blank_custom_title_in_the_head_does_not_stop_the_search_for_an_ai_title() {
        let content = format!(
            "{{\"type\":\"custom-title\",\"customTitle\":\"\"}}\n{}",
            session_with_gap(
                HEAD_CAP_BYTES as usize + 4096,
                &ai_title_record("Claude's title")
            )
        );
        let meta = only_session(&TempDir::new().unwrap(), &content);
        assert_eq!(meta.title.as_deref(), Some("Claude's title"));
        assert!(!meta.title_is_custom);
    }

    #[test]
    fn a_rename_to_nothing_in_the_tail_does_not_stop_the_search_for_an_ai_title() {
        // The operator cleared the title after Claude had written one.
        let content = session_with_gap(
            HEAD_CAP_BYTES as usize + 4096,
            &format!(
                "{}{}{{\"type\":\"custom-title\",\"customTitle\":\"  \"}}\n",
                ai_title_record("Claude's title"),
                filler_line(70 * 1024),
            ),
        );
        let meta = only_session(&TempDir::new().unwrap(), &content);
        assert_eq!(meta.title.as_deref(), Some("Claude's title"));
        assert!(!meta.title_is_custom);
    }

    #[test]
    fn a_record_exactly_as_long_as_the_limit_is_parsed_and_one_byte_longer_is_not() {
        let line = |len: usize| {
            let (open, close) = (
                "{\"type\":\"ai-title\",\"aiTitle\":\"exact\",\"pad\":\"",
                "\"}\n",
            );
            format!(
                "{open}{}{close}",
                "p".repeat(len - open.len() - close.len())
            )
        };
        // The limit counts the line without its newline.
        for (len, expected) in [
            (AI_TITLE_LINE_MAX_BYTES + 1, "exact"),
            (AI_TITLE_LINE_MAX_BYTES + 2, "first line"),
        ] {
            let content = session_with_gap(HEAD_CAP_BYTES as usize + 4096, &line(len));
            let meta = only_session(&TempDir::new().unwrap(), &content);
            assert_eq!(meta.title.as_deref(), Some(expected), "{len} bytes");
        }
    }

    // --- the memoized walk (see `super::super::cache`) --------------------
    //
    // The cache's own tests drive it with closures; these drive it through
    // real files, which is the only place the two halves of the split
    // (`stat_session` forming a key, `parse_session` deciding whether the
    // answer is one to keep) meet a filesystem.

    fn discover_cached_sorted(root: &TempDir, cache: &MetaCache) -> Vec<SessionMeta> {
        let mut sessions = provider(root).discover_cached(cache).unwrap();
        sessions.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        sessions
    }

    #[test]
    fn a_cached_walk_returns_what_an_uncached_one_does() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            "{\"type\":\"custom-title\",\"customTitle\":\"one\",\"cwd\":\"/tmp/one\"}\n",
        );
        write_session(
            &root,
            "other",
            "s2.jsonl",
            "{\"type\":\"user\",\"message\":{\"content\":\"hello\"}}\n",
        );

        let cache = MetaCache::quiet_for_test();
        let uncached = discover_sorted(&root);
        let first = discover_cached_sorted(&root, &cache);
        let second = discover_cached_sorted(&root, &cache);

        assert_eq!(first, uncached);
        assert_eq!(second, uncached, "the second walk is the memoized one");
    }

    /// A session gaining a record — the ordinary case, and the one the memo
    /// must never answer for. The appended record is a `custom-title` over a
    /// session that had only a user message to be named by, because that is
    /// what a second walk can actually be seen to have re-read: within the
    /// head, an *earlier* custom-title already wins over a later one.
    #[test]
    fn an_appended_session_is_read_again_rather_than_remembered() {
        let root = TempDir::new().unwrap();
        let first_line = "{\"type\":\"user\",\"message\":{\"content\":\"hello\"}}\n";
        let path = write_session(&root, "proj", "s1.jsonl", first_line);

        let cache = MetaCache::quiet_for_test();
        assert_eq!(
            discover_cached_sorted(&root, &cache)[0].title.as_deref(),
            Some("hello")
        );

        fs::write(
            &path,
            format!("{first_line}{{\"type\":\"custom-title\",\"customTitle\":\"renamed\"}}\n"),
        )
        .unwrap();

        assert_eq!(
            discover_cached_sorted(&root, &cache)[0].title.as_deref(),
            Some("renamed")
        );
    }

    #[test]
    fn a_session_that_disappears_is_not_answered_from_the_cache() {
        let root = TempDir::new().unwrap();
        let path = write_session(
            &root,
            "proj",
            "s1.jsonl",
            "{\"type\":\"custom-title\",\"customTitle\":\"one\"}\n",
        );

        let cache = MetaCache::quiet_for_test();
        assert_eq!(discover_cached_sorted(&root, &cache).len(), 1);

        fs::remove_file(&path).unwrap();
        assert!(discover_cached_sorted(&root, &cache).is_empty());
    }

    /// A file rewritten to a different length under a preserved mtime — the
    /// half of the key that catches a same-length rewrite is not the half
    /// doing the work here. Uses the same `set_mtime` helper the mtime tests
    /// above use, because a plain rewrite would move both.
    #[test]
    fn a_rewrite_that_only_moves_the_size_is_still_noticed() {
        let root = TempDir::new().unwrap();
        let path = write_session(
            &root,
            "proj",
            "s1.jsonl",
            "{\"type\":\"custom-title\",\"customTitle\":\"one\"}\n",
        );
        let frozen = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        set_mtime(&path, frozen);

        let cache = MetaCache::quiet_for_test();
        assert_eq!(
            discover_cached_sorted(&root, &cache)[0].title.as_deref(),
            Some("one")
        );

        fs::write(
            &path,
            "{\"type\":\"custom-title\",\"customTitle\":\"one and a half\"}\n",
        )
        .unwrap();
        set_mtime(&path, frozen);

        assert_eq!(
            discover_cached_sorted(&root, &cache)[0].title.as_deref(),
            Some("one and a half")
        );
    }
}
