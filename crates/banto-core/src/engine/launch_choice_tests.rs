//! Which model and reasoning effort a brigade member is launched with, driven
//! through real `Event`s the way the shell feeds them.
//!
//! The rule under test: a launch that creates a session passes its model and
//! effort, a launch that resumes one passes neither — for every site that starts
//! a member, both products. A flag-less resume restores what the session last
//! used, so a flag on one overwrites a choice the operator made inside it
//! (`RoleLaunch`'s doc). A Codex model written as a family word meets the
//! installed Codex's catalog first (`Cmd::ReadCodexCatalog`), once per creation
//! round, and the answer, or the lack of one, is said on the status line.

use std::path::PathBuf;
use std::time::Instant;

use crate::app::App;
use crate::config::{BrigadeConfig, WorkerAgentSetting};
use crate::model::{Activity, AgentKind, BrigadeRole, PtyExitReason, SessionRow};
use crate::screen::Screen;

use super::*;

fn row(id: &str, agent: AgentKind) -> SessionRow {
    SessionRow {
        id: id.to_string(),
        agent,
        title: Some(id.to_string()),
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

/// A catalog in the shape `codex debug models --bundled` prints, synthetic:
/// two members of the `terra` family, one hidden and newer, and another family.
pub(super) fn catalog() -> Result<String, String> {
    Ok(r#"{"models":[
        {"slug":"gpt-5.6-terra","visibility":"list","priority":3},
        {"slug":"gpt-6.1-terra","visibility":"list","priority":2},
        {"slug":"gpt-7-terra","visibility":"hide","priority":0},
        {"slug":"gpt-6.1-sol","visibility":"list","priority":1}
    ]}"#
    .to_string())
}

/// What the shell does with a `Cmd::ReadCodexCatalog` in `cmds`: answer it
/// with `catalog` and hand back what that releases in its place, the rest of
/// `cmds` unchanged.
pub(super) fn answer_catalog(
    state: &mut EmporiumState,
    app: &mut App,
    brigade: &BrigadeConfig,
    cmds: Vec<Cmd>,
    catalog: &Result<String, String>,
) -> Vec<Cmd> {
    let mut out = Vec::new();
    for cmd in cmds {
        if let Cmd::ReadCodexCatalog { round } = cmd {
            out.extend(update(
                state,
                app,
                brigade,
                Event::CodexCatalogRead {
                    round,
                    result: catalog.clone(),
                },
                now(),
            ));
        } else {
            out.push(cmd);
        }
    }
    out
}

/// One `Cmd::OpenEmbedded`, flattened for comparing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Launch {
    id: String,
    agent: AgentKind,
    role: Option<BrigadeRole>,
    model: Option<String>,
    effort: Option<String>,
    permission_mode: Option<String>,
    disallowed_tools: Option<String>,
}

fn launches(cmds: &[Cmd]) -> Vec<Launch> {
    cmds.iter()
        .filter_map(|cmd| match cmd {
            Cmd::OpenEmbedded {
                target,
                brigade,
                model,
                effort,
                permission_mode,
                disallowed_tools,
                ..
            } => Some(Launch {
                id: target.id.clone(),
                agent: target.agent,
                role: brigade.as_ref().map(|(_, _, role)| *role),
                model: model.clone(),
                effort: effort.clone(),
                permission_mode: permission_mode.clone(),
                disallowed_tools: disallowed_tools.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn s(text: &str) -> Option<String> {
    Some(text.to_string())
}

/// A configuration whose every model and effort differs from the built-in ones,
/// so a value that leaks from the wrong place cannot match by luck.
fn configured() -> BrigadeConfig {
    BrigadeConfig {
        worker_model: "opus".to_string(),
        worker_effort: "low".to_string(),
        worker_model_codex: "gpt-5.6-terra".to_string(),
        worker_effort_codex: "xhigh".to_string(),
        goinkyo_model: "haiku".to_string(),
        goinkyo_effort: "medium".to_string(),
        ..BrigadeConfig::default()
    }
}

/// What the shell does for each command a formation or an addition produces,
/// until nothing is left to answer: the trust check passes, the store records
/// the brigade or the Worker, a Director or Worker pane spawns, the catalog is
/// read. Every `Cmd::OpenEmbedded` seen on the way is returned, and how many
/// times the catalog was asked for.
struct Run {
    opens: Vec<Cmd>,
    catalog_reads: usize,
}

fn run(
    state: &mut EmporiumState,
    app: &mut App,
    brigade: &BrigadeConfig,
    first: Vec<Cmd>,
    catalog: &Result<String, String>,
    workers: usize,
) -> Run {
    let mut pending: std::collections::VecDeque<Cmd> = first.into();
    let mut out = Run {
        opens: Vec::new(),
        catalog_reads: 0,
    };
    let mut next_worker = workers;
    while let Some(cmd) = pending.pop_front() {
        let events = match cmd {
            Cmd::CheckCodexTrust => vec![Event::CodexTrustChecked {
                primed: true,
                hook_launchable: true,
            }],
            Cmd::Store(StoreIntent::FormBrigade {
                director_row_id,
                name,
                cwd,
                worker_count,
                worker_agent,
                worker_model,
                worker_effort,
                ..
            }) => vec![Event::BrigadeFormed {
                director_row_id,
                name,
                cwd,
                worker_agent,
                worker_model,
                worker_effort,
                result: Ok((
                    1,
                    (1..=worker_count).map(|n| format!("worker-{n}")).collect(),
                )),
            }],
            Cmd::Store(StoreIntent::AddWorker {
                brigade_id,
                cwd,
                worker_agent,
                worker_model,
                worker_effort,
            }) => {
                next_worker += 1;
                vec![Event::WorkerAdded {
                    brigade_id,
                    cwd,
                    worker_agent,
                    worker_model,
                    worker_effort,
                    result: Ok(format!("worker-{next_worker}")),
                }]
            }
            Cmd::ReadCodexCatalog { round } => {
                out.catalog_reads += 1;
                vec![Event::CodexCatalogRead {
                    round,
                    result: catalog.clone(),
                }]
            }
            open @ Cmd::OpenEmbedded { .. } => {
                let Cmd::OpenEmbedded { key, .. } = &open else {
                    unreachable!()
                };
                let key = key.clone();
                out.opens.push(open);
                vec![Event::Spawned { key }]
            }
            _ => Vec::new(),
        };
        for event in events {
            pending.extend(update(state, app, brigade, event, now()));
        }
    }
    out
}

/// `B` on the row `dir` of product `director_agent`, carried through.
fn form(
    brigade: &BrigadeConfig,
    director_agent: AgentKind,
    catalog: &Result<String, String>,
) -> (EmporiumState, App, Run) {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row("dir", director_agent)]);
    state.pending_membership = Some(PendingMembership::BrigadeKey);
    let first = update(
        &mut state,
        &mut app,
        brigade,
        Event::MembershipResolved {
            session_id: "dir".to_string(),
            membership: None,
            members: None,
            layout: None,
            names: Vec::new(),
        },
        now(),
    );
    let out = run(
        &mut state,
        &mut app,
        brigade,
        first,
        catalog,
        brigade.worker_count(),
    );
    (state, app, out)
}

// --- a session that exists is resumed with neither ----------------------------

#[test]
fn restaging_a_brigade_resumes_every_member_with_no_model_and_no_effort() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![
        row("dir", AgentKind::ClaudeCode),
        row("w1", AgentKind::ClaudeCode),
        row("w2", AgentKind::Codex),
        row("g1", AgentKind::ClaudeCode),
    ]);
    state.pending_membership = Some(PendingMembership::Activate);
    let roster = vec![
        ("director".to_string(), BrigadeRole::Director, s("dir")),
        ("worker-1".to_string(), BrigadeRole::Worker, s("w1")),
        ("worker-2".to_string(), BrigadeRole::Worker, s("w2")),
        ("goinkyo".to_string(), BrigadeRole::Goinkyo, s("g1")),
    ];
    let cmds = update(
        &mut state,
        &mut app,
        &configured(),
        Event::MembershipResolved {
            session_id: "dir".to_string(),
            membership: Some((1, "director".to_string(), BrigadeRole::Director)),
            members: Some(roster),
            layout: None,
            names: Vec::new(),
        },
        now(),
    );

    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::ReadCodexCatalog { .. })),
        "a resume needs no model, so no catalog: {cmds:?}"
    );
    let got = launches(&cmds);
    assert_eq!(got.len(), 4, "{got:?}");
    for launch in &got {
        assert_eq!(launch.model, None, "{launch:?}");
        assert_eq!(launch.effort, None, "{launch:?}");
    }
    // The role's own flags are untouched by this: who may approve and which
    // tools exist are the role's on every launch.
    let by_id = |id: &str| got.iter().find(|launch| launch.id == id).unwrap();
    assert_eq!(by_id("dir").permission_mode, None);
    assert_eq!(by_id("w1").permission_mode, s("auto"));
    assert_eq!(by_id("w2").permission_mode, s("auto"));
    assert_eq!(by_id("w2").agent, AgentKind::Codex);
    assert_eq!(by_id("g1").permission_mode, s("auto"));
    assert_eq!(by_id("g1").disallowed_tools, s("Edit,Write,NotebookEdit"));
    assert_eq!(by_id("w1").disallowed_tools, None);
}

#[test]
fn a_relaunched_worker_resumes_with_no_model_and_no_effort_for_either_product() {
    for agent in [AgentKind::ClaudeCode, AgentKind::Codex] {
        let mut state = EmporiumState::new(PrefixKey::default());
        for (id, token) in [("dir", DIRECTOR_TOKEN), ("w1", "worker-1")] {
            state
                .screens
                .insert(SessionKey::from_id(id), Screen::new(24, 80));
            state
                .member_tokens
                .insert(SessionKey::from_id(id), token.to_string());
            state.member_brigades.insert(SessionKey::from_id(id), 1);
        }
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(SessionKey::from_id("dir")),
            panes: vec![SessionKey::from_id("dir"), SessionKey::from_id("w1")],
            focused: 1,
        };
        let mut app = App::new(vec![row("dir", AgentKind::ClaudeCode), row("w1", agent)]);
        let cmds = update(
            &mut state,
            &mut app,
            &configured(),
            Event::PtyExited {
                key: SessionKey::from_id("w1"),
                reason: Some(PtyExitReason::Code(0)),
            },
            now(),
        );
        let got = launches(&cmds);
        assert_eq!(got.len(), 1, "{agent:?}: {cmds:?}");
        assert_eq!(got[0].id, "w1");
        assert_eq!(got[0].model, None, "{agent:?}");
        assert_eq!(got[0].effort, None, "{agent:?}");
        assert_eq!(got[0].permission_mode, s("auto"));
        assert!(
            !cmds
                .iter()
                .any(|cmd| matches!(cmd, Cmd::ReadCodexCatalog { .. })),
            "{agent:?}: {cmds:?}"
        );
    }
}

