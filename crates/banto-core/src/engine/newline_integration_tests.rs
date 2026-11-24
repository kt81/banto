use super::*;

#[test]
#[allow(clippy::disallowed_methods)]
fn negotiated_codex_shift_enter_is_a_newline_record() {
    let now = Instant::now();
    let pane = SessionKey::from_id("synthetic");
    let row = SessionRow {
        id: "synthetic".into(),
        agent: AgentKind::Codex,
        title: None,
        cwd: None,
        dialog_open: false,
        activity: Activity::Alive,
        is_agent: false,
        preview: None,
        mtime: std::time::SystemTime::UNIX_EPOCH,
        size: 0,
        source_archived: false,
        archived: false,
    };
    let mut app = App::new(vec![row]);
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut screen = crate::screen::Screen::new(24, 80);
    screen.process(b"\x1b[?9001h");
    state.screens.insert(pane.clone(), screen);
    state.stage = Stage::Solo(pane.clone());
    state.focus = Focus::Pane;
    let cmds = update(
        &mut state,
        &mut app,
        &BrigadeConfig::default(),
        Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            Modifiers::SHIFT,
        ))),
        now,
    );
    assert!(cmds.iter().any(|cmd| matches!(cmd, Cmd::WritePty { key, bytes } if key == &pane && bytes == b"\x1b[13;28;13;1;16;1_\x1b[13;28;13;0;16;1_")), "{cmds:?}");
}

#[allow(clippy::disallowed_methods)]
fn fresh(agent: Option<AgentKind>, mode: bool) -> (EmporiumState, App, SessionKey, Instant) {
    let now = Instant::now();
    let key = SessionKey::new_worker(7, "worker-1");
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    state.pending_opens.insert(key.clone(), PendingOpen::Solo);
    update(
        &mut state,
        &mut app,
        &BrigadeConfig::default(),
        Event::Spawned {
            key: key.clone(),
            agent,
        },
        now,
    );
    if mode {
        update(
            &mut state,
            &mut app,
            &BrigadeConfig::default(),
            Event::PtyOutput {
                key: key.clone(),
                chunk: b"\x1b[?9001h".to_vec(),
            },
            now,
        );
    }
    (state, app, key, now)
}

fn send_key(state: &mut EmporiumState, app: &mut App, key: KeyEvent, now: Instant) -> Vec<u8> {
    let cmds = update(
        state,
        app,
        &BrigadeConfig::default(),
        Event::Input(InputEvent::Key(key)),
        now,
    );
    cmds.into_iter()
        .find_map(|cmd| match cmd {
            Cmd::WritePty { bytes, .. } => Some(bytes),
            _ => None,
        })
        .unwrap()
}

#[test]
fn fresh_codex_before_discovery_encodes_ctrl_j() {
    let (mut state, mut app, _pane, now) = fresh(Some(AgentKind::Codex), true);
    let ctrl_j = KeyEvent::new(KeyCode::Char('j'), Modifiers::CONTROL);
    assert_eq!(
        send_key(&mut state, &mut app, ctrl_j, now),
        b"\x1b[74;36;10;1;8;1_\x1b[74;36;10;0;8;1_"
    );

    assert_eq!(
        send_key(
            &mut state,
            &mut app,
            KeyEvent::new(KeyCode::Enter, Modifiers::NONE),
            now
        ),
        b"\r"
    );
}

#[test]
fn unknown_claude_disabled_and_conflicting_agent_keep_legacy_input() {
    for (agent, mode) in [
        (None, true),
        (Some(AgentKind::ClaudeCode), true),
        (Some(AgentKind::Codex), false),
    ] {
        let (mut state, mut app, _pane, now) = fresh(agent, mode);
        assert_eq!(
            send_key(
                &mut state,
                &mut app,
                KeyEvent::new(KeyCode::Char('j'), Modifiers::CONTROL),
                now
            ),
            b"\n"
        );
    }
    let (mut state, mut app, pane, now) = fresh(Some(AgentKind::Codex), true);
    state.member_brigades.insert(pane.clone(), 7);
    state.member_tokens.insert(pane.clone(), "worker-1".into());
    state.member_facts.insert(
        (7, "worker-1".into()),
        MemberFacts {
            agent: Some(AgentKind::ClaudeCode),
            inbound: None,
            sent: None,
            live: None,
        },
    );
    assert_eq!(
        send_key(
            &mut state,
            &mut app,
            KeyEvent::new(KeyCode::Enter, Modifiers::SHIFT),
            now
        ),
        b"\n"
    );
}

#[test]
fn literal_prefix_uses_the_same_encoding() {
    let (mut state, mut app, _pane, now) = fresh(Some(AgentKind::Codex), true);
    state.prefix = PrefixKey::parse("C-j");
    let key = KeyEvent::new(KeyCode::Char('j'), Modifiers::CONTROL);
    let first = update(
        &mut state,
        &mut app,
        &BrigadeConfig::default(),
        Event::Input(InputEvent::Key(key)),
        now,
    );
    assert!(!first.iter().any(|cmd| matches!(cmd, Cmd::WritePty { .. })));
    assert_eq!(
        send_key(&mut state, &mut app, key, now),
        b"\x1b[74;36;10;1;8;1_\x1b[74;36;10;0;8;1_"
    );
}

#[test]
fn launch_identity_follows_rekey_and_is_forgotten_on_new_spawn() {
    let (mut state, mut app, pane, now) = fresh(Some(AgentKind::Codex), true);
    let renamed = SessionKey::from_id("discovered");
    assert!(state.rekey_pane_state(&pane, &renamed));
    assert!(state.uses_codex_win32_input(&app, &renamed));
    state
        .pending_opens
        .insert(renamed.clone(), PendingOpen::Solo);
    update(
        &mut state,
        &mut app,
        &BrigadeConfig::default(),
        Event::Spawned {
            key: renamed.clone(),
            agent: Some(AgentKind::ClaudeCode),
        },
        now,
    );
    state
        .screens
        .get_mut(&renamed)
        .unwrap()
        .process(b"\x1b[?9001h");
    assert!(!state.uses_codex_win32_input(&app, &renamed));
}

#[test]
fn mode_disable_immediately_restores_legacy_keyboard_encoding() {
    let (mut state, mut app, pane, now) = fresh(Some(AgentKind::Codex), true);
    update(
        &mut state,
        &mut app,
        &BrigadeConfig::default(),
        Event::PtyOutput {
            key: pane,
            chunk: b"\x1b[?9001l".to_vec(),
        },
        now,
    );
    assert_eq!(
        send_key(
            &mut state,
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), Modifiers::CONTROL),
            now
        ),
        b"\n"
    );
}

#[test]
fn exit_and_lost_output_forget_launched_agent() {
    for lost in [false, true] {
        let (mut state, mut app, pane, now) = fresh(Some(AgentKind::Codex), true);
        let event = if lost {
            Event::PtyOutputLost { key: pane.clone() }
        } else {
            Event::PtyExited {
                key: pane.clone(),
                reason: None,
            }
        };
        update(&mut state, &mut app, &BrigadeConfig::default(), event, now);
        assert!(!state.launch_agents.contains_key(&pane));
    }
}
