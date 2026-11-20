use std::path::PathBuf;

use crate::model::Activity;
use crate::screen::Screen;

use super::*;

#[allow(clippy::disallowed_methods)]
fn now() -> Instant {
    Instant::now()
}

fn row(id: &str, agent: AgentKind) -> SessionRow {
    SessionRow {
        id: id.to_string(),
        agent,
        title: Some(id.to_string()),
        cwd: Some(PathBuf::from("/work")),
        dialog_open: false,
        activity: Activity::Alive,
        is_agent: false,
        preview: None,
        mtime: std::time::SystemTime::UNIX_EPOCH,
        size: 0,
        source_archived: false,
        archived: false,
    }
}

fn unknown(state: &mut EmporiumState) -> SessionKey {
    let key = SessionKey::new_worker(9, "unknown");
    state.screens.insert(key.clone(), Screen::new(24, 80));
    state.unresolved_codex.insert(key.clone());
    key
}

fn membership(
    session: &str,
    members: Option<Vec<(MemberToken, BrigadeRole, Option<String>)>>,
) -> Event {
    Event::MembershipResolved {
        session_id: session.to_string(),
        membership: Some((1, "director".to_string(), BrigadeRole::Director)),
        members,
        layout: None,
        names: Vec::new(),
    }
}

#[test]
fn existing_director_can_disband_with_an_unrelated_unidentified_codex() {
    let mut state = EmporiumState::new(PrefixKey::default());
    unknown(&mut state);
    let mut app = App::new(vec![row("dir", AgentKind::Codex)]);
    state.pending_membership = Some((PendingMembership::BrigadeKey, "dir".to_string()));
    let cmds = update(
        &mut state,
        &mut app,
        &BrigadeConfig::default(),
        membership("dir", None),
        now(),
    );
    assert!(cmds.is_empty());
    assert!(matches!(app.modal(), Some(Modal::ConfirmDisband { .. })));
}

#[test]
fn confirmation_does_not_kill_a_known_director_if_identity_became_uncertain() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let director = SessionKey::from_id("dir");
    state.screens.insert(director.clone(), Screen::new(24, 80));
    let mut app = App::new(vec![row("dir", AgentKind::Codex)]);
    app.open_confirm_director_reopen_modal("dir".to_string(), "dir".to_string());
    unknown(&mut state);
    let cmds = confirm_director_reopen_modal(&mut state, &mut app);
    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::KillPty { key } if key == &director)),
        "{cmds:?}"
    );
    assert!(state.screens.contains_key(&director));
}

#[test]
fn staging_mixed_brigade_does_not_partially_launch_while_identity_is_unknown() {
    let mut state = EmporiumState::new(PrefixKey::default());
    unknown(&mut state);
    let mut app = App::new(vec![
        row("dir", AgentKind::Codex),
        row("worker", AgentKind::ClaudeCode),
    ]);
    state.pending_membership = Some((PendingMembership::Activate, "dir".to_string()));
    let members = vec![
        (
            "director".to_string(),
            BrigadeRole::Director,
            Some("dir".to_string()),
        ),
        (
            "worker-1".to_string(),
            BrigadeRole::Worker,
            Some("worker".to_string()),
        ),
    ];
    let cmds = update(
        &mut state,
        &mut app,
        &BrigadeConfig::default(),
        membership("dir", Some(members)),
        now(),
    );
    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::OpenEmbedded { .. })),
        "partial launch: {cmds:?}"
    );
    assert!(
        !state
            .pending_opens
            .contains_key(&SessionKey::from_id("dir"))
    );
    assert!(
        state
            .status
            .as_deref()
            .is_some_and(|s| s.contains("identif")),
        "{:?}",
        state.status
    );
}

#[test]
fn formation_answer_racing_an_unidentified_codex_is_not_reported_as_spawned() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row("dir", AgentKind::Codex)]);
    unknown(&mut state);
    let cmds = update(
        &mut state,
        &mut app,
        &BrigadeConfig::default(),
        Event::BrigadeFormed {
            director_row_id: "dir".to_string(),
            name: "dir".to_string(),
            cwd: PathBuf::from("/work"),
            worker_agent: AgentKind::Codex,
            worker_model: String::new(),
            worker_effort: String::new(),
            result: Ok((1, vec!["worker-1".to_string()])),
        },
        now(),
    );
    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::OpenEmbedded { target, .. } if target.id == "dir"))
    );
    assert!(
        state
            .status
            .as_deref()
            .is_some_and(|s| s.contains("identif")),
        "must explain the held Director: {:?}",
        state.status
    );
}

#[test]
fn saved_brigade_can_be_staged_after_the_unidentified_pane_exits() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row("dir", AgentKind::Codex)]);
    let config = BrigadeConfig {
        worker_model_codex: String::new(),
        ..BrigadeConfig::default()
    };
    let unknown_key = unknown(&mut state);
    let held = update(
        &mut state,
        &mut app,
        &config,
        Event::BrigadeFormed {
            director_row_id: "dir".to_string(),
            name: "dir".to_string(),
            cwd: PathBuf::from("/work"),
            worker_agent: AgentKind::Codex,
            worker_model: String::new(),
            worker_effort: String::new(),
            result: Ok((1, vec!["worker-1".to_string()])),
        },
        now(),
    );
    assert!(
        !held
            .iter()
            .any(|cmd| matches!(cmd, Cmd::OpenEmbedded { .. }))
    );
    let exit = update(
        &mut state,
        &mut app,
        &config,
        Event::PtyExited {
            key: unknown_key,
            reason: None,
        },
        now(),
    );
    assert!(
        !exit
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Store(StoreIntent::FormBrigade { .. })))
    );
    state.pending_membership = Some((PendingMembership::Activate, "dir".to_string()));
    let members = vec![
        (
            "director".to_string(),
            BrigadeRole::Director,
            Some("dir".to_string()),
        ),
        ("worker-1".to_string(), BrigadeRole::Worker, None),
    ];
    let cmds = update(
        &mut state,
        &mut app,
        &config,
        membership("dir", Some(members)),
        now(),
    );
    assert_eq!(cmds.iter().filter(|cmd| matches!(cmd, Cmd::OpenEmbedded { target, brigade: Some((1, _, BrigadeRole::Director)), .. } if target.id == "dir" && target.agent == AgentKind::Codex)).count(), 1, "{cmds:?}");
    assert_eq!(cmds.iter().filter(|cmd| matches!(cmd, Cmd::OpenEmbedded { target, brigade: Some((1, _, BrigadeRole::Worker)), .. } if target.id.is_empty() && target.agent == AgentKind::Codex)).count(), 1, "{cmds:?}");
    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Store(StoreIntent::FormBrigade { .. })))
    );
}