#[test]
fn opening_a_solo_session_and_reopening_a_director_pass_no_model_and_no_effort() {
    for agent in [AgentKind::ClaudeCode, AgentKind::Codex] {
        // A row activated on its own.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = App::new(vec![row("a", agent)]);
        state.pending_membership = Some(PendingMembership::Activate);
        let cmds = update(
            &mut state,
            &mut app,
            &configured(),
            Event::MembershipResolved {
                session_id: "a".to_string(),
                membership: None,
                members: None,
                layout: None,
                names: Vec::new(),
            },
            now(),
        );
        let got = launches(&cmds);
        assert_eq!(got.len(), 1, "{cmds:?}");
        assert_eq!((got[0].model.clone(), got[0].effort.clone()), (None, None));

        // The Director of a brigade being formed, opened wired onto an existing
        // session.
        let (_, _, out) = form(&configured(), agent, &catalog());
        let director = launches(&out.opens)
            .into_iter()
            .find(|launch| launch.role == Some(BrigadeRole::Director))
            .expect("a Director open");
        assert_eq!(
            (director.model, director.effort, director.permission_mode),
            (None, None, None),
            "{agent:?}"
        );
    }
}

// --- a session that is created gets the defaults -------------------------------

#[test]
fn a_formed_claude_brigade_creates_its_workers_with_the_configured_model_and_effort() {
    let brigade = BrigadeConfig {
        workers: 2,
        ..configured()
    };
    let (_, _, out) = form(&brigade, AgentKind::ClaudeCode, &catalog());
    let workers: Vec<Launch> = launches(&out.opens)
        .into_iter()
        .filter(|launch| launch.role == Some(BrigadeRole::Worker))
        .collect();
    assert_eq!(workers.len(), 2, "{workers:?}");
    for worker in &workers {
        assert_eq!(worker.id, "", "created, not resumed");
        assert_eq!(worker.agent, AgentKind::ClaudeCode);
        assert_eq!(worker.model, s("opus"));
        assert_eq!(worker.effort, s("low"));
        assert_eq!(worker.permission_mode, s("auto"));
    }
    assert_eq!(out.catalog_reads, 0, "Claude has no catalog to read");
}

#[test]
fn a_formed_codex_brigade_reads_the_catalog_once_and_creates_every_worker_with_the_resolved_model()
{
    let brigade = BrigadeConfig {
        workers: 3,
        worker_agent: WorkerAgentSetting::Codex,
        worker_model_codex: "terra".to_string(),
        ..configured()
    };
    let (state, _, out) = form(&brigade, AgentKind::ClaudeCode, &catalog());
    assert_eq!(out.catalog_reads, 1, "one read answers all three");
    let workers: Vec<Launch> = launches(&out.opens)
        .into_iter()
        .filter(|launch| launch.role == Some(BrigadeRole::Worker))
        .collect();
    assert_eq!(workers.len(), 3, "{workers:?}");
    for worker in &workers {
        assert_eq!(worker.agent, AgentKind::Codex);
        // The newest visible `terra`: not the hidden `gpt-7-terra`.
        assert_eq!(worker.model, s("gpt-6.1-terra"));
        assert_eq!(worker.effort, s("xhigh"));
        assert_eq!(worker.id, "");
    }
    assert!(state.codex_opens_awaiting_catalog.is_empty());
    assert_eq!(
        state.status.as_deref(),
        Some("codex worker: terra → gpt-6.1-terra"),
        "said once for the one word"
    );
}

#[test]
fn an_exact_codex_slug_is_passed_as_it_is_with_nothing_said() {
    let brigade = BrigadeConfig {
        worker_agent: WorkerAgentSetting::Codex,
        worker_model_codex: "gpt-5.6-terra".to_string(),
        ..configured()
    };
    let (state, _, out) = form(&brigade, AgentKind::ClaudeCode, &catalog());
    let worker = launches(&out.opens)
        .into_iter()
        .find(|launch| launch.role == Some(BrigadeRole::Worker))
        .unwrap();
    assert_eq!(worker.model, s("gpt-5.6-terra"));
    assert!(
        !state
            .status
            .as_deref()
            .unwrap_or("")
            .contains("codex worker"),
        "{:?}",
        state.status
    );
}

#[test]
fn an_empty_codex_model_passes_no_model_and_reads_no_catalog_but_still_passes_the_effort() {
    let brigade = BrigadeConfig {
        worker_agent: WorkerAgentSetting::Codex,
        worker_model_codex: String::new(),
        ..configured()
    };
    let (_, _, out) = form(&brigade, AgentKind::ClaudeCode, &catalog());
    assert_eq!(out.catalog_reads, 0);
    let worker = launches(&out.opens)
        .into_iter()
        .find(|launch| launch.role == Some(BrigadeRole::Worker))
        .unwrap();
    assert_eq!(worker.model, None);
    assert_eq!(worker.effort, s("xhigh"));
}

#[test]
fn an_empty_effort_passes_no_effort_for_either_product() {
    for (agent, setting) in [
        (AgentKind::ClaudeCode, WorkerAgentSetting::Claude),
        (AgentKind::Codex, WorkerAgentSetting::Codex),
    ] {
        let brigade = BrigadeConfig {
            worker_agent: setting,
            worker_effort: String::new(),
            worker_effort_codex: String::new(),
            ..configured()
        };
        let (_, _, out) = form(&brigade, AgentKind::ClaudeCode, &catalog());
        let worker = launches(&out.opens)
            .into_iter()
            .find(|launch| launch.role == Some(BrigadeRole::Worker))
            .unwrap();
        assert_eq!(worker.agent, agent);
        assert_eq!(worker.effort, None, "{agent:?}");
        assert!(worker.model.is_some(), "the model is a separate setting");
    }
}

#[test]
fn an_unreadable_catalog_leaves_a_family_word_unpassed_and_says_what_codex_will_use() {
    let brigade = BrigadeConfig {
        worker_agent: WorkerAgentSetting::Codex,
        worker_model_codex: "terra".to_string(),
        ..configured()
    };
    let (state, _, out) = form(
        &brigade,
        AgentKind::ClaudeCode,
        &Err("timed out after 5s".to_string()),
    );
    let worker = launches(&out.opens)
        .into_iter()
        .find(|launch| launch.role == Some(BrigadeRole::Worker))
        .unwrap();
    assert_eq!(worker.model, None, "no -m for a bare family word");
    assert_eq!(
        worker.effort,
        s("xhigh"),
        "the effort does not need the catalog"
    );
    let status = state.status.as_deref().unwrap();
    assert!(status.contains("timed out after 5s"), "{status}");
    assert!(
        status.contains("Codex uses its own default model"),
        "{status}"
    );
}

#[test]
fn an_unreadable_catalog_still_passes_a_configured_version_as_a_slug_and_says_so() {
    let brigade = BrigadeConfig {
        worker_agent: WorkerAgentSetting::Codex,
        worker_model_codex: "gpt-9-sol".to_string(),
        ..configured()
    };
    let (state, _, out) = form(
        &brigade,
        AgentKind::ClaudeCode,
        &Err("codex was not found".to_string()),
    );
    let worker = launches(&out.opens)
        .into_iter()
        .find(|launch| launch.role == Some(BrigadeRole::Worker))
        .unwrap();
    assert_eq!(worker.model, s("gpt-9-sol"));
    assert!(state.status.as_deref().unwrap().contains("passed as given"));
}

#[test]
fn a_codex_model_the_catalog_does_not_know_is_passed_as_given_and_said_so() {
    let brigade = BrigadeConfig {
        worker_agent: WorkerAgentSetting::Codex,
        worker_model_codex: "gpt-9-mystery".to_string(),
        ..configured()
    };
    let (state, _, out) = form(&brigade, AgentKind::ClaudeCode, &catalog());
    let worker = launches(&out.opens)
        .into_iter()
        .find(|launch| launch.role == Some(BrigadeRole::Worker))
        .unwrap();
    assert_eq!(worker.model, s("gpt-9-mystery"));
    assert!(
        state
            .status
            .as_deref()
            .unwrap()
            .contains("not in Codex's model list")
    );
}

#[test]
fn adding_a_worker_creates_it_with_the_configured_model_and_effort_for_either_product() {
    for (setting, agent, model, effort) in [
        (
            WorkerAgentSetting::Claude,
            AgentKind::ClaudeCode,
            "opus",
            "low",
        ),
        (
            WorkerAgentSetting::Codex,
            AgentKind::Codex,
            "gpt-6.1-terra",
            "xhigh",
        ),
    ] {
        let brigade = BrigadeConfig {
            worker_agent: setting,
            worker_model_codex: "terra".to_string(),
            ..configured()
        };
        let mut state = EmporiumState::new(PrefixKey::default());
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(SessionKey::from_id("dir")),
            panes: vec![SessionKey::from_id("dir")],
            focused: 0,
        };
        let mut app = App::new(vec![row("dir", AgentKind::ClaudeCode)]);
        let first = add_worker(&mut state, &mut app, &brigade);
        let out = run(&mut state, &mut app, &brigade, first, &catalog(), 1);
        let got = launches(&out.opens);
        assert_eq!(got.len(), 1, "{agent:?}: {got:?}");
        assert_eq!(got[0].agent, agent);
        assert_eq!(got[0].id, "");
        assert_eq!(got[0].model, s(model), "{agent:?}");
        assert_eq!(got[0].effort, s(effort), "{agent:?}");
    }
}

