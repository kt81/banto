//! Tests for the pane lifecycle log's pure lines. Every fixture is synthetic.

use std::path::PathBuf;

use super::*;
use crate::engine::{
    Cmd, GoinkyoGuardFacts, GoinkyoLook, GoinkyoObservation, GoinkyoRow, GoinkyoSpawnCandidate,
    SpawnRefusal, StoreIntent, decide_goinkyo,
};
use crate::input::InputEvent;
use crate::model::{BrigadeRole, PtyExitReason, Quiescence, SessionToOpen, StoredLayout};

/// Text that must never reach a line. It is token-shaped on purpose: a text field
/// passed through `token()` or `id()` by mistake would let it through, and this
/// test is what catches that. Identifiers that the log does write (member tokens,
/// session ids) are given ordinary values below, so the marker can only come
/// from a field the log must not print.
const MARKER: &str = "zqxmarker";

fn marker_pathbuf() -> PathBuf {
    PathBuf::from(MARKER)
}

fn written_text(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|line| line.render(1_000, 5))
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_no_marker(text: &str) {
    assert!(!text.contains(MARKER), "a line leaked the marker: {text}");
}

#[test]
fn every_text_carrying_variant_leaves_its_marker_out() {
    let key = SessionKey::from_id("sess-1");
    let facts = PaneFacts {
        asked: true,
        member: Some((9, "worker-a".to_string())),
        pid: Some(4321),
        was: Some(Quiescence::NotIdle),
        input_ago_ms: Some(1234),
    };
    let text_events = vec![
        Event::Input(InputEvent::Paste(MARKER.to_string())),
        Event::PtyOutput {
            key: key.clone(),
            chunk: MARKER.as_bytes().to_vec(),
        },
        Event::PtyExited {
            key: key.clone(),
            reason: Some(PtyExitReason::Signal("SIGTERM".to_string())),
        },
        Event::PtyOutputLost { key: key.clone() },
        Event::PtyWriteDropped { key: key.clone() },
        Event::Spawned {
            key: key.clone(),
            agent: Some(AgentKind::Codex),
        },
        Event::SpawnFailed {
            key: key.clone(),
            error: MARKER.to_string(),
            refusal: SpawnRefusal::PtyOpen,
            os_error: Some(2),
        },
        Event::DiscoveryResult {
            key: key.clone(),
            session_id: "sess-1".to_string(),
            member: Some((9, "worker-a".to_string())),
        },
        Event::MemberSessionForked {
            brigade_id: 9,
            token: "worker-a".to_string(),
            old_id: "sess-1".to_string(),
            new_id: "sess-2".to_string(),
            held_by_row: false,
        },
        Event::PaneSessionMoved {
            old_id: "sess-1".to_string(),
            new_id: "sess-2".to_string(),
            held_by_row: false,
        },
        Event::MembershipResolved {
            session_id: "sess-1".to_string(),
            membership: Some((9, "worker-a".to_string(), BrigadeRole::Worker)),
            members: Some(vec![(
                "worker-a".to_string(),
                BrigadeRole::Worker,
                Some("sess-1".to_string()),
            )]),
            layout: None,
            names: vec![(MARKER.to_string(), MARKER.to_string())],
        },
        Event::BrigadeFormed {
            director_row_id: MARKER.to_string(),
            name: MARKER.to_string(),
            cwd: marker_pathbuf(),
            worker_agent: AgentKind::Codex,
            worker_model: MARKER.to_string(),
            worker_effort: MARKER.to_string(),
            result: Err(MARKER.to_string()),
        },
        Event::WorkerAdded {
            brigade_id: 9,
            cwd: marker_pathbuf(),
            worker_agent: AgentKind::ClaudeCode,
            worker_model: MARKER.to_string(),
            worker_effort: MARKER.to_string(),
            result: Err(MARKER.to_string()),
        },
        Event::Disbanded {
            brigade_id: 9,
            result: Err(MARKER.to_string()),
        },
        Event::WorkerDismissed {
            brigade_id: 9,
            result: Err(MARKER.to_string()),
        },
        Event::ExitedWorkerDismissed {
            brigade_id: 9,
            token: "worker-a".to_string(),
            repeated: false,
            result: Err(MARKER.to_string()),
        },
        Event::StartupNotices {
            notices: vec![MARKER.to_string()],
        },
    ];
    let mut written = Vec::new();
    for ev in &text_events {
        written.extend(event_lines(ev, &facts));
    }
    assert_no_marker(&written_text(&written));

    let open = Cmd::OpenEmbedded {
        key: key.clone(),
        target: SessionToOpen {
            id: "sess-1".to_string(),
            agent: AgentKind::ClaudeCode,
            title: MARKER.to_string(),
            cwd: marker_pathbuf(),
        },
        brigade: Some((9, "worker-a".to_string(), BrigadeRole::Worker)),
        model: Some(MARKER.to_string()),
        effort: Some(MARKER.to_string()),
        permission_mode: Some(MARKER.to_string()),
        disallowed_tools: Some(MARKER.to_string()),
        autocompact: Some(MARKER.to_string()),
        first_prompt: None,
    };
    let cmds = vec![
        open,
        Cmd::HangUpPty { key: key.clone() },
        Cmd::Store(StoreIntent::RecordInterruptOutcome {
            brigade_id: 9,
            token: "worker-a".to_string(),
            message_id: 4,
            outcome: crate::model::InterruptOutcome::NoPane,
            why: Some(crate::model::InterruptWhy::Starting),
        }),
        Cmd::KillPty { key: key.clone() },
        Cmd::OpenCodexTrustPane { key: key.clone() },
        Cmd::RekeyPty {
            from: key.clone(),
            to: SessionKey::from_id("sess-2"),
        },
        Cmd::WritePty {
            key: key.clone(),
            bytes: MARKER.as_bytes().to_vec(),
        },
        Cmd::ForwardClipboardToHost {
            bytes: MARKER.as_bytes().to_vec(),
        },
        Cmd::ReadCodexCatalog { round: 1 },
        Cmd::Store(StoreIntent::FormBrigade {
            director_row_id: MARKER.to_string(),
            director_agent: AgentKind::Codex,
            name: MARKER.to_string(),
            cwd: marker_pathbuf(),
            worker_count: 2,
            worker_agent: AgentKind::Codex,
            worker_model: MARKER.to_string(),
            worker_effort: MARKER.to_string(),
        }),
        Cmd::Store(StoreIntent::AddWorker {
            brigade_id: 9,
            cwd: marker_pathbuf(),
            worker_agent: AgentKind::Codex,
            worker_model: MARKER.to_string(),
            worker_effort: MARKER.to_string(),
        }),
        Cmd::Store(StoreIntent::Disband { brigade_id: 9 }),
        Cmd::Store(StoreIntent::DismissWorker {
            brigade_id: 9,
            token: "worker-a".to_string(),
        }),
        Cmd::Store(StoreIntent::DismissExitedWorker {
            brigade_id: 9,
            token: "worker-a".to_string(),
            repeated: true,
        }),
        Cmd::Store(StoreIntent::SetMemberSession {
            brigade_id: 9,
            token: "worker-a".to_string(),
            session_id: "sess-1".to_string(),
        }),
        Cmd::Store(StoreIntent::ClearMemberSession {
            brigade_id: 9,
            token: "worker-a".to_string(),
        }),
        Cmd::Store(StoreIntent::SetPin {
            id: MARKER.to_string(),
            pinned: true,
        }),
        Cmd::Store(StoreIntent::Archive {
            id: MARKER.to_string(),
            title: MARKER.to_string(),
        }),
        Cmd::Store(StoreIntent::Unarchive {
            id: MARKER.to_string(),
            title: MARKER.to_string(),
        }),
        Cmd::Store(StoreIntent::JoinGroup {
            session_id: MARKER.to_string(),
            target: crate::engine::GroupJoinTargetData::New(MARKER.to_string()),
        }),
        Cmd::Store(StoreIntent::RenameGroup {
            group_id: 3,
            old_name: MARKER.to_string(),
            new_name: MARKER.to_string(),
        }),
        Cmd::Store(StoreIntent::DeleteGroup {
            group_id: 3,
            name: MARKER.to_string(),
        }),
        Cmd::Store(StoreIntent::ResolveMembership {
            session_id: MARKER.to_string(),
        }),
        Cmd::Store(StoreIntent::SaveBrigadeLayout {
            brigade_id: 9,
            layout: StoredLayout::default(),
        }),
        Cmd::Store(StoreIntent::SaveSidebarWidth { width: 30 }),
        Cmd::Store(StoreIntent::SaveBellWhenFocused { on: true }),
        Cmd::Store(StoreIntent::ClearRequest {
            brigade_id: 9,
            seq: 1,
        }),
        Cmd::Store(StoreIntent::SaveLastWorkerAgent {
            agent: AgentKind::Codex,
        }),
    ];
    let cmd_facts = CmdFacts {
        handle_present: true,
        replaced: true,
        pid: Some(77),
    };
    let lines: Vec<Line> = cmds
        .iter()
        .filter_map(|cmd| cmd_line(cmd, "PtyExited", cmd_facts))
        .collect();
    assert_no_marker(&written_text(&lines));
}

