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

use super::{ProviderError, SessionProvider};
use crate::model::{SessionId, SessionMeta};

/// Stable provider name stored in the DB.
const PROVIDER_NAME: &str = "claude-code";

/// Maximum number of bytes read from the head of each session file.
/// Titles and cwd appear in the first few records, so this is plenty.
const HEAD_CAP_BYTES: u64 = 256 * 1024;

/// Maximum title length in characters (not bytes). Also used to cap
/// `SessionMeta.preview` (see [`HeadFields::preview`]).
const TITLE_MAX_CHARS: usize = 200;

/// Session provider for the Claude Code CLI.
///
/// Reads session `.jsonl` files under `<claude_home>/projects/`. Everything
/// under `claude_home` is treated as strictly read-only.
pub struct ClaudeCodeProvider {
    claude_home: PathBuf,
}

impl ClaudeCodeProvider {
    /// Create a provider rooted at `claude_home`, the directory that
    /// contains `projects/` (normally `~/.claude`).
    pub fn new(claude_home: PathBuf) -> Self {
        Self { claude_home }
    }

    /// Default Claude Code home: `~/.claude`, if a home directory exists.
    pub fn default_home() -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(".claude"))
    }

    /// Find the session Claude Code most recently created or updated for
    /// `cwd` at or after `since`.
    ///
    /// Used to discover the session id assigned to a `claude` process that
    /// was launched without a known session id (e.g. banto's "new session"
    /// flow). Matching is done on the `cwd` recorded *inside* each candidate
    /// file's head record (the same field `read_session` extracts), not by
    /// decoding the project directory name: Claude's cwd-to-directory-name
    /// encoding is lossy (`:`, `\`, `/`, and `.` all collapse to `-`), so a
    /// renamed project can leave files with different recorded cwd values
    /// in the same directory.
    ///
    /// `since` is compared against each file's filesystem mtime, which can
    /// have coarse resolution; callers should capture `since` a moment
    /// *before* launching the new session so a file created in the same
    /// clock tick is not missed.
    ///
    /// Read-only like the rest of this provider: unreadable or broken files
    /// are skipped, and a missing `projects` directory yields `None`. When
    /// several candidates match, the one with the greatest mtime wins.
    pub fn find_new_session(&self, cwd: &Path, since: SystemTime) -> Option<SessionId> {
        let projects = self.claude_home.join("projects");
        let entries = fs::read_dir(&projects).ok()?;

        let mut best: Option<(SystemTime, SessionId)> = None;
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
                if best
                    .as_ref()
                    .is_none_or(|(best_mtime, _)| mtime > *best_mtime)
                {
                    best = Some((mtime, SessionId(id.to_string())));
                }
            }
        }
        best.map(|(_, id)| id)
    }
}

impl SessionProvider for ClaudeCodeProvider {
    fn name(&self) -> &'static str {
        PROVIDER_NAME
    }

    fn discover(&self) -> Result<Vec<SessionMeta>, ProviderError> {
        let projects = self.claude_home.join("projects");
        let entries = match fs::read_dir(&projects) {
            Ok(entries) => entries,
            // A Claude home without a projects dir simply has no sessions.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(ProviderError::Io(err)),
        };

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
                if let Some(meta) = read_session(&file.path()) {
                    sessions.push(meta);
                }
            }
        }
        Ok(sessions)
    }
}

/// Build a [`SessionMeta`] for one candidate path, or `None` if the entry
/// is not a readable, non-empty `.jsonl` file.
fn read_session(path: &Path) -> Option<SessionMeta> {
    if !path.extension().is_some_and(|ext| ext == "jsonl") {
        return None;
    }
    if !path.is_file() {
        return None;
    }
    let id = path.file_stem()?.to_str()?.to_string();
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() == 0 {
        return None;
    }
    let mtime = metadata.modified().ok()?;
    let head = read_head(path, HEAD_CAP_BYTES).ok()?;
    let fields = extract_head_fields(&head);
    let title = fields.title();
    let preview = fields.preview();
    Some(SessionMeta {
        id: SessionId(id),
        provider: PROVIDER_NAME.to_string(),
        title,
        cwd: fields.cwd,
        source_path: path.to_path_buf(),
        mtime,
        size: metadata.len(),
        is_agent: fields.is_agent,
        preview,
    })
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
        ClaudeCodeProvider::new(root.path().to_path_buf())
    }

    /// Set a file's mtime deterministically (`File::set_modified`, stable
    /// since Rust 1.75), matching the pattern used in `banto::session`.
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
        assert_eq!(meta.provider, "claude-code");
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
    fn default_home_ends_with_dot_claude() {
        // Only inspects the constructed path; never reads the real ~/.claude.
        if let Some(home) = ClaudeCodeProvider::default_home() {
            assert!(home.ends_with(".claude"));
        }
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
}