#[test]
fn a_worker_that_died_before_its_session_existed_is_created_again_from_the_defaults() {
    // Its roster entry has no session id and no pane: staging has nothing to
    // resume, so it creates one, from the configuration. What a picker said
    // when the Worker was first added is not stored, so cannot come back.
    for (setting, agent, model, effort) in [
        (
            WorkerAgentSetting::Claude,
            AgentKind::ClaudeCode,
            "opus",
            "low",
        ),
        (
            WorkerAgentSetting::Codex,
            AgentKind::Codex,
            "gpt-6.1-terra",
            "xhigh",
        ),
    ] {
        let brigade = BrigadeConfig {
            worker_agent: setting,
            worker_model_codex: "terra".to_string(),
            ..configured()
        };
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = App::new(vec![row("dir", AgentKind::ClaudeCode)]);
        state.pending_membership = Some(PendingMembership::Activate);
        let first = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MembershipResolved {
                session_id: "dir".to_string(),
                membership: Some((1, "director".to_string(), BrigadeRole::Director)),
                members: Some(vec![
                    ("director".to_string(), BrigadeRole::Director, s("dir")),
                    ("worker-1".to_string(), BrigadeRole::Worker, None),
                ]),
                layout: None,
                names: Vec::new(),
            },
            now(),
        );
        let first = answer_catalog(&mut state, &mut app, &brigade, first, &catalog());
        let created: Vec<Launch> = launches(&first)
            .into_iter()
            .filter(|launch| launch.id.is_empty())
            .collect();
        assert_eq!(created.len(), 1, "{agent:?}: {first:?}");
        assert_eq!(created[0].agent, agent);
        assert_eq!(created[0].model, s(model), "{agent:?}");
        assert_eq!(created[0].effort, s(effort), "{agent:?}");
        // And the Director that is resumed alongside it passes neither.
        let resumed: Vec<Launch> = launches(&first)
            .into_iter()
            .filter(|launch| !launch.id.is_empty())
            .collect();
        assert!(
            resumed
                .iter()
                .all(|l| l.model.is_none() && l.effort.is_none()),
            "{resumed:?}"
        );
    }
}

#[test]
fn a_fresh_goinkyo_is_created_with_its_model_and_effort_and_a_resumed_one_with_neither() {
    let brigade = configured();
    // Fresh: its row exists with no session id yet.
    let candidate = GoinkyoSpawnCandidate {
        brigade_id: 1,
        cwd: PathBuf::from("/work"),
        roster: Vec::new(),
    };
    let mut state = EmporiumState::new(PrefixKey::default());
    let cmds = update_goinkyo_awaiting_spawn(
        &mut state,
        &brigade,
        GoinkyoObservation::AwaitingSpawn(candidate),
        now(),
    );
    let got = launches(&cmds);
    assert_eq!(got.len(), 1, "{cmds:?}");
    assert_eq!(got[0].role, Some(BrigadeRole::Goinkyo));
    assert_eq!(got[0].model, s("haiku"));
    assert_eq!(got[0].effort, s("medium"));
    assert_eq!(got[0].permission_mode, s("auto"));
    assert_eq!(got[0].disallowed_tools, s("Edit,Write,NotebookEdit"));

    // Resumed: the restaging test above covers `g1`; here the same Goinkyo's
    // row now exists and the brigade is staged again.
    state.pending_membership = Some(PendingMembership::Activate);
    let mut app = App::new(vec![
        row("dir", AgentKind::ClaudeCode),
        row("g1", AgentKind::ClaudeCode),
    ]);
    let cmds = update(
        &mut state,
        &mut app,
        &brigade,
        Event::MembershipResolved {
            session_id: "dir".to_string(),
            membership: Some((1, "director".to_string(), BrigadeRole::Director)),
            members: Some(vec![
                ("director".to_string(), BrigadeRole::Director, s("dir")),
                ("goinkyo".to_string(), BrigadeRole::Goinkyo, s("g1")),
            ]),
            layout: None,
            names: Vec::new(),
        },
        now(),
    );
    let goinkyo = launches(&cmds)
        .into_iter()
        .find(|launch| launch.role == Some(BrigadeRole::Goinkyo))
        .expect("the Goinkyo is resumed");
    assert_eq!((goinkyo.model, goinkyo.effort), (None, None));
    assert_eq!(goinkyo.permission_mode, s("auto"));
}

// --- the picker's choice -------------------------------------------------------

#[test]
fn what_the_picker_confirms_is_what_the_workers_are_created_with() {
    let brigade = BrigadeConfig {
        worker_agent: WorkerAgentSetting::Select,
        workers: 2,
        ..configured()
    };
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row("dir", AgentKind::ClaudeCode)]);
    state.pending_membership = Some(PendingMembership::BrigadeKey);
    update(
        &mut state,
        &mut app,
        &brigade,
        Event::MembershipResolved {
            session_id: "dir".to_string(),
            membership: None,
            members: None,
            layout: None,
            names: Vec::new(),
        },
        now(),
    );
    assert!(matches!(app.modal(), Some(Modal::WorkerAgentPicker(_))));

    // Toggle to Codex (refilling the defaults), then type over both fields.
    app.modal_toggle_agent();
    for _ in 0.."gpt-5.6-terra".len() {
        app.modal_backspace();
    }
    for c in "terra".chars() {
        app.modal_push_char(c);
    }
    app.modal_complete_candidate(); // Tab: to the effort field
    for _ in 0.."xhigh".len() {
        app.modal_backspace();
    }
    for c in "minimal".chars() {
        app.modal_push_char(c);
    }
    let first = confirm_worker_agent_modal(&mut state, &mut app);
    let out = run(&mut state, &mut app, &brigade, first, &catalog(), 2);
    let workers: Vec<Launch> = launches(&out.opens)
        .into_iter()
        .filter(|launch| launch.role == Some(BrigadeRole::Worker))
        .collect();
    assert_eq!(workers.len(), 2);
    for worker in &workers {
        assert_eq!(worker.agent, AgentKind::Codex);
        assert_eq!(
            worker.model,
            s("gpt-6.1-terra"),
            "the typed family word, resolved"
        );
        assert_eq!(worker.effort, s("minimal"));
    }
}

#[test]
fn the_picker_for_adding_a_worker_creates_that_worker_with_its_choice() {
    let brigade = BrigadeConfig {
        worker_agent: WorkerAgentSetting::Select,
        ..configured()
    };
    let mut state = EmporiumState::new(PrefixKey::default());
    state.stage = Stage::Brigade {
        id: 1,
        director: Some(SessionKey::from_id("dir")),
        panes: vec![SessionKey::from_id("dir")],
        focused: 0,
    };
    let mut app = App::new(vec![row("dir", AgentKind::ClaudeCode)]);
    assert!(add_worker(&mut state, &mut app, &brigade).is_empty());
    let Some(Modal::WorkerAgentPicker(picker)) = app.modal() else {
        panic!("expected the picker");
    };
    assert_eq!(picker.purpose(), WorkerPickerFor::AddWorker);
    assert_eq!(picker.model_input(), "opus");
    assert_eq!(picker.effort_input(), "low");

    // Blank both: the Worker is created with no model flag and no effort flag.
    for _ in 0..4 {
        app.modal_backspace();
    }
    app.modal_complete_candidate();
    for _ in 0..3 {
        app.modal_backspace();
    }
    let first = confirm_worker_agent_modal(&mut state, &mut app);
    let out = run(&mut state, &mut app, &brigade, first, &catalog(), 1);
    let got = launches(&out.opens);
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].model.clone(), got[0].effort.clone()), (None, None));
}

#[test]
fn the_picker_opens_on_the_agent_chosen_last_time_with_that_agents_defaults() {
    let brigade = BrigadeConfig {
        worker_agent: WorkerAgentSetting::Select,
        ..configured()
    };
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row("dir", AgentKind::ClaudeCode)]);
    state.pending_membership = Some(PendingMembership::BrigadeKey);
    let resolve = |state: &mut EmporiumState, app: &mut App| {
        state.pending_membership = Some(PendingMembership::BrigadeKey);
        update(
            state,
            app,
            &brigade,
            Event::MembershipResolved {
                session_id: "dir".to_string(),
                membership: None,
                members: None,
                layout: None,
                names: Vec::new(),
            },
            now(),
        );
    };

    // Nothing stored: the Director's own product, as before.
    resolve(&mut state, &mut app);
    let Some(Modal::WorkerAgentPicker(picker)) = app.modal() else {
        panic!("expected the picker");
    };
    assert_eq!(picker.agent(), AgentKind::ClaudeCode);
    app.close_modal();

    // The stored choice comes in as an event at startup.
    update(
        &mut state,
        &mut app,
        &brigade,
        Event::LastWorkerAgentLoaded {
            agent: Some(AgentKind::Codex),
        },
        now(),
    );
    resolve(&mut state, &mut app);
    let Some(Modal::WorkerAgentPicker(picker)) = app.modal() else {
        panic!("expected the picker");
    };
    assert_eq!(
        picker.agent(),
        AgentKind::Codex,
        "the stored choice, not the Director's"
    );
    assert_eq!(picker.model_input(), "gpt-5.6-terra");
    assert_eq!(picker.effort_input(), "xhigh");
}