#[test]
fn start_line_has_the_fixed_fields_in_order() {
    let line = start_line("0.1.0", "0123456789ab", "yes", 23, 4242);
    assert_eq!(
        line.render(1_000, 5),
        "1000 panes +5ms start version=0.1.0 commit=0123456789ab dirty=yes schema=23 pid=4242"
    );
}

#[test]
fn pty_exit_line_names_the_pane_its_member_and_the_reason() {
    let ev = Event::PtyExited {
        key: SessionKey::from_id("sess-1"),
        reason: Some(PtyExitReason::Code(0)),
    };
    let facts = PaneFacts {
        asked: false,
        member: Some((7, "worker-a".to_string())),
        pid: Some(4321),
        was: Some(Quiescence::Idle),
        input_ago_ms: Some(2500),
    };
    let lines = event_lines(&ev, &facts);
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0].render(1_000, 5),
        "1000 panes +5ms PtyExited key=sess-1 pid=4321 brigade=7 member=worker-a asked=no was=idle input_ago_ms=2500 exit=code code=0"
    );
}

#[test]
fn pty_exit_line_says_what_the_pane_was_doing_in_one_fixed_word() {
    let ev = Event::PtyExited {
        key: SessionKey::from_id("sess-1"),
        reason: None,
    };
    let body = |was, input_ago_ms| {
        let facts = PaneFacts {
            was,
            input_ago_ms,
            ..PaneFacts::default()
        };
        event_lines(&ev, &facts)[0].body()
    };
    for (was, word) in [
        (Some(Quiescence::Idle), "idle"),
        (Some(Quiescence::NotIdle), "not_idle"),
        (Some(Quiescence::Background), "background"),
        (Some(Quiescence::Unknown), "unknown"),
        (None, "unknown"),
    ] {
        assert!(
            body(was, None).contains(&format!(" was={word} input_ago_ms=none ")),
            "{was:?}: {}",
            body(was, None)
        );
    }
    assert!(body(None, Some(0)).contains(" input_ago_ms=0 "));
}

