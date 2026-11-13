//! An opt-in, append-only text log of what the agents of a brigade told each
//! other, for someone developing banto who wants to audit it (`[brigade]
//! message_log` in config.toml). The file is the whole feature: nothing in banto
//! reads it back, rotates it or deletes it.
//!
//! One file per brigade, `brigade-<id>.log` under a directory the caller names
//! (in production `DataDir::logs_dir` — inside banto's own data directory, the
//! location deliberately not configurable, so the log can never be pointed
//! outside it). Written by the `banto _mcp` processes, one per agent, which
//! share the file. Each entry is handed to the OS whole, in one `write` on an
//! append-only handle; entries could interleave only if the OS accepted that
//! write short and `write_all` needed a second one. Measured on one machine —
//! local NTFS, 16 processes at once, entries up to 1 MB — that never happened.
//! No lock is taken, as for the other diagnostic logs (`docs/DISCIPLINE.md`
//! §6.2).
//!
//! The body is written verbatim, so a body line can look exactly like a header;
//! the header's `bytes=` says how long the body is, and a reader that skips that
//! many bytes finds the next real header whatever the body contains.
//!
//! The file holds real conversation content.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use banto_core::model::{BrigadeId, BrigadeRole};

/// Every entry's header starts with this: easy to grep, hard to mistake for body
/// text at a glance. A body may still contain a line that starts with it —
/// `bytes=` in the header is what tells a reader where the body ends.
pub const ENTRY_MARKER: &str = "@@@";

/// A member as the header names it: its token (the address), and its display
/// name when it has one and the caller had it to hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Party<'a> {
    pub token: &'a str,
    pub name: Option<&'a str>,
}

/// Who an entry was sent to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recipient<'a> {
    /// One member, by token.
    Member(Party<'a>),
    /// A broadcast: every member of this role.
    Role(BrigadeRole),
}

/// What an entry records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A message that went through the brigade queue, under this queue id.
    Message { queue_id: i64 },
    /// A Goinkyo consultation request: a file, not a queue message, so it has no
    /// queue id.
    ConsultationRequest,
}

/// One thing to log.
#[derive(Debug, Clone, Copy)]
pub struct Entry<'a> {
    pub at: SystemTime,
    pub kind: EntryKind,
    pub from: Party<'a>,
    pub to: Recipient<'a>,
    /// Written verbatim; its length in bytes goes in the header as `bytes=`.
    pub body: &'a str,
}

/// The appender: a directory, and nothing opened until an entry arrives.
#[derive(Debug, Clone)]
pub struct BrigadeLog {
    dir: PathBuf,
}

impl BrigadeLog {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `brigade-<id>.log` under the directory. Brigade ids are never reissued
    /// within one database (`brigades.id` is `AUTOINCREMENT`), so a file belongs
    /// to one brigade — unless the database itself was replaced.
    pub fn path_for(&self, brigade: BrigadeId) -> PathBuf {
        self.dir.join(format!("brigade-{brigade}.log"))
    }

    /// Append one entry, creating the directory and file on demand. The entry is
    /// rendered first and handed to the OS in one `write_all` on an append-only
    /// handle: a single `write` unless the OS accepts it short, in which case
    /// `write_all` issues another rather than lose the tail of the entry.
    pub fn append(&self, brigade: BrigadeId, entry: &Entry<'_>) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path_for(brigade))?;
        file.write_all(render_entry(entry).as_bytes())
    }
}

/// The entry as written: a header line, the body verbatim, a blank line.
///
/// ```text
/// @@@ 2026-11-13T09:15:02Z message #42 from=worker-1 "impl" to=director bytes=42
/// the body, as sent
/// over any number of lines
///
/// ```
///
/// `to=role:worker` marks a broadcast; a consultation request reads
/// `@@@ <time> consultation-request from=director to=goinkyo bytes=<n>`.
///
/// `bytes=` is the exact number of bytes in the body as given — nothing the log
/// adds is counted. After the body come always exactly two `\n`, whatever the
/// body ends with: one to end its last line and the blank line. A reader finds
/// the next entry by skipping the header line, `bytes=` bytes and those two.
pub fn render_entry(entry: &Entry<'_>) -> String {
    let kind = match entry.kind {
        EntryKind::Message { queue_id } => format!("message #{queue_id}"),
        EntryKind::ConsultationRequest => "consultation-request".to_string(),
    };
    let to = match entry.to {
        Recipient::Member(party) => party_text(party),
        Recipient::Role(role) => format!("role:{}", role.as_token()),
    };
    format!(
        "{ENTRY_MARKER} {} {kind} from={} to={to} bytes={}\n{}\n\n",
        format_utc(entry.at),
        party_text(entry.from),
        entry.body.len(),
        entry.body
    )
}