#[test]
fn confirming_stores_the_agent_once_and_a_value_loaded_at_start_is_never_written_back() {
    let brigade = BrigadeConfig {
        worker_agent: WorkerAgentSetting::Select,
        ..configured()
    };
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row("dir", AgentKind::ClaudeCode)]);
    let saves = |cmds: &[Cmd]| {
        cmds.iter()
            .filter_map(|cmd| match cmd {
                Cmd::Store(StoreIntent::SaveLastWorkerAgent { agent }) => Some(*agent),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    // Loading what the store holds writes nothing back.
    let cmds = update(
        &mut state,
        &mut app,
        &brigade,
        Event::LastWorkerAgentLoaded {
            agent: Some(AgentKind::ClaudeCode),
        },
        now(),
    );
    assert!(saves(&cmds).is_empty(), "{cmds:?}");
    let cmds = update(
        &mut state,
        &mut app,
        &brigade,
        Event::Resized {
            width: 120,
            height: 40,
        },
        now(),
    );
    assert!(saves(&cmds).is_empty(), "{cmds:?}");

    // Confirming the picker on the other agent stores it, once.
    state.pending_membership = Some(PendingMembership::BrigadeKey);
    update(
        &mut state,
        &mut app,
        &brigade,
        Event::MembershipResolved {
            session_id: "dir".to_string(),
            membership: None,
            members: None,
            layout: None,
            names: Vec::new(),
        },
        now(),
    );
    app.modal_toggle_agent();
    let key = Event::Input(InputEvent::Key(crate::input::KeyEvent::new(
        crate::input::KeyCode::Enter,
        crate::input::Modifiers::NONE,
    )));
    let cmds = update(&mut state, &mut app, &brigade, key, now());
    assert_eq!(saves(&cmds), [AgentKind::Codex], "{cmds:?}");
    let cmds = update(
        &mut state,
        &mut app,
        &brigade,
        Event::Resized {
            width: 120,
            height: 40,
        },
        now(),
    );
    assert!(saves(&cmds).is_empty(), "not again: {cmds:?}");

    // Confirming on the agent already stored writes nothing.
    state.pending_membership = Some(PendingMembership::BrigadeKey);
    let first = update(
        &mut state,
        &mut app,
        &brigade,
        Event::MembershipResolved {
            session_id: "dir".to_string(),
            membership: None,
            members: None,
            layout: None,
            names: Vec::new(),
        },
        now(),
    );
    assert!(first.is_empty());
    let key = Event::Input(InputEvent::Key(crate::input::KeyEvent::new(
        crate::input::KeyCode::Enter,
        crate::input::Modifiers::NONE,
    )));
    let cmds = update(&mut state, &mut app, &brigade, key, now());
    assert!(saves(&cmds).is_empty(), "{cmds:?}");
}

#[test]
fn other_worker_agent_settings_never_show_the_picker_and_use_the_resolved_agents_defaults() {
    for (setting, director, agent, model, effort) in [
        (
            WorkerAgentSetting::Inherit,
            AgentKind::Codex,
            AgentKind::Codex,
            "gpt-6.1-terra",
            "xhigh",
        ),
        (
            WorkerAgentSetting::Inherit,
            AgentKind::ClaudeCode,
            AgentKind::ClaudeCode,
            "opus",
            "low",
        ),
        (
            WorkerAgentSetting::Claude,
            AgentKind::Codex,
            AgentKind::ClaudeCode,
            "opus",
            "low",
        ),
        (
            WorkerAgentSetting::Codex,
            AgentKind::ClaudeCode,
            AgentKind::Codex,
            "gpt-6.1-terra",
            "xhigh",
        ),
    ] {
        let brigade = BrigadeConfig {
            worker_agent: setting,
            worker_model_codex: "terra".to_string(),
            ..configured()
        };
        let mut state = EmporiumState::new(PrefixKey::default());
        // Even with a stored "last chosen": it is the picker's, not these.
        state.last_worker_agent = Some(if agent == AgentKind::Codex {
            AgentKind::ClaudeCode
        } else {
            AgentKind::Codex
        });
        let mut app = App::new(vec![row("dir", director)]);
        state.pending_membership = Some(PendingMembership::BrigadeKey);
        let first = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MembershipResolved {
                session_id: "dir".to_string(),
                membership: None,
                members: None,
                layout: None,
                names: Vec::new(),
            },
            now(),
        );
        assert!(app.modal().is_none(), "{setting:?}: no picker");
        let out = run(&mut state, &mut app, &brigade, first, &catalog(), 1);
        let worker = launches(&out.opens)
            .into_iter()
            .find(|launch| launch.role == Some(BrigadeRole::Worker))
            .unwrap();
        assert_eq!(worker.agent, agent, "{setting:?}");
        assert_eq!(worker.model, s(model), "{setting:?}");
        assert_eq!(worker.effort, s(effort), "{setting:?}");
    }
}

// --- the catalog answer --------------------------------------------------------

#[test]
fn a_catalog_answer_with_nothing_waiting_is_ignored() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![]);
    let cmds = update(
        &mut state,
        &mut app,
        &BrigadeConfig::default(),
        Event::CodexCatalogRead {
            round: 1,
            result: catalog(),
        },
        now(),
    );
    assert!(cmds.is_empty(), "{cmds:?}");
    assert_eq!(state.status, None);
}

#[test]
fn two_workers_with_different_words_are_each_resolved_and_both_said() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![]);
    let brigade = BrigadeConfig::default();
    // Both are on the brigade's roster, as a creation always is.
    state
        .member_order
        .insert(1, vec!["worker-1".to_string(), "worker-2".to_string()]);
    let mut asks = 0;
    for (token, word) in [("worker-1", "terra"), ("worker-2", "gpt-9-mystery")] {
        let cmds = open_worker(
            &mut state,
            1,
            token,
            std::path::Path::new("/work"),
            WorkerChoice {
                agent: AgentKind::Codex,
                model: word,
                effort: "",
            },
            None,
        );
        asks += cmds
            .iter()
            .filter(|cmd| matches!(cmd, Cmd::ReadCodexCatalog { .. }))
            .count();
        assert!(
            !cmds
                .iter()
                .any(|cmd| matches!(cmd, Cmd::OpenEmbedded { .. })),
            "held until the catalog answers"
        );
    }
    assert_eq!(asks, 1, "the second finds a read already asked for");
    let round = held_round(&state);
    let released = update(
        &mut state,
        &mut app,
        &brigade,
        Event::CodexCatalogRead {
            round,
            result: catalog(),
        },
        now(),
    );
    let got = launches(&released);
    assert_eq!(
        got.iter().map(|l| l.model.clone()).collect::<Vec<_>>(),
        [s("gpt-6.1-terra"), s("gpt-9-mystery")]
    );
    let status = state.status.as_deref().unwrap();
    assert!(status.contains("terra → gpt-6.1-terra"), "{status}");
    assert!(status.contains("gpt-9-mystery"), "{status}");

    // The next creation asks again: nothing is kept between reads.
    let cmds = open_worker(
        &mut state,
        1,
        "worker-3",
        std::path::Path::new("/work"),
        WorkerChoice {
            agent: AgentKind::Codex,
            model: "terra",
            effort: "",
        },
        None,
    );
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::ReadCodexCatalog { .. }))
    );
}

#[test]
fn a_stream_recorded_before_the_effort_existed_still_loads_with_an_empty_one() {
    // `worker_effort` came after streams were being recorded; a capture without
    // it must replay, as a Worker created with no effort of its own.
    let formed = Event::BrigadeFormed {
        director_row_id: "dir".to_string(),
        name: "cell".to_string(),
        cwd: PathBuf::from("/work"),
        worker_agent: AgentKind::ClaudeCode,
        worker_model: "sonnet".to_string(),
        worker_effort: "high".to_string(),
        result: Ok((1, vec!["worker-1".to_string()])),
    };
    let mut json: serde_json::Value = serde_json::to_value(&formed).unwrap();
    let body = json
        .get_mut("BrigadeFormed")
        .and_then(|body| body.as_object_mut())
        .expect("externally tagged");
    assert!(body.remove("worker_effort").is_some());
    let loaded: Event = serde_json::from_value(json).unwrap();
    let Event::BrigadeFormed { worker_effort, .. } = loaded else {
        panic!("expected BrigadeFormed");
    };
    assert_eq!(worker_effort, "");

    let added = Event::WorkerAdded {
        brigade_id: 1,
        cwd: PathBuf::from("/work"),
        worker_agent: AgentKind::Codex,
        worker_model: "terra".to_string(),
        worker_effort: "high".to_string(),
        result: Ok("worker-2".to_string()),
    };
    let mut json = serde_json::to_value(&added).unwrap();
    json.get_mut("WorkerAdded")
        .and_then(|body| body.as_object_mut())
        .unwrap()
        .remove("worker_effort");
    let Event::WorkerAdded { worker_effort, .. } = serde_json::from_value(json).unwrap() else {
        panic!("expected WorkerAdded");
    };
    assert_eq!(worker_effort, "");
}

// --- a held creation is carried out only if it is still wanted ----------------

/// The round the held creations wait for.
fn held_round(state: &EmporiumState) -> u64 {
    state.codex_opens_awaiting_catalog[0].round
}

/// A Codex brigade formed up to the point where its Workers wait on the
/// catalog: the read has been asked for and not answered.
fn formation_awaiting_catalog(workers: usize) -> (EmporiumState, App, BrigadeConfig) {
    let brigade = BrigadeConfig {
        workers: workers as u32,
        worker_agent: WorkerAgentSetting::Codex,
        worker_model_codex: "terra".to_string(),
        ..configured()
    };
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row("dir", AgentKind::ClaudeCode)]);
    state.pending_membership = Some(PendingMembership::BrigadeKey);
    let mut cmds = update(
        &mut state,
        &mut app,
        &brigade,
        Event::MembershipResolved {
            session_id: "dir".to_string(),
            membership: None,
            members: None,
            layout: None,
            names: Vec::new(),
        },
        now(),
    );
    // The trust check, the store, the Director's spawn: answered as the shell
    // would, stopping at the catalog read.
    loop {
        let Some(cmd) = cmds.pop() else {
            panic!("the formation never asked for the catalog");
        };
        let events = match cmd {
            Cmd::ReadCodexCatalog { .. } => return (state, app, brigade),
            Cmd::CheckCodexTrust => vec![Event::CodexTrustChecked {
                primed: true,
                hook_launchable: true,
            }],
            Cmd::Store(StoreIntent::FormBrigade {
                director_row_id,
                name,
                cwd,
                worker_agent,
                worker_model,
                worker_effort,
                ..
            }) => vec![Event::BrigadeFormed {
                director_row_id,
                name,
                cwd,
                worker_agent,
                worker_model,
                worker_effort,
                result: Ok((1, (1..=workers).map(|n| format!("worker-{n}")).collect())),
            }],
            Cmd::OpenEmbedded { key, .. } => vec![Event::Spawned { key }],
            _ => Vec::new(),
        };
        for event in events {
            cmds.extend(update(&mut state, &mut app, &brigade, event, now()));
        }
    }
}

fn disbanded(brigade_id: BrigadeId) -> Event {
    Event::Disbanded {
        brigade_id,
        result: Ok((Default::default(), Default::default())),
    }
}

fn worker_key(brigade_id: BrigadeId, token: &str) -> SessionKey {
    SessionKey::new_worker(brigade_id, token)
}