#[test]
fn spawn_failure_line_carries_the_refusal_word_and_os_error_number() {
    let ev = Event::SpawnFailed {
        key: SessionKey::from_id("sess-2"),
        error: "already running elsewhere".to_string(),
        refusal: SpawnRefusal::AlreadyRunning,
        os_error: None,
    };
    let lines = event_lines(&ev, &PaneFacts::default());
    assert_eq!(
        lines[0].body(),
        "SpawnFailed key=sess-2 refusal=already_running os_error=none"
    );
}

#[test]
fn a_membership_resolution_is_one_line_per_roster_entry() {
    let ev = Event::MembershipResolved {
        session_id: "dir-1".to_string(),
        membership: Some((3, "worker-a".to_string(), BrigadeRole::Worker)),
        members: Some(vec![
            (
                "director".to_string(),
                BrigadeRole::Director,
                Some("dir-1".to_string()),
            ),
            ("worker-a".to_string(), BrigadeRole::Worker, None),
        ]),
        layout: None,
        names: Vec::new(),
    };
    let lines = event_lines(&ev, &PaneFacts::default());
    let bodies: Vec<String> = lines.iter().map(Line::body).collect();
    assert_eq!(
        bodies,
        vec![
            "MembershipResolved session=dir-1 brigade=3 member=worker-a role=worker roster=2",
            "MembershipRoster member=director role=director brigade=3 member_session=dir-1",
            "MembershipRoster member=worker-a role=worker brigade=3 member_session=none",
        ]
    );
}

