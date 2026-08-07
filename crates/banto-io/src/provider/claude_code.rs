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
use banto_core::model::{AgentKind, SessionId, SessionMeta};

/// Maximum number of bytes read from the head of each session file.
/// Titles and cwd appear in the first few records, so this is plenty.
const HEAD_CAP_BYTES: u64 = 256 * 1024;

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

    fn find_new_sessions(&self, cwd: &Path, since: SystemTime) -> Vec<SessionId> {
        let projects = self.claude_home.projects_dir();
        let Ok(entries) = fs::read_dir(&projects) else {
            return Vec::new();
        };

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
                let Ok(head) = read_head(&path, HEAD_CAP_BYTES) else {
                    continue;
                };
                let fields = extract_head_fields(&head);
                if fields.cwd.as_deref() != Some(cwd) {
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
    let Ok(head) = read_head(path, HEAD_CAP_BYTES) else {
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
    }
    let title = fields.title();
    let preview = fields.preview();
    let meta = Some(SessionMeta {
        id: SessionId(id.to_string()),
        agent: AgentKind::ClaudeCode,
        title,
        cwd: fields.cwd,
        source_path: path.to_path_buf(),
        mtime,
        size,
        is_agent: fields.is_agent,
        preview,
        continuation_of_uuid: fields.continuation_of_uuid,
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

/// Read at most `cap` bytes from the head of `path` as lossy UTF-8.
///
/// If the cap is hit, everything after the last newline is dropped so that
/// a truncated final line is never parsed.
fn read_head(path: &Path, cap: u64) -> std::io::Result<String> {
    let file = File::open(path)?;
    let mut buf = Vec::new();
    file.take(cap).read_to_end(&mut buf)?;
    if buf.len() as u64 == cap {
        match buf.iter().rposition(|&b| b == b'\n') {
            Some(pos) => buf.truncate(pos + 1),
            None => buf.clear(),
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
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
    /// `logicalParentUuid` of the first `compact_boundary` record seen, if
    /// any (see `SessionMeta::continuation_of_uuid`).
    continuation_of_uuid: Option<String>,
}

impl HeadFields {
    /// Pick the title by priority: custom-title > ai-title > first user
    /// message. Candidates that normalize to an empty string are skipped.
    fn title(&self) -> Option<String> {
        [&self.custom_title, &self.ai_title, &self.user_text]
            .into_iter()
            .flatten()
            .find_map(|raw| normalize_single_line(raw, TITLE_MAX_CHARS))
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
            Some("user") if fields.user_text.is_none() => {
                fields.user_text = message_text(&record);
            }
            // An auto-compaction continuation's head carries this record,
            // which is also the record that sets `cwd` for such a file (no
            // earlier record in a continuation's head does) — so it is
            // always processed before the break below can fire, even though
            // the break does not itself check `continuation_of_uuid`.
            Some("system")
                if fields.continuation_of_uuid.is_none()
                    && record.get("subtype").and_then(Value::as_str)
                        == Some("compact_boundary") =>
            {
                fields.continuation_of_uuid = record
                    .get("logicalParentUuid")
                    .and_then(Value::as_str)
                    .map(str::to_string);
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

    #[test]
    fn compact_boundary_head_sets_continuation_of_uuid() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"custom-title","customTitle":"Custom title"}
{"type":"mode","mode":"default"}
{"type":"system","subtype":"compact_boundary","logicalParentUuid":"94cc49bf-5dc1-4930-a942-7f3413594b86","cwd":"/tmp/proj","compactMetadata":{"trigger":"auto"}}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(
            sessions[0].continuation_of_uuid.as_deref(),
            Some("94cc49bf-5dc1-4930-a942-7f3413594b86")
        );
    }

    #[test]
    fn ordinary_session_has_no_continuation_of_uuid() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"mode","mode":"default","cwd":"/tmp/proj"}
{"type":"user","message":{"content":"hello"}}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].continuation_of_uuid, None);
    }

    #[test]
    fn a_system_record_with_a_different_subtype_is_not_mistaken_for_a_continuation() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"system","subtype":"other","logicalParentUuid":"should-be-ignored","cwd":"/tmp/proj"}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].continuation_of_uuid, None);
    }

    #[test]
    fn a_compact_boundary_missing_logical_parent_uuid_is_tolerated() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"system","subtype":"compact_boundary","cwd":"/tmp/proj"}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].continuation_of_uuid, None);
    }

    #[test]
    fn only_the_first_compact_boundary_uuid_is_captured() {
        let root = TempDir::new().unwrap();
        write_session(
            &root,
            "proj",
            "s1.jsonl",
            r#"{"type":"system","subtype":"compact_boundary","logicalParentUuid":"first","cwd":"/tmp/proj"}
{"type":"system","subtype":"compact_boundary","logicalParentUuid":"second"}
"#,
        );
        let sessions = discover_sorted(&root);
        assert_eq!(sessions[0].continuation_of_uuid.as_deref(), Some("first"));
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
        // by a title record: the title is out of reach, but the session is
        // still listed with its fs metadata.
        let mut content = "x".repeat(300 * 1024);
        content.push_str("\n{\"type\":\"ai-title\",\"aiTitle\":\"unreachable\"}\n");
        write_session(&root, "proj", "big.jsonl", &content);
        let sessions = discover_sorted(&root);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, None);
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

        let cache = MetaCache::new();
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

        let cache = MetaCache::new();
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

        let cache = MetaCache::new();
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

        let cache = MetaCache::new();
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