/// Nothing `open_worker` registers for `token` may be left behind.
fn assert_nothing_registered(state: &EmporiumState, brigade_id: BrigadeId, token: &str) {
    let key = worker_key(brigade_id, token);
    assert!(
        !state.member_tokens.contains_key(&key),
        "member_tokens {token}"
    );
    assert!(
        !state.member_brigades.contains_key(&key),
        "member_brigades {token}"
    );
    assert!(
        !state.pending_opens.contains_key(&key),
        "pending_opens {token}"
    );
    assert!(!state.screens.contains_key(&key), "screens {token}");
    assert!(
        state.pending_kickoffs.iter().all(|k| k.key != key),
        "pending_kickoffs {token}"
    );
}

#[test]
fn a_disband_between_the_catalog_request_and_its_answer_opens_nothing() {
    let (mut state, mut app, brigade) = formation_awaiting_catalog(2);
    let round = held_round(&state);
    assert_eq!(state.codex_opens_awaiting_catalog.len(), 2);

    update(&mut state, &mut app, &brigade, disbanded(1), now());
    let released = update(
        &mut state,
        &mut app,
        &brigade,
        Event::CodexCatalogRead {
            round,
            result: catalog(),
        },
        now(),
    );

    assert!(launches(&released).is_empty(), "{released:?}");
    assert!(state.codex_opens_awaiting_catalog.is_empty());
    for token in ["worker-1", "worker-2"] {
        assert_nothing_registered(&state, 1, token);
    }
    assert_eq!(
        state.status.as_deref(),
        Some("brigade disbanded"),
        "a dropped creation says nothing: the operator asked for the disband"
    );
}

#[test]
fn a_successful_disband_drops_that_brigades_held_creations_at_once() {
    let (mut state, mut app, brigade) = formation_awaiting_catalog(2);
    update(&mut state, &mut app, &brigade, disbanded(1), now());
    // Before any answer: already gone, with what open_worker had registered.
    assert!(state.codex_opens_awaiting_catalog.is_empty());
    for token in ["worker-1", "worker-2"] {
        assert_nothing_registered(&state, 1, token);
    }
}

#[test]
fn an_add_worker_held_for_the_catalog_is_dropped_by_a_disband_too() {
    let brigade = BrigadeConfig {
        worker_agent: WorkerAgentSetting::Codex,
        worker_model_codex: "terra".to_string(),
        ..configured()
    };
    let mut state = EmporiumState::new(PrefixKey::default());
    state.stage = Stage::Brigade {
        id: 1,
        director: Some(SessionKey::from_id("dir")),
        panes: vec![SessionKey::from_id("dir")],
        focused: 0,
    };
    let mut app = App::new(vec![row("dir", AgentKind::ClaudeCode)]);
    let cmds = update(
        &mut state,
        &mut app,
        &brigade,
        Event::WorkerAdded {
            brigade_id: 1,
            cwd: PathBuf::from("/work"),
            worker_agent: AgentKind::Codex,
            worker_model: "terra".to_string(),
            worker_effort: "high".to_string(),
            result: Ok("worker-2".to_string()),
        },
        now(),
    );
    let Some(Cmd::ReadCodexCatalog { round }) = cmds
        .iter()
        .find(|cmd| matches!(cmd, Cmd::ReadCodexCatalog { .. }))
        .cloned()
    else {
        panic!("{cmds:?}");
    };

    update(&mut state, &mut app, &brigade, disbanded(1), now());
    let released = update(
        &mut state,
        &mut app,
        &brigade,
        Event::CodexCatalogRead {
            round,
            result: catalog(),
        },
        now(),
    );
    assert!(launches(&released).is_empty(), "{released:?}");
    assert_nothing_registered(&state, 1, "worker-2");
}

#[test]
fn a_disband_still_in_flight_when_the_answer_arrives_drops_the_creation() {
    let (mut state, mut app, brigade) = formation_awaiting_catalog(1);
    let round = held_round(&state);
    // The operator confirmed the disband; the store has not answered yet.
    state.disbanding.insert(1);
    let released = update(
        &mut state,
        &mut app,
        &brigade,
        Event::CodexCatalogRead {
            round,
            result: catalog(),
        },
        now(),
    );
    assert!(launches(&released).is_empty(), "{released:?}");
    assert_nothing_registered(&state, 1, "worker-1");
}

#[test]
fn a_member_removed_from_the_roster_while_its_creation_is_held_is_not_opened() {
    let (mut state, mut app, brigade) = formation_awaiting_catalog(2);
    let round = held_round(&state);
    // What a dismissal forgets (`forget_dismissed_member`): its place in the
    // join order. There is no tile to dismiss a Worker that has no pane, so this
    // is the core's own guard, not a reachable gesture.
    forget_dismissed_member(&mut state, 1, "worker-2");
    let released = update(
        &mut state,
        &mut app,
        &brigade,
        Event::CodexCatalogRead {
            round,
            result: catalog(),
        },
        now(),
    );
    let got = launches(&released);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        released.iter().all(|cmd| match cmd {
            Cmd::OpenEmbedded { key, .. } => *key == worker_key(1, "worker-1"),
            _ => true,
        }),
        "{released:?}"
    );
    assert_nothing_registered(&state, 1, "worker-2");
    assert!(state.pending_opens.contains_key(&worker_key(1, "worker-1")));
}

#[test]
fn a_member_that_already_has_a_pane_is_not_opened_a_second_time() {
    let (mut state, mut app, brigade) = formation_awaiting_catalog(1);
    let round = held_round(&state);
    state
        .screens
        .insert(worker_key(1, "worker-1"), Screen::new(24, 80));
    let released = update(
        &mut state,
        &mut app,
        &brigade,
        Event::CodexCatalogRead {
            round,
            result: catalog(),
        },
        now(),
    );
    assert!(launches(&released).is_empty(), "{released:?}");
}

#[test]
fn disbanding_one_brigade_leaves_another_brigades_held_creations_to_be_released() {
    let brigade = BrigadeConfig::default();
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![]);
    for brigade_id in [1, 2] {
        state
            .member_order
            .insert(brigade_id, vec!["worker-1".to_string()]);
        open_worker(
            &mut state,
            brigade_id,
            "worker-1",
            std::path::Path::new("/work"),
            WorkerChoice {
                agent: AgentKind::Codex,
                model: "terra",
                effort: "",
            },
            None,
        );
    }
    assert_eq!(state.codex_opens_awaiting_catalog.len(), 2);
    let round = held_round(&state);

    update(&mut state, &mut app, &brigade, disbanded(1), now());
    assert_eq!(state.codex_opens_awaiting_catalog.len(), 1);
    assert_nothing_registered(&state, 1, "worker-1");

    let released = update(
        &mut state,
        &mut app,
        &brigade,
        Event::CodexCatalogRead {
            round,
            result: catalog(),
        },
        now(),
    );
    let got = launches(&released);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].model, s("gpt-6.1-terra"));
    assert!(state.pending_opens.contains_key(&worker_key(2, "worker-1")));
}

#[test]
fn a_second_read_is_asked_for_when_the_first_brigades_creations_were_dropped_and_a_late_answer_is_harmless()
 {
    let brigade = BrigadeConfig::default();
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![]);
    let choice = || WorkerChoice {
        agent: AgentKind::Codex,
        model: "terra",
        effort: "",
    };
    state.member_order.insert(1, vec!["worker-1".to_string()]);
    state.member_order.insert(2, vec!["worker-1".to_string()]);
    let dir = std::path::Path::new("/w");
    let first = open_worker(&mut state, 1, "worker-1", dir, choice(), None);
    assert_eq!(first, vec![Cmd::ReadCodexCatalog { round: 1 }]);
    update(&mut state, &mut app, &brigade, disbanded(1), now());
    // Brigade 1's answer is still on its way, and brigade 2 now finds nothing
    // held: it asks again rather than wait on a read that may be for nobody.
    let second = open_worker(&mut state, 2, "worker-1", dir, choice(), None);
    assert_eq!(second, vec![Cmd::ReadCodexCatalog { round: 2 }]);

    let answer = |round| Event::CodexCatalogRead {
        round,
        result: catalog(),
    };
    // Round 1's answer is late: it is for nobody, and brigade 2 keeps waiting
    // for its own.
    let late = update(&mut state, &mut app, &brigade, answer(1), now());
    assert!(late.is_empty(), "{late:?}");
    assert_eq!(state.codex_opens_awaiting_catalog.len(), 1);
    let released = update(&mut state, &mut app, &brigade, answer(2), now());
    assert_eq!(launches(&released).len(), 1);
}

#[test]
fn an_answer_that_never_arrives_is_given_up_on_and_the_workers_open_as_for_an_unreadable_catalog() {
    let (mut state, mut app, brigade) = formation_awaiting_catalog(2);
    let round = held_round(&state);
    let tick = |state: &mut EmporiumState, app: &mut App, at: Instant| {
        update(
            state,
            app,
            &brigade,
            Event::Tick {
                relay: Vec::new(),
                brigades: Vec::new(),
            },
            at,
        )
    };
    let start = now();
    // The first tick that sees them held starts the clock.
    assert!(launches(&tick(&mut state, &mut app, start)).is_empty());
    let almost = start + CODEX_CATALOG_GIVE_UP - std::time::Duration::from_secs(1);
    assert!(
        launches(&tick(&mut state, &mut app, almost)).is_empty(),
        "still waiting"
    );
    assert_eq!(state.codex_opens_awaiting_catalog.len(), 2);

    let cmds = tick(
        &mut state,
        &mut app,
        start + CODEX_CATALOG_GIVE_UP + std::time::Duration::from_secs(1),
    );
    let got = launches(&cmds);
    assert_eq!(got.len(), 2, "{cmds:?}");
    // Case 5 of the rules: a bare family word, no catalog, so no -m — said.
    assert!(got.iter().all(|l| l.model.is_none()), "{got:?}");
    let status = state.status.as_deref().unwrap();
    assert!(status.contains("no answer"), "{status}");
    assert!(
        status.contains("Codex uses its own default model"),
        "{status}"
    );
    assert!(state.codex_opens_awaiting_catalog.is_empty());

    // The answer that was only late finds nothing waiting.
    let late = update(
        &mut state,
        &mut app,
        &brigade,
        Event::CodexCatalogRead {
            round,
            result: catalog(),
        },
        now(),
    );
    assert!(late.is_empty(), "{late:?}");
}