#[test]
fn a_store_intent_that_is_not_about_a_member_writes_nothing() {
    let archive = Cmd::Store(StoreIntent::Archive {
        id: "s".to_string(),
        title: "t".to_string(),
    });
    assert!(cmd_line(&archive, "Tick", CmdFacts::default()).is_none());
    assert!(cmd_line(&Cmd::Reload, "Tick", CmdFacts::default()).is_none());
    assert!(
        cmd_line(
            &Cmd::WritePty {
                key: SessionKey::from_id("s"),
                bytes: Vec::new(),
            },
            "Input",
            CmdFacts::default()
        )
        .is_none()
    );
}

#[test]
fn every_written_cmd_line_carries_the_event_that_produced_it() {
    let kill = cmd_line(
        &Cmd::KillPty {
            key: SessionKey::from_id("sess-3"),
        },
        "Tick",
        CmdFacts {
            handle_present: true,
            replaced: false,
            pid: None,
        },
    )
    .expect("KillPty is written");
    assert_eq!(kill.body(), "KillPty key=sess-3 by=Tick handle=yes");
    let hangup = cmd_line(
        &Cmd::HangUpPty {
            key: SessionKey::from_id("sess-3"),
        },
        "Input",
        CmdFacts {
            handle_present: true,
            replaced: false,
            pid: None,
        },
    )
    .expect("HangUpPty is written");
    assert_eq!(hangup.body(), "HangUpPty key=sess-3 by=Input handle=yes");
    let decided = |why| {
        cmd_line(
            &Cmd::Store(StoreIntent::RecordInterruptOutcome {
                brigade_id: 5,
                token: "worker-2".to_string(),
                message_id: 11,
                outcome: crate::model::InterruptOutcome::NoPane,
                why,
            }),
            "Tick",
            CmdFacts::default(),
        )
        .expect("a decided interrupt is written")
        .body()
    };
    assert_eq!(
        decided(Some(crate::model::InterruptWhy::NotRunning)),
        "Store intent=RecordInterruptOutcome by=Tick brigade=5 member=worker-2 message=11 \
         outcome=no-pane why=not-running"
    );
    assert_eq!(
        decided(None),
        "Store intent=RecordInterruptOutcome by=Tick brigade=5 member=worker-2 message=11 \
         outcome=no-pane"
    );
    let disband = cmd_line(
        &Cmd::Store(StoreIntent::Disband { brigade_id: 5 }),
        "ExitedWorkerDismissed",
        CmdFacts::default(),
    )
    .expect("Disband is written");
    assert_eq!(
        disband.body(),
        "Store intent=Disband by=ExitedWorkerDismissed brigade=5"
    );
}

#[test]
fn is_logged_cmd_agrees_with_the_line_cmd_line_writes() {
    let cmds = [
        Cmd::HangUpPty {
            key: SessionKey::from_id("s"),
        },
        Cmd::KillPty {
            key: SessionKey::from_id("s"),
        },
        Cmd::Store(StoreIntent::RecordInterruptOutcome {
            brigade_id: 1,
            token: "worker-1".to_string(),
            message_id: 2,
            outcome: crate::model::InterruptOutcome::Armed,
            why: None,
        }),
        Cmd::Store(StoreIntent::Disband { brigade_id: 5 }),
        Cmd::Store(StoreIntent::Archive {
            id: "s".to_string(),
            title: "t".to_string(),
        }),
        Cmd::Reload,
        Cmd::WritePty {
            key: SessionKey::from_id("s"),
            bytes: Vec::new(),
        },
    ];
    for cmd in &cmds {
        assert_eq!(
            is_logged_cmd(cmd),
            cmd_line(cmd, "Tick", CmdFacts::default()).is_some(),
            "{cmd:?}"
        );
    }
}

#[test]
fn an_unwritten_event_has_no_line() {
    assert!(
        event_lines(
            &Event::Tick {
                relay: Vec::new(),
                brigades: Vec::new(),
            },
            &PaneFacts::default()
        )
        .is_empty()
    );
    assert!(
        event_lines(
            &Event::WindowFocusChanged { focused: true },
            &PaneFacts::default()
        )
        .is_empty()
    );
}