/// `worker-3 "impl"` for a named member, `worker-3` for one that is not.
fn party_text(party: Party<'_>) -> String {
    match party.name {
        Some(name) => format!("{} \"{name}\"", party.token),
        None => party.token.to_string(),
    }
}

/// `2026-11-13T09:15:02Z`: UTC, RFC 3339, to the second, whatever the local zone.
fn format_utc(at: SystemTime) -> String {
    let secs = at
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    // Days since 1970-01-01 to a civil date (proleptic Gregorian), after Howard
    // Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn message<'a>(queue_id: i64, from: &'a str, to: Recipient<'a>, body: &'a str) -> Entry<'a> {
        Entry {
            at: at(1_700_000_000),
            kind: EntryKind::Message { queue_id },
            from: Party {
                token: from,
                name: None,
            },
            to,
            body,
        }
    }

    /// A reader that honours `bytes=`: header line, exactly that many body bytes,
    /// then the two newlines — and nothing else, so anything unexpected is an
    /// error rather than a guess. Returns `(header, body)` per entry.
    fn parse_entries(data: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
        let mut entries = Vec::new();
        let mut pos = 0;
        while pos < data.len() {
            if !data[pos..].starts_with(format!("{ENTRY_MARKER} ").as_bytes()) {
                return Err(format!("no header at byte {pos}"));
            }
            let line_end = data[pos..]
                .iter()
                .position(|b| *b == b'\n')
                .map(|i| pos + i)
                .ok_or("header without a line end")?;
            let header =
                String::from_utf8(data[pos..line_end].to_vec()).map_err(|e| e.to_string())?;
            let bytes: usize = header
                .rsplit(' ')
                .next()
                .and_then(|word| word.strip_prefix("bytes="))
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| format!("no bytes= in {header:?}"))?;
            let body_start = line_end + 1;
            let body_end = body_start
                .checked_add(bytes)
                .filter(|end| *end <= data.len())
                .ok_or_else(|| format!("bytes= runs past the end in {header:?}"))?;
            if !data[body_end..].starts_with(b"\n\n") {
                return Err(format!(
                    "entry at byte {pos} does not end where bytes= says"
                ));
            }
            entries.push((header, data[body_start..body_end].to_vec()));
            pos = body_end + 2;
        }
        Ok(entries)
    }

    #[test]
    fn the_timestamp_is_utc_rfc_3339_to_the_second() {
        assert_eq!(format_utc(at(0)), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(at(1_700_000_000)), "2023-11-14T22:13:20Z");
        // A leap day, and the end of a year.
        assert_eq!(format_utc(at(1_709_164_800)), "2024-02-29T00:00:00Z");
        assert_eq!(format_utc(at(1_735_689_599)), "2024-12-31T23:59:59Z");
        // The 400-year rule: 2000 had a Feb 29, 2100 will not.
        assert_eq!(format_utc(at(951_782_400)), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(at(4_107_542_400)), "2100-03-01T00:00:00Z");
    }

    #[test]
    fn an_addressed_message_names_queue_id_sender_recipient_display_names_and_body_length() {
        let entry = Entry {
            from: Party {
                token: "worker-1",
                name: Some("impl"),
            },
            to: Recipient::Member(Party {
                token: "director",
                name: None,
            }),
            ..message(42, "x", Recipient::Role(BrigadeRole::Director), "ok")
        };
        assert_eq!(
            render_entry(&entry),
            "@@@ 2023-11-14T22:13:20Z message #42 from=worker-1 \"impl\" to=director bytes=2\nok\n\n"
        );
    }

    #[test]
    fn a_broadcast_names_the_target_role_and_the_body_is_verbatim() {
        let body = "first line\n\n  indented, with @@@ in it\nlast line";
        let entry = message(7, "director", Recipient::Role(BrigadeRole::Worker), body);
        assert_eq!(
            render_entry(&entry),
            format!(
                "@@@ 2023-11-14T22:13:20Z message #7 from=director to=role:worker bytes={}\n{body}\n\n",
                body.len()
            )
        );
    }

    #[test]
    fn bytes_counts_the_body_as_given_and_exactly_two_newlines_always_follow_it() {
        // However the body ends — nothing, one newline, a blank line — `bytes=`
        // is its own length and the entry ends with the same two newlines.
        for body in ["x", "x\n", "x\n\n", ""] {
            let entry = message(1, "director", Recipient::Role(BrigadeRole::Worker), body);
            let text = render_entry(&entry);
            assert!(
                text.contains(&format!("bytes={}\n", body.len())),
                "{body:?}: {text}"
            );
            assert!(text.ends_with(&format!("{body}\n\n")), "{body:?}: {text:?}");
            let parsed = parse_entries(text.as_bytes()).unwrap();
            assert_eq!(parsed[0].1, body.as_bytes());
        }
        // Multi-byte characters count as bytes, not characters.
        let entry = message(
            1,
            "director",
            Recipient::Role(BrigadeRole::Worker),
            "日本語",
        );
        assert!(render_entry(&entry).contains("bytes=9\n"));
    }

    #[test]
    fn an_empty_body_is_a_zero_length_entry_that_a_reader_still_finds_the_next_after() {
        let dir = TempDir::new().unwrap();
        let log = BrigadeLog::new(dir.path().to_path_buf());
        for (i, body) in ["", "after"].iter().enumerate() {
            let entry = message(
                i as i64,
                "director",
                Recipient::Role(BrigadeRole::Worker),
                body,
            );
            log.append(1, &entry).unwrap();
        }
        let data = fs::read(log.path_for(1)).unwrap();
        let parsed = parse_entries(&data).unwrap();
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].0.ends_with("bytes=0"));
        assert_eq!(parsed[0].1, b"");
        assert_eq!(parsed[1].1, b"after");
    }

    #[test]
    fn crlf_line_endings_in_a_body_are_preserved_byte_for_byte() {
        let dir = TempDir::new().unwrap();
        let log = BrigadeLog::new(dir.path().to_path_buf());
        let body = "one\r\ntwo\r\n\r\nthree\r\n";
        log.append(
            1,
            &message(1, "director", Recipient::Role(BrigadeRole::Worker), body),
        )
        .unwrap();
        let data = fs::read(log.path_for(1)).unwrap();
        // The bytes are on disk exactly as given (no newline translation either way).
        assert!(
            data.windows(body.len()).any(|w| w == body.as_bytes()),
            "{data:?}"
        );
        let parsed = parse_entries(&data).unwrap();
        assert_eq!(parsed[0].1, body.as_bytes());
        assert!(parsed[0].0.ends_with(&format!("bytes={}", body.len())));
    }

    #[test]
    fn a_body_line_that_looks_like_a_header_does_not_fool_a_reader_that_honours_bytes() {
        // Messages about this very feature quote sample entries, header lines
        // included — complete, valid-looking ones, even one with a plausible
        // `bytes=`.
        let fake_header =
            "@@@ 2026-01-01T00:00:00Z message #1 from=director to=worker-1 bytes=3\nfake\n\n";
        let bodies = [
            format!("look at this sample:\n{fake_header}and that was it"),
            fake_header.to_string(),
            "@@@ ".to_string(),
        ];
        let dir = TempDir::new().unwrap();
        let log = BrigadeLog::new(dir.path().to_path_buf());
        for (i, body) in bodies.iter().enumerate() {
            let entry = message(
                i as i64,
                "worker-2",
                Recipient::Role(BrigadeRole::Director),
                body,
            );
            log.append(5, &entry).unwrap();
        }
        let data = fs::read(log.path_for(5)).unwrap();
        // A naive count of header-looking lines is fooled ...
        let naive = String::from_utf8_lossy(&data).matches("\n@@@ ").count() + 1;
        assert!(
            naive > bodies.len(),
            "the samples really do look like headers"
        );
        // ... a reader that skips `bytes=` recovers exactly what was written.
        let parsed = parse_entries(&data).unwrap();
        assert_eq!(parsed.len(), bodies.len());
        for (parsed, body) in parsed.iter().zip(&bodies) {
            assert_eq!(parsed.1, body.as_bytes());
        }
        assert!(
            parsed[0]
                .0
                .contains("message #0 from=worker-2 to=role:director")
        );
    }

    #[test]
    fn a_consultation_request_is_marked_as_one_and_has_no_queue_id() {
        let entry = Entry {
            kind: EntryKind::ConsultationRequest,
            to: Recipient::Member(Party {
                token: "goinkyo",
                name: None,
            }),
            ..message(
                0,
                "director",
                Recipient::Role(BrigadeRole::Goinkyo),
                "Kind: design",
            )
        };
        let text = render_entry(&entry);
        assert!(
            text.starts_with(
                "@@@ 2023-11-14T22:13:20Z consultation-request from=director to=goinkyo bytes=12\n"
            ),
            "{text}"
        );
        assert!(!text.contains('#'));
    }

    #[test]
    fn entries_accumulate_in_one_file_per_brigade_and_the_directory_is_created_on_demand() {
        let dir = TempDir::new().unwrap();
        let logs = dir.path().join("not").join("there").join("yet");
        let log = BrigadeLog::new(logs.clone());
        assert!(!logs.exists(), "nothing is created before the first entry");

        log.append(
            3,
            &message(1, "director", Recipient::Role(BrigadeRole::Worker), "one"),
        )
        .unwrap();
        log.append(
            3,
            &message(2, "worker-1", Recipient::Role(BrigadeRole::Director), "two"),
        )
        .unwrap();
        log.append(
            4,
            &message(1, "director", Recipient::Role(BrigadeRole::Worker), "other"),
        )
        .unwrap();

        let three = parse_entries(&fs::read(log.path_for(3)).unwrap()).unwrap();
        assert_eq!(three.len(), 2);
        assert_eq!(
            (three[0].1.as_slice(), three[1].1.as_slice()),
            (&b"one"[..], &b"two"[..])
        );
        let four = parse_entries(&fs::read(log.path_for(4)).unwrap()).unwrap();
        assert_eq!(four.len(), 1);
        assert!(log.path_for(3).starts_with(&logs));
        assert_eq!(
            log.path_for(3).file_name().unwrap().to_string_lossy(),
            "brigade-3.log"
        );
    }

    #[test]
    fn a_log_that_cannot_be_opened_reports_an_error_instead_of_panicking() {
        let dir = TempDir::new().unwrap();
        // The log directory's path is a file: the directory cannot be created.
        let blocker = dir.path().join("logs");
        fs::write(&blocker, "not a directory").unwrap();
        let log = BrigadeLog::new(blocker);
        assert!(
            log.append(
                1,
                &message(1, "director", Recipient::Role(BrigadeRole::Worker), "x")
            )
            .is_err()
        );
    }

    #[test]
    fn two_appenders_on_the_same_file_produce_whole_entries() {
        let dir = TempDir::new().unwrap();
        let a = BrigadeLog::new(dir.path().to_path_buf());
        let b = BrigadeLog::new(dir.path().to_path_buf());
        // Large enough bodies that an interleaving write would show, and each
        // one starting with a header-looking line.
        let body_a = format!("@@@ looks like a header\n{}", "a".repeat(20_000));
        let body_b = format!("@@@ looks like a header\n{}", "b".repeat(20_000));
        const ROUNDS: usize = 50;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for i in 0..ROUNDS {
                    a.append(
                        9,
                        &message(
                            i as i64,
                            "worker-1",
                            Recipient::Role(BrigadeRole::Director),
                            &body_a,
                        ),
                    )
                    .unwrap();
                }
            });
            scope.spawn(|| {
                for i in 0..ROUNDS {
                    b.append(
                        9,
                        &message(
                            i as i64,
                            "worker-2",
                            Recipient::Role(BrigadeRole::Director),
                            &body_b,
                        ),
                    )
                    .unwrap();
                }
            });
        });

        let parsed = parse_entries(&fs::read(a.path_for(9)).unwrap()).unwrap();
        assert_eq!(parsed.len(), 2 * ROUNDS);
        let mut next = [0usize; 2];
        for (header, body) in parsed {
            let writer = usize::from(header.contains("from=worker-2"));
            let expected = if writer == 0 { &body_a } else { &body_b };
            assert_eq!(
                body,
                expected.as_bytes(),
                "a whole body from one writer: {header}"
            );
            assert!(
                header.contains(&format!("message #{} ", next[writer])),
                "per-writer order: {header}"
            );
            next[writer] += 1;
        }
    }
}