#[test]
fn workers_of_a_brigade_disbanded_before_its_director_spawned_are_not_opened() {
    // Not the catalog: the older deferred launch of the same shape. The
    // Director's spawn is what opens the Workers, and a disband can land between
    // the open being asked for and the spawn being reported.
    let brigade = BrigadeConfig {
        workers: 2,
        ..configured()
    };
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row("dir", AgentKind::ClaudeCode)]);
    update(
        &mut state,
        &mut app,
        &brigade,
        Event::BrigadeFormed {
            director_row_id: "dir".to_string(),
            name: "cell".to_string(),
            cwd: PathBuf::from("/work"),
            worker_agent: AgentKind::ClaudeCode,
            worker_model: "opus".to_string(),
            worker_effort: "low".to_string(),
            result: Ok((1, vec!["worker-1".to_string(), "worker-2".to_string()])),
        },
        now(),
    );
    update(&mut state, &mut app, &brigade, disbanded(1), now());
    let cmds = update(
        &mut state,
        &mut app,
        &brigade,
        Event::Spawned {
            key: SessionKey::from_id("dir"),
        },
        now(),
    );
    assert!(launches(&cmds).is_empty(), "{cmds:?}");
    assert_nothing_registered(&state, 1, "worker-1");
}

// --- one rule for every value: trimmed, and nothing left means no flag ----------

#[test]
fn a_padded_or_blank_worker_value_is_trimmed_or_passes_no_flag_on_every_path() {
    for (agent, setting) in [
        (AgentKind::ClaudeCode, WorkerAgentSetting::Claude),
        (AgentKind::Codex, WorkerAgentSetting::Codex),
    ] {
        // Padded: trimmed, passed as is.
        let padded = BrigadeConfig {
            worker_agent: setting,
            worker_model: "  opus ".to_string(),
            worker_effort: "\tlow\n".to_string(),
            worker_model_codex: " gpt-5.6-terra ".to_string(),
            worker_effort_codex: " xhigh ".to_string(),
            ..configured()
        };
        let (_, _, out) = form(&padded, AgentKind::ClaudeCode, &catalog());
        let worker = launches(&out.opens)
            .into_iter()
            .find(|l| l.role == Some(BrigadeRole::Worker))
            .unwrap();
        let (model, effort) = match agent {
            AgentKind::ClaudeCode => ("opus", "low"),
            AgentKind::Codex => ("gpt-5.6-terra", "xhigh"),
        };
        assert_eq!(worker.model, s(model), "{agent:?}");
        assert_eq!(worker.effort, s(effort), "{agent:?}");

        // Nothing but whitespace: no flag, and for Codex no catalog read.
        let blank = BrigadeConfig {
            worker_agent: setting,
            worker_model: "   ".to_string(),
            worker_effort: "\t".to_string(),
            worker_model_codex: "  ".to_string(),
            worker_effort_codex: " \n".to_string(),
            ..configured()
        };
        let (_, _, out) = form(&blank, AgentKind::ClaudeCode, &catalog());
        let worker = launches(&out.opens)
            .into_iter()
            .find(|l| l.role == Some(BrigadeRole::Worker))
            .unwrap();
        assert_eq!((worker.model, worker.effort), (None, None), "{agent:?}");
        assert_eq!(out.catalog_reads, 0, "{agent:?}: nothing to resolve");
    }
}

#[test]
fn the_goinkyo_follows_the_same_rule() {
    let spawn = |brigade: &BrigadeConfig| {
        let mut state = EmporiumState::new(PrefixKey::default());
        let cmds = update_goinkyo_awaiting_spawn(
            &mut state,
            brigade,
            GoinkyoObservation::AwaitingSpawn(GoinkyoSpawnCandidate {
                brigade_id: 1,
                cwd: PathBuf::from("/work"),
                roster: Vec::new(),
            }),
            now(),
        );
        launches(&cmds).remove(0)
    };
    let padded = spawn(&BrigadeConfig {
        goinkyo_model: " haiku ".to_string(),
        goinkyo_effort: "\tmedium ".to_string(),
        ..configured()
    });
    assert_eq!((padded.model, padded.effort), (s("haiku"), s("medium")));
    let blank = spawn(&BrigadeConfig {
        goinkyo_model: "  ".to_string(),
        goinkyo_effort: "\n".to_string(),
        ..configured()
    });
    assert_eq!((blank.model, blank.effort), (None, None));
}

#[test]
fn the_pickers_blank_and_padded_inputs_follow_the_same_rule() {
    let brigade = BrigadeConfig {
        worker_agent: WorkerAgentSetting::Select,
        ..configured()
    };
    let confirm = |model: &str, effort: &str| {
        let mut state = EmporiumState::new(PrefixKey::default());
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(SessionKey::from_id("dir")),
            panes: vec![SessionKey::from_id("dir")],
            focused: 0,
        };
        let mut app = App::new(vec![row("dir", AgentKind::ClaudeCode)]);
        add_worker(&mut state, &mut app, &brigade);
        // Replace both fields' text.
        for _ in 0..10 {
            app.modal_backspace();
        }
        for c in model.chars() {
            app.modal_push_char(c);
        }
        app.modal_complete_candidate();
        for _ in 0..10 {
            app.modal_backspace();
        }
        for c in effort.chars() {
            app.modal_push_char(c);
        }
        let first = confirm_worker_agent_modal(&mut state, &mut app);
        let out = run(&mut state, &mut app, &brigade, first, &catalog(), 1);
        launches(&out.opens).remove(0)
    };
    let padded = confirm("  opus  ", " low ");
    assert_eq!((padded.model, padded.effort), (s("opus"), s("low")));
    let blank = confirm("   ", "  ");
    assert_eq!((blank.model, blank.effort), (None, None));
}

#[test]
fn the_picker_opens_with_trimmed_defaults() {
    let brigade = BrigadeConfig {
        worker_agent: WorkerAgentSetting::Select,
        worker_model: "  opus ".to_string(),
        worker_effort: "   ".to_string(),
        ..configured()
    };
    let mut state = EmporiumState::new(PrefixKey::default());
    state.stage = Stage::Brigade {
        id: 1,
        director: Some(SessionKey::from_id("dir")),
        panes: vec![SessionKey::from_id("dir")],
        focused: 0,
    };
    let mut app = App::new(vec![row("dir", AgentKind::ClaudeCode)]);
    add_worker(&mut state, &mut app, &brigade);
    let Some(Modal::WorkerAgentPicker(picker)) = app.modal() else {
        panic!("expected the picker");
    };
    assert_eq!(picker.model_input(), "opus");
    assert_eq!(picker.effort_input(), "");
}

// --- a member's pane is a pane under any key ------------------------------------

/// Re-stage brigade 1 from a roster in which `worker-1` already has a session
/// `w1` (so staging resumes it), the way pressing Enter on the Director would.
fn restage_with_worker_resumed(state: &mut EmporiumState, app: &mut App, brigade: &BrigadeConfig) {
    *app = App::new(vec![
        row("dir", AgentKind::ClaudeCode),
        row("w1", AgentKind::Codex),
    ]);
    state.pending_membership = Some(PendingMembership::Activate);
    let cmds = update(
        state,
        app,
        brigade,
        Event::MembershipResolved {
            session_id: "dir".to_string(),
            membership: Some((1, "director".to_string(), BrigadeRole::Director)),
            members: Some(vec![
                ("director".to_string(), BrigadeRole::Director, s("dir")),
                ("worker-1".to_string(), BrigadeRole::Worker, s("w1")),
            ]),
            layout: None,
            names: Vec::new(),
        },
        now(),
    );
    for cmd in cmds {
        if let Cmd::OpenEmbedded { key, .. } = cmd {
            update(state, app, brigade, Event::Spawned { key }, now());
        }
    }
}

#[test]
fn a_member_whose_pane_arrived_under_another_key_is_not_created_a_second_time() {
    // Reproduced with real events: the formation's Worker is held for the
    // catalog; the brigade is re-staged from a roster that already knows its
    // session, which resumes it under that session's key; the answer then finds
    // no pane under the synthetic key the held creation was made for.
    let (mut state, mut app, brigade) = formation_awaiting_catalog(1);
    let round = held_round(&state);
    restage_with_worker_resumed(&mut state, &mut app, &brigade);
    assert!(
        state.screens.contains_key(&SessionKey::from_id("w1")),
        "the resumed pane exists under the session's key"
    );

    let released = update(
        &mut state,
        &mut app,
        &brigade,
        Event::CodexCatalogRead {
            round,
            result: catalog(),
        },
        now(),
    );
    assert!(
        launches(&released).is_empty(),
        "a second, id-less creation for a member that has a session: {released:?}"
    );
    // The live pane's own mappings are not touched by the undo of the dropped
    // creation, and the dropped one's are gone.
    let live = SessionKey::from_id("w1");
    assert_eq!(
        state.member_tokens.get(&live).map(String::as_str),
        Some("worker-1")
    );
    assert_eq!(state.member_brigades.get(&live), Some(&1));
    assert!(state.screens.contains_key(&live));
    assert_nothing_registered(&state, 1, "worker-1");
}

#[test]
fn no_creation_is_held_or_requested_for_a_member_that_already_has_a_pane_under_any_key() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let live = SessionKey::from_id("w1");
    state.screens.insert(live.clone(), Screen::new(24, 80));
    state
        .member_tokens
        .insert(live.clone(), "worker-1".to_string());
    state.member_brigades.insert(live.clone(), 1);
    state.member_order.insert(1, vec!["worker-1".to_string()]);
    for agent in [AgentKind::Codex, AgentKind::ClaudeCode] {
        let cmds = open_worker(
            &mut state,
            1,
            "worker-1",
            std::path::Path::new("/work"),
            WorkerChoice {
                agent,
                model: "terra",
                effort: "",
            },
            None,
        );
        assert!(cmds.is_empty(), "{agent:?}: {cmds:?}");
    }
    assert!(state.codex_opens_awaiting_catalog.is_empty());
    assert_nothing_registered(&state, 1, "worker-1");
    assert!(
        state.screens.contains_key(&live),
        "the live pane is untouched"
    );
}