#[test]
fn the_pane_facts_are_read_for_an_exit_or_a_spawn_only() {
    let key = SessionKey::from_id("sess-7");
    assert_eq!(
        pane_facts_key(&Event::PtyOutputLost { key: key.clone() }),
        Some(&key)
    );
    assert!(
        pane_facts_key(&Event::Tick {
            relay: Vec::new(),
            brigades: Vec::new(),
        })
        .is_none()
    );
}

#[test]
fn a_token_is_kept_only_when_it_is_plain() {
    assert_eq!(token("worker-a.1_b"), "worker-a.1_b");
    assert_eq!(token("two words"), UNKNOWN);
    assert_eq!(token(""), UNKNOWN);
    assert_eq!(token(&"a".repeat(41)), UNKNOWN);
    assert_eq!(token(&"a".repeat(40)).len(), 40);
}

#[test]
fn an_id_may_carry_the_placeholder_colons_and_nothing_else() {
    assert_eq!(id("new-worker::1::goinkyo"), "new-worker::1::goinkyo");
    assert_eq!(id("a/b"), UNKNOWN);
    assert_eq!(id(&"a".repeat(65)), UNKNOWN);
}

#[test]
fn a_line_longer_than_the_limit_is_cut_to_it() {
    let mut line = Line::new("x");
    for _ in 0..20 {
        line = line.token("member", &"a".repeat(40));
    }
    assert!(line.render(1_000, 5).len() <= MAX_LINE_BYTES);
}

#[test]
fn a_line_over_the_limit_loses_trailing_fields_and_keeps_its_key_and_by() {
    let key = SessionKey::from_id(&"k".repeat(64));
    let mut line = Line::new("OpenEmbedded")
        .key("key", &key)
        .word("by", "ExitedWorkerDismissed");
    for _ in 0..20 {
        line = line.token("member", &"a".repeat(40));
    }
    let text = line.render(1_000, 5);
    assert!(text.len() <= MAX_LINE_BYTES, "{} bytes", text.len());
    assert!(
        text.starts_with(&format!(
            "1000 panes +5ms OpenEmbedded key={} by=ExitedWorkerDismissed member=",
            "k".repeat(64)
        )),
        "{text}"
    );
}

#[test]
fn goinkyo_line_names_its_fields_in_the_fixed_order() {
    let look = GoinkyoLook {
        brigade_id: 3,
        roster_read: true,
        row: GoinkyoRow::NoSession,
        director_hosted: true,
        guard: GoinkyoGuardFacts {
            guard: Some(SessionKey::from_id("new-worker::3::goinkyo")),
            pane: false,
            opening: true,
        },
        cwd: PathBuf::from("."),
        roster: Vec::new(),
        agent: None,
        director_agent: None,
    };
    assert_eq!(
        goinkyo_line(&look).body(),
        "goinkyo brigade=3 row=no_session director=hosted roster=read guard=placeholder pane=no opening=yes"
    );
    let unread = GoinkyoLook {
        row: GoinkyoRow::Session("sess-9".to_string()),
        roster_read: false,
        guard: GoinkyoGuardFacts {
            guard: None,
            pane: false,
            opening: false,
        },
        director_hosted: false,
        ..look
    };
    assert_eq!(
        goinkyo_line(&unread).body(),
        "goinkyo brigade=3 row=unread director=unknown roster=unreadable guard=none pane=no opening=no"
    );
}

/// The decision the gather made before it was a value: the branches of the
/// old loop body, written out, against which [`decide_goinkyo`] is checked for
/// every combination of its inputs.
fn old_gather_decision(look: &GoinkyoLook) -> Option<GoinkyoObservation> {
    if !look.roster_read {
        return None;
    }
    let row_has_session = matches!(look.row, GoinkyoRow::Session(_));
    let row_present = !matches!(look.row, GoinkyoRow::Absent);
    if !row_present {
        if look.director_hosted || look.guard.guard.is_some() {
            return Some(GoinkyoObservation::NoGoinkyo {
                brigade_id: look.brigade_id,
            });
        }
        return None;
    }
    if row_has_session || !look.director_hosted {
        return None;
    }
    Some(GoinkyoObservation::AwaitingSpawn(GoinkyoSpawnCandidate {
        brigade_id: look.brigade_id,
        cwd: look.cwd.clone(),
        roster: look.roster.clone(),
        agent: look.agent,
        director_agent: look.director_agent,
    }))
}

