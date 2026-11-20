//! A session with no title is not named by its id: the brigade formed on it is
//! stored without a name, and a sentence that has to say which session it means
//! uses the start of the id.

use std::path::PathBuf;
use std::time::Instant;

use crate::app::App;
use crate::config::{BrigadeConfig, WorkerAgentSetting};
use crate::model::{Activity, AgentKind, SessionRow};
use crate::screen::Screen;

use super::*;

const ID: &str = "0123456789abcdef-0123-4567";

fn row(id: &str, title: Option<&str>) -> SessionRow {
    agent_row(id, title, AgentKind::ClaudeCode)
}

fn agent_row(id: &str, title: Option<&str>, agent: AgentKind) -> SessionRow {
    SessionRow {
        id: id.to_string(),
        agent,
        title: title.map(str::to_string),
        cwd: Some(PathBuf::from("/work/alpha")),
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

#[allow(clippy::disallowed_methods)]
fn now() -> Instant {
    Instant::now()
}

/// What `B` on the Director row `ID` produces, with the brigade config `brigade`.
fn press_b(title: Option<&str>, brigade: &BrigadeConfig) -> (EmporiumState, App, Vec<Cmd>) {
    press_b_on(row(ID, title), brigade)
}

fn press_b_on(director: SessionRow, brigade: &BrigadeConfig) -> (EmporiumState, App, Vec<Cmd>) {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![director]);
    state.pending_membership = Some((PendingMembership::BrigadeKey, ID.to_string()));
    let cmds = update(
        &mut state,
        &mut app,
        brigade,
        Event::MembershipResolved {
            session_id: ID.to_string(),
            membership: None,
            members: None,
            layout: None,
            names: Vec::new(),
        },
        now(),
    );
    (state, app, cmds)
}

fn formed_name(cmds: &[Cmd]) -> String {
    cmds.iter()
        .find_map(|cmd| match cmd {
            Cmd::Store(StoreIntent::FormBrigade { name, .. }) => Some(name.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no FormBrigade in {cmds:?}"))
}

/// The shell's answer to `FormBrigade`: the store recorded the brigade as
/// number 7 under the name it was sent.
fn formed_as_brigade_7(state: &mut EmporiumState, app: &mut App, name: String) {
    update(
        state,
        app,
        &BrigadeConfig::default(),
        Event::BrigadeFormed {
            director_row_id: ID.to_string(),
            name,
            cwd: PathBuf::from("/work/alpha"),
            worker_agent: AgentKind::ClaudeCode,
            worker_model: String::new(),
            worker_effort: String::new(),
            result: Ok((7, vec!["worker-1".to_string()])),
        },
        now(),
    );
}

#[test]
fn a_brigade_formed_on_an_untitled_director_row_is_stored_without_a_name_and_called_by_its_number()
{
    let (mut state, mut app, cmds) = press_b(None, &BrigadeConfig::default());
    let name = formed_name(&cmds);
    assert_eq!(name, "", "not the row's id");
    formed_as_brigade_7(&mut state, &mut app, name);
    assert_eq!(state.brigade_name(&app, 7), "brigade 7");
}

#[test]
fn a_title_of_nothing_visible_is_no_name_either() {
    let (_, _, cmds) = press_b(Some(" \u{200b}\n"), &BrigadeConfig::default());
    assert_eq!(formed_name(&cmds), "");
}

#[test]
fn the_name_chosen_before_the_worker_picker_is_empty_for_an_untitled_row_too() {
    let select = BrigadeConfig {
        worker_agent: WorkerAgentSetting::Select,
        ..BrigadeConfig::default()
    };
    let (state, _, _) = press_b(None, &select);
    let pending = state
        .pending_brigade_formation
        .as_ref()
        .expect("the picker is open for the formation");
    assert_eq!(pending.name, "");
}

#[test]
fn a_brigade_formed_on_a_titled_director_row_keeps_its_title() {
    let (mut state, mut app, cmds) = press_b(Some("Fix the relay"), &BrigadeConfig::default());
    let name = formed_name(&cmds);
    assert_eq!(name, "Fix the relay");
    formed_as_brigade_7(&mut state, &mut app, name);
    assert_eq!(state.brigade_name(&app, 7), "\"Fix the relay\"");
}

#[test]
fn the_quit_prompt_calls_an_untitled_pane_by_the_start_of_its_id_and_a_titled_one_by_its_title() {
    let mut state = EmporiumState::new(PrefixKey::default());
    state
        .screens
        .insert(SessionKey::from_id(ID), Screen::new(24, 80));
    state
        .screens
        .insert(SessionKey::from_id("titled-id-1234"), Screen::new(24, 80));
    let app = App::new(vec![
        row(ID, None),
        row("titled-id-1234", Some("Fix login")),
    ]);
    assert_eq!(state.quit_name(&app, &SessionKey::from_id(ID)), "01234567");
    assert_eq!(
        state.quit_name(&app, &SessionKey::from_id("titled-id-1234")),
        "Fix login"
    );
}

#[test]
fn a_brigade_with_a_codex_member_keeps_an_untitled_directors_name_empty_through_the_trust_check() {
    let (mut state, mut app, cmds) = press_b_on(
        agent_row(ID, None, AgentKind::Codex),
        &BrigadeConfig::default(),
    );
    assert_eq!(cmds, vec![Cmd::CheckCodexTrust]);
    let stashed = state
        .pending_codex_trust_formation
        .as_ref()
        .expect("the formation waits for the trust check");
    assert_eq!(stashed.name, "", "not the row's id");

    let cmds = update(
        &mut state,
        &mut app,
        &BrigadeConfig::default(),
        Event::CodexTrustChecked {
            primed: true,
            hook_launchable: true,
        },
        now(),
    );
    assert_eq!(formed_name(&cmds), "");
}

#[test]
fn the_status_after_forming_calls_an_untitled_directors_brigade_by_its_number() {
    let (mut state, mut app, cmds) = press_b(None, &BrigadeConfig::default());
    formed_as_brigade_7(&mut state, &mut app, formed_name(&cmds));
    let status = state.status.clone().expect("a status line");
    assert!(
        status.starts_with("brigade formed — brigade 7, "),
        "{status}"
    );
    assert!(!status.contains(ID) && !status.contains(": ,"), "{status}");
}