#[test]
fn a_member_whose_creation_is_already_held_or_in_flight_is_not_created_again() {
    let mut state = EmporiumState::new(PrefixKey::default());
    state.member_order.insert(1, vec!["worker-1".to_string()]);
    let ask = |state: &mut EmporiumState, agent| {
        open_worker(
            state,
            1,
            "worker-1",
            std::path::Path::new("/work"),
            WorkerChoice {
                agent,
                model: "terra",
                effort: "",
            },
            None,
        )
    };
    // Held for the catalog: a second request for the same member adds nothing,
    // and does not ask for another read.
    assert_eq!(ask(&mut state, AgentKind::Codex).len(), 1);
    assert!(ask(&mut state, AgentKind::Codex).is_empty());
    assert_eq!(state.codex_opens_awaiting_catalog.len(), 1);

    // In flight (a Claude creation already issued, no pane yet): same.
    let mut state = EmporiumState::new(PrefixKey::default());
    state.member_order.insert(1, vec!["worker-1".to_string()]);
    assert_eq!(ask(&mut state, AgentKind::ClaudeCode).len(), 1);
    assert!(ask(&mut state, AgentKind::ClaudeCode).is_empty());
}

// --- one answer, one round ------------------------------------------------------

#[test]
fn creations_requested_while_a_round_is_waiting_join_it_and_share_its_one_read() {
    let mut state = EmporiumState::new(PrefixKey::default());
    state
        .member_order
        .insert(1, vec!["worker-1".to_string(), "worker-2".to_string()]);
    let mut reads = Vec::new();
    for token in ["worker-1", "worker-2"] {
        reads.extend(open_worker(
            &mut state,
            1,
            token,
            std::path::Path::new("/work"),
            WorkerChoice {
                agent: AgentKind::Codex,
                model: "terra",
                effort: "",
            },
            None,
        ));
    }
    assert_eq!(reads, vec![Cmd::ReadCodexCatalog { round: 1 }]);
    let rounds: Vec<u64> = state
        .codex_opens_awaiting_catalog
        .iter()
        .map(|held| held.round)
        .collect();
    assert_eq!(rounds, [1, 1], "both wait for the round that will answer");
}

/// A held creation for brigade `brigade_id`, asked for in whatever round the
/// state decides; returns the round.
fn hold_one(state: &mut EmporiumState, brigade_id: BrigadeId) -> u64 {
    state
        .member_order
        .insert(brigade_id, vec!["worker-1".to_string()]);
    let cmds = open_worker(
        state,
        brigade_id,
        "worker-1",
        std::path::Path::new("/work"),
        WorkerChoice {
            agent: AgentKind::Codex,
            model: "terra",
            effort: "",
        },
        None,
    );
    let [Cmd::ReadCodexCatalog { round }] = cmds[..] else {
        panic!("expected one read: {cmds:?}");
    };
    round
}

fn answer(state: &mut EmporiumState, app: &mut App, round: u64) -> Vec<Cmd> {
    update(
        state,
        app,
        &BrigadeConfig::default(),
        Event::CodexCatalogRead {
            round,
            result: catalog(),
        },
        now(),
    )
}

fn tick_at(state: &mut EmporiumState, app: &mut App, at: Instant) -> Vec<Cmd> {
    update(
        state,
        app,
        &BrigadeConfig::default(),
        Event::Tick {
            relay: Vec::new(),
            brigades: Vec::new(),
        },
        at,
    )
}

#[test]
fn a_late_answer_for_a_round_that_was_given_up_does_not_release_the_next_round() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![]);
    let a = hold_one(&mut state, 1);
    let start = now();
    tick_at(&mut state, &mut app, start);
    let timed_out = tick_at(
        &mut state,
        &mut app,
        start + CODEX_CATALOG_GIVE_UP + std::time::Duration::from_secs(1),
    );
    assert_eq!(launches(&timed_out).len(), 1, "round A given up");

    let b = hold_one(&mut state, 2);
    assert_ne!(a, b);
    // A's answer was only late: B is still waiting and nothing opens.
    let late = answer(&mut state, &mut app, a);
    assert!(late.is_empty(), "{late:?}");
    assert_eq!(state.codex_opens_awaiting_catalog.len(), 1);
    // B's own answer releases B.
    let released = answer(&mut state, &mut app, b);
    assert_eq!(launches(&released).len(), 1);
    assert!(state.codex_opens_awaiting_catalog.is_empty());
}

#[test]
fn a_late_answer_for_a_round_a_disband_emptied_does_not_release_the_next_round() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![]);
    let a = hold_one(&mut state, 1);
    update(
        &mut state,
        &mut app,
        &BrigadeConfig::default(),
        disbanded(1),
        now(),
    );
    let b = hold_one(&mut state, 2);
    assert_ne!(a, b);

    let late = answer(&mut state, &mut app, a);
    assert!(late.is_empty(), "{late:?}");
    assert_eq!(state.codex_opens_awaiting_catalog.len(), 1, "B still held");
    let released = answer(&mut state, &mut app, b);
    assert_eq!(launches(&released).len(), 1);
}

#[test]
fn two_rounds_answered_in_reverse_order_each_release_only_their_own() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![]);
    let a = hold_one(&mut state, 1);
    update(
        &mut state,
        &mut app,
        &BrigadeConfig::default(),
        disbanded(1),
        now(),
    );
    let b = hold_one(&mut state, 2);

    // The later round answers first.
    let released = answer(&mut state, &mut app, b);
    assert_eq!(launches(&released).len(), 1);
    assert!(state.pending_opens.contains_key(&worker_key(2, "worker-1")));
    // The earlier one's answer then changes nothing.
    let late = answer(&mut state, &mut app, a);
    assert!(late.is_empty(), "{late:?}");
}

#[test]
fn an_answer_for_a_round_nobody_asked_for_changes_nothing() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![]);
    let a = hold_one(&mut state, 1);
    for unknown in [0, a + 1, 99] {
        let cmds = answer(&mut state, &mut app, unknown);
        assert!(cmds.is_empty(), "round {unknown}: {cmds:?}");
        assert_eq!(
            state.codex_opens_awaiting_catalog.len(),
            1,
            "round {unknown}"
        );
        assert_eq!(state.status, None, "round {unknown}");
    }
    assert_eq!(launches(&answer(&mut state, &mut app, a)).len(), 1);
}

#[test]
fn giving_up_on_one_round_does_not_start_the_next_rounds_clock() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![]);
    hold_one(&mut state, 1);
    let start = now();
    let step = std::time::Duration::from_secs;
    tick_at(&mut state, &mut app, start);
    let t_a = start + CODEX_CATALOG_GIVE_UP + step(1);
    assert_eq!(launches(&tick_at(&mut state, &mut app, t_a)).len(), 1);

    // B asks right after A was given up. A's clock must not carry over: B is
    // still held a moment before ITS give-up, however old A's clock was.
    hold_one(&mut state, 2);
    tick_at(&mut state, &mut app, t_a + step(1));
    let almost = t_a + step(1) + CODEX_CATALOG_GIVE_UP - step(1);
    assert!(launches(&tick_at(&mut state, &mut app, almost)).is_empty());
    assert_eq!(state.codex_opens_awaiting_catalog.len(), 1);
    let at = t_a + step(1) + CODEX_CATALOG_GIVE_UP + step(1);
    assert_eq!(launches(&tick_at(&mut state, &mut app, at)).len(), 1);
}

#[test]
fn a_capture_made_before_the_round_existed_loads_as_round_zero_and_releases_nothing() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![]);
    let round = hold_one(&mut state, 1);
    let json = r#"{"CodexCatalogRead":{"result":{"Err":"gone"}}}"#;
    let old: Event = serde_json::from_str(json).unwrap();
    let Event::CodexCatalogRead { round: loaded, .. } = &old else {
        panic!("expected CodexCatalogRead");
    };
    assert_eq!(*loaded, 0);
    assert_ne!(round, 0, "rounds count from 1");
    let cmds = update(&mut state, &mut app, &BrigadeConfig::default(), old, now());
    assert!(cmds.is_empty(), "{cmds:?}");
    assert_eq!(state.codex_opens_awaiting_catalog.len(), 1);
}