#[test]
fn decide_goinkyo_matches_the_gather_branches_it_replaces() {
    let rows = [
        GoinkyoRow::Absent,
        GoinkyoRow::NoSession,
        GoinkyoRow::Session("sess-9".to_string()),
    ];
    let mut checked = 0;
    for row in &rows {
        for director_hosted in [false, true] {
            for has_guard in [false, true] {
                for roster_read in [false, true] {
                    let look = GoinkyoLook {
                        brigade_id: 4,
                        roster_read,
                        row: row.clone(),
                        director_hosted,
                        guard: GoinkyoGuardFacts {
                            guard: has_guard.then(|| SessionKey::from_id("new-worker::4::goinkyo")),
                            pane: false,
                            opening: false,
                        },
                        cwd: PathBuf::from("/synthetic/cwd"),
                        roster: vec!["director".to_string(), "goinkyo".to_string()],
                        agent: None,
                        director_agent: None,
                    };
                    assert_eq!(
                        decide_goinkyo(&look),
                        old_gather_decision(&look),
                        "differs for {look:?}"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert_eq!(checked, 24);
}

#[test]
fn changed_only_reports_a_change_and_not_a_repeat() {
    let mut seen: ChangedOnly<i64> = ChangedOnly::default();
    assert!(seen.changed(&1, "a"));
    seen.remember(1, "a");
    assert!(!seen.changed(&1, "a"));
    assert!(seen.changed(&1, "b"));
    assert!(seen.changed(&2, "a"));
}

#[test]
fn a_change_that_was_never_remembered_is_still_a_change() {
    // What the rate limit does to a state line it drops: the next tick must
    // see the same change again, not a repeat of a line nobody wrote.
    let seen: ChangedOnly<i64> = ChangedOnly::default();
    assert!(seen.changed(&1, "a"));
    assert!(seen.changed(&1, "a"));
}

#[test]
fn rate_limit_admits_a_window_then_counts_the_rest() {
    let mut rate = RateLimit::default();
    for _ in 0..RATE_LIMIT_PER_MINUTE {
        assert_eq!(rate.admit(0), Admission::Write { suppressed: 0 });
    }
    for _ in 0..5 {
        assert_eq!(rate.admit(0), Admission::Drop);
    }
    assert_eq!(
        rate.admit(60_000),
        Admission::Write { suppressed: 5 },
        "the first line of the next window reports what was dropped"
    );
    assert_eq!(rate.admit(60_000), Admission::Write { suppressed: 0 });
}

#[test]
fn rate_limit_window_starts_at_its_first_line() {
    let mut rate = RateLimit::default();
    for _ in 0..RATE_LIMIT_PER_MINUTE {
        rate.admit(10_000);
    }
    assert_eq!(rate.admit(69_999), Admission::Drop);
    assert_eq!(rate.admit(70_000), Admission::Write { suppressed: 1 });
}

#[test]
fn suppressed_line_names_the_count() {
    assert_eq!(suppressed_line(12).body(), "suppressed count=12");
}

#[test]
fn reaped_line_names_its_pane_and_the_pid_it_held() {
    let key = SessionKey::from_id("abc");
    assert_eq!(reaped_line(&key, Some(42)).body(), "Reaped key=abc pid=42");
    assert_eq!(reaped_line(&key, None).body(), "Reaped key=abc pid=none");
}

#[test]
fn open_codex_trust_pane_line_names_its_key_replacement_and_pid() {
    let cmd = Cmd::OpenCodexTrustPane {
        key: SessionKey::from_id("sess-5"),
    };
    let facts = CmdFacts {
        handle_present: true,
        replaced: true,
        pid: Some(77),
    };
    assert_eq!(
        cmd_line(&cmd, "Tick", facts).expect("written").body(),
        "OpenCodexTrustPane key=sess-5 by=Tick replaced=yes pid=77"
    );
}