#[test]
fn the_formations_workers_skip_a_member_that_already_has_a_pane_when_the_director_arrives() {
    // The Workers of a formation are opened by the Director's spawn. If one of
    // them has a pane by then, under another key, it is not created again.
    let brigade = BrigadeConfig {
        workers: 2,
        ..configured()
    };
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row("dir", AgentKind::ClaudeCode)]);
    update(
        &mut state,
        &mut app,
        &brigade,
        Event::BrigadeFormed {
            director_row_id: "dir".to_string(),
            name: "cell".to_string(),
            cwd: PathBuf::from("/work"),
            worker_agent: AgentKind::ClaudeCode,
            worker_model: "opus".to_string(),
            worker_effort: "low".to_string(),
            result: Ok((1, vec!["worker-1".to_string(), "worker-2".to_string()])),
        },
        now(),
    );
    let live = SessionKey::from_id("w1");
    state.screens.insert(live.clone(), Screen::new(24, 80));
    state
        .member_tokens
        .insert(live.clone(), "worker-1".to_string());
    state.member_brigades.insert(live, 1);

    let cmds = update(
        &mut state,
        &mut app,
        &brigade,
        Event::Spawned {
            key: SessionKey::from_id("dir"),
        },
        now(),
    );
    let opened: Vec<_> = cmds
        .iter()
        .filter_map(|cmd| match cmd {
            Cmd::OpenEmbedded {
                brigade: Some((_, token, _)),
                ..
            } => Some(token.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(opened, ["worker-2"]);
}

// --- a member whose open is already in flight --------------------------------

/// Re-stage brigade 1 from a roster in which `worker-1` has session `w1`,
/// answering nothing: the resume it issues stays in flight. Returns what the
/// staging issued.
fn restage_leaving_the_resume_in_flight(
    state: &mut EmporiumState,
    app: &mut App,
    brigade: &BrigadeConfig,
) -> Vec<Cmd> {
    restage_with_roster_tail(state, app, brigade, Vec::new())
}

/// The same, with more of the store's roster after `worker-1`.
fn restage_with_roster_tail(
    state: &mut EmporiumState,
    app: &mut App,
    brigade: &BrigadeConfig,
    tail: Vec<(String, BrigadeRole, Option<String>)>,
) -> Vec<Cmd> {
    *app = App::new(vec![
        row("dir", AgentKind::ClaudeCode),
        row("w1", AgentKind::Codex),
    ]);
    state.pending_membership = Some(PendingMembership::Activate);
    let mut roster = vec![
        ("director".to_string(), BrigadeRole::Director, s("dir")),
        ("worker-1".to_string(), BrigadeRole::Worker, s("w1")),
    ];
    roster.extend(tail);
    update(
        state,
        app,
        brigade,
        Event::MembershipResolved {
            session_id: "dir".to_string(),
            membership: Some((1, "director".to_string(), BrigadeRole::Director)),
            members: Some(roster),
            layout: None,
            names: Vec::new(),
        },
        now(),
    )
}

fn opens_for(cmds: &[Cmd], token: &str) -> usize {
    cmds.iter()
        .filter(|cmd| matches!(held_creation_member_of(cmd), Some((_, t)) if t == token))
        .count()
}

fn held_creation_member_of(cmd: &Cmd) -> Option<(BrigadeId, &str)> {
    match cmd {
        Cmd::OpenEmbedded {
            brigade: Some((brigade_id, token, _)),
            ..
        } => Some((*brigade_id, token.as_str())),
        _ => None,
    }
}

#[test]
fn a_held_creation_is_not_released_beside_a_resume_that_is_still_in_flight() {
    // Reproduced with real events, `Spawned` held back until after the answer:
    // the formation's Worker is held for the catalog; a re-stage resumes it
    // under its session's key (issued, not yet spawned); the answer arrives.
    let (mut state, mut app, brigade) = formation_awaiting_catalog(1);
    let round = held_round(&state);
    let staged = restage_leaving_the_resume_in_flight(&mut state, &mut app, &brigade);
    assert_eq!(opens_for(&staged, "worker-1"), 1, "the resume: {staged:?}");
    let live = SessionKey::from_id("w1");
    assert!(state.pending_opens.contains_key(&live));
    assert!(!state.screens.contains_key(&live), "not spawned yet");

    let released = update(
        &mut state,
        &mut app,
        &brigade,
        Event::CodexCatalogRead {
            round,
            result: catalog(),
        },
        now(),
    );
    assert!(
        launches(&released).is_empty(),
        "a fresh session beside the in-flight resume: {released:?}"
    );
    // Only the creation's own placeholder bookkeeping went; the resume's stays.
    assert_nothing_registered(&state, 1, "worker-1");
    assert!(state.pending_opens.contains_key(&live));
    assert_eq!(
        state.member_tokens.get(&live).map(String::as_str),
        Some("worker-1")
    );

    // And the resume then lands as an ordinary arrival.
    update(
        &mut state,
        &mut app,
        &brigade,
        Event::Spawned { key: live.clone() },
        now(),
    );
    assert!(state.screens.contains_key(&live));
    assert!(state.pending_opens.is_empty());
}

#[test]
fn the_formations_workers_skip_a_member_whose_resume_is_in_flight_when_the_director_arrives() {
    let brigade = BrigadeConfig {
        workers: 2,
        ..configured()
    };
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row("dir", AgentKind::ClaudeCode)]);
    update(
        &mut state,
        &mut app,
        &brigade,
        Event::BrigadeFormed {
            director_row_id: "dir".to_string(),
            name: "cell".to_string(),
            cwd: PathBuf::from("/work"),
            worker_agent: AgentKind::ClaudeCode,
            worker_model: "opus".to_string(),
            worker_effort: "low".to_string(),
            result: Ok((1, vec!["worker-1".to_string(), "worker-2".to_string()])),
        },
        now(),
    );
    // The Director has been asked for and has not arrived; a re-stage meanwhile
    // resumes worker-1 under its session key and leaves it in flight.
    // The store's roster has both Workers; worker-2 has no session yet.
    let staged = restage_with_roster_tail(
        &mut state,
        &mut app,
        &brigade,
        vec![("worker-2".to_string(), BrigadeRole::Worker, None)],
    );
    assert_eq!(opens_for(&staged, "worker-1"), 1);
    assert_eq!(opens_for(&staged, "worker-2"), 1, "created by the staging");
    assert_eq!(
        opens_for(&staged, "director"),
        0,
        "the Director's own open is in flight too: {staged:?}"
    );

    let cmds = update(
        &mut state,
        &mut app,
        &brigade,
        Event::Spawned {
            key: SessionKey::from_id("dir"),
        },
        now(),
    );
    // Neither Worker is opened a second time by the Director's arrival.
    assert_eq!(opens_for(&cmds, "worker-1"), 0, "{cmds:?}");
    assert_eq!(opens_for(&cmds, "worker-2"), 0, "{cmds:?}");
}

#[test]
fn a_resume_that_fails_leaves_nothing_that_blocks_the_member_afterwards() {
    let (mut state, mut app, brigade) = formation_awaiting_catalog(1);
    let round = held_round(&state);
    restage_leaving_the_resume_in_flight(&mut state, &mut app, &brigade);
    let released = update(
        &mut state,
        &mut app,
        &brigade,
        Event::CodexCatalogRead {
            round,
            result: catalog(),
        },
        now(),
    );
    assert!(launches(&released).is_empty());

    // The resume fails to start.
    update(
        &mut state,
        &mut app,
        &brigade,
        Event::SpawnFailed {
            key: SessionKey::from_id("w1"),
            error: "refused".to_string(),
        },
        now(),
    );
    assert!(
        state.pending_opens.is_empty(),
        "{:?}",
        state.pending_opens.keys()
    );
    assert!(state.codex_opens_awaiting_catalog.is_empty());

    // A later re-stage resumes it again...
    let again = restage_leaving_the_resume_in_flight(&mut state, &mut app, &brigade);
    assert_eq!(opens_for(&again, "worker-1"), 1, "{again:?}");

    // ...and a creation for it is possible once that fails too.
    update(
        &mut state,
        &mut app,
        &brigade,
        Event::SpawnFailed {
            key: SessionKey::from_id("w1"),
            error: "refused".to_string(),
        },
        now(),
    );
    let created = open_worker(
        &mut state,
        1,
        "worker-1",
        std::path::Path::new("/work"),
        WorkerChoice {
            agent: AgentKind::ClaudeCode,
            model: "opus",
            effort: "",
        },
        None,
    );
    assert_eq!(opens_for(&created, "worker-1"), 1, "{created:?}");
}

#[test]
fn a_member_with_an_open_in_flight_is_not_created_again_by_restage_add_worker_or_formation() {
    let brigade = configured();
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![]);
    state.member_order.insert(1, vec!["worker-1".to_string()]);

    // Re-stage twice: the second issues no second open for the same key and
    // does not report that nothing could be opened.
    let first = restage_leaving_the_resume_in_flight(&mut state, &mut app, &brigade);
    assert_eq!(opens_for(&first, "worker-1"), 1);
    let second = restage_leaving_the_resume_in_flight(&mut state, &mut app, &brigade);
    assert_eq!(opens_for(&second, "worker-1"), 0, "{second:?}");
    assert_eq!(opens_for(&second, "director"), 0, "{second:?}");
    assert_ne!(
        state.status.as_deref(),
        Some("no brigade members could be opened")
    );
    assert!(
        matches!(state.stage, Stage::Brigade { id: 1, .. }),
        "still staged"
    );

    // An addition for the same member (as a store reporting a token again would
    // say): nothing issued.
    let added = update(
        &mut state,
        &mut app,
        &brigade,
        Event::WorkerAdded {
            brigade_id: 1,
            cwd: PathBuf::from("/work"),
            worker_agent: AgentKind::Codex,
            worker_model: "terra".to_string(),
            worker_effort: String::new(),
            result: Ok("worker-1".to_string()),
        },
        now(),
    );
    assert!(
        added
            .iter()
            .all(|cmd| !matches!(cmd, Cmd::OpenEmbedded { .. } | Cmd::ReadCodexCatalog { .. })),
        "{added:?}"
    );
    assert!(state.codex_opens_awaiting_catalog.is_empty());
}

#[test]
fn placeholder_keys_name_one_member_each() {
    // The ownership maps are overwritten for a key before its screen is looked
    // at (`open_worker`, `stage_brigade`). That is safe only if no other
    // member's pane can sit under the key; the key is built from the brigade id
    // and the token, so it is shared only by one member's own successive
    // creations.
    let keys = [
        SessionKey::new_worker(1, "worker-1"),
        SessionKey::new_worker(1, "worker-2"),
        SessionKey::new_worker(2, "worker-1"),
        SessionKey::new_worker(11, "worker-1"),
        SessionKey::new_worker(1, "goinkyo"),
    ];
    for (i, a) in keys.iter().enumerate() {
        for b in &keys[i + 1..] {
            assert_ne!(a, b);
        }
    }
    assert_eq!(
        SessionKey::new_worker(1, "worker-1"),
        SessionKey::new_worker(1, "worker-1")
    );
    assert_eq!(
        SessionKey::new_worker(1, "worker-1").worker_identity(),
        Some((1, "worker-1".to_string()))
    );
}

#[test]
fn the_round_counter_wraps_past_the_end_without_landing_on_no_round() {
    let mut state = EmporiumState::new(PrefixKey::default());
    state.next_catalog_round = u64::MAX;
    state.member_order.insert(1, vec!["worker-1".to_string()]);
    state.member_order.insert(2, vec!["worker-1".to_string()]);
    let ask = |state: &mut EmporiumState, brigade_id| {
        open_worker(
            state,
            brigade_id,
            "worker-1",
            std::path::Path::new("/work"),
            WorkerChoice {
                agent: AgentKind::Codex,
                model: "terra",
                effort: "",
            },
            None,
        )
    };
    assert_eq!(
        ask(&mut state, 1),
        vec![Cmd::ReadCodexCatalog { round: u64::MAX }]
    );
    state.codex_opens_awaiting_catalog.clear();
    state.pending_opens.clear();
    // The next one is 1, never 0, which names no round.
    assert_eq!(ask(&mut state, 2), vec![Cmd::ReadCodexCatalog { round: 1 }]);
}
