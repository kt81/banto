//! When the emporium asks for the data directory to be swept
//! ([`Cmd::SweepDataDir`]): once shortly after the first tick and then every
//! interval, and at no other time — in particular not when a member or brigade
//! leaves the store, because what that ends is too fresh to be deleted anyway.
//! What the sweep deletes is `banto_io::data_dir`'s, tested there against real
//! files; this is only the when.

use std::time::{Duration, Instant};

use crate::app::App;
use crate::config::BrigadeConfig;

use super::*;

#[allow(clippy::disallowed_methods)]
fn now() -> Instant {
    Instant::now()
}

fn send(state: &mut EmporiumState, app: &mut App, event: Event, at: Instant) -> Vec<Cmd> {
    update(state, app, &BrigadeConfig::default(), event, at)
}

fn tick(state: &mut EmporiumState, app: &mut App, at: Instant) -> Vec<Cmd> {
    send(
        state,
        app,
        Event::Tick {
            relay: Vec::new(),
            brigades: Vec::new(),
        },
        at,
    )
}

fn sweeps(cmds: &[Cmd]) -> usize {
    cmds.iter().filter(|cmd| **cmd == Cmd::SweepDataDir).count()
}

fn fresh() -> (EmporiumState, App) {
    (
        EmporiumState::new(PrefixKey::default()),
        App::new(Vec::new()),
    )
}

#[test]
fn the_first_tick_starts_the_clock_and_sweeps_nothing() {
    let (mut state, mut app) = fresh();
    let t = now();
    assert_eq!(sweeps(&tick(&mut state, &mut app, t)), 0);
    assert_eq!(
        sweeps(&tick(&mut state, &mut app, t + Duration::from_secs(1))),
        0
    );
}

#[test]
fn the_first_sweep_comes_after_the_delay_and_not_before() {
    let (mut state, mut app) = fresh();
    let t = now();
    tick(&mut state, &mut app, t);
    let almost = t + DATA_DIR_FIRST_SWEEP_DELAY - Duration::from_millis(1);
    assert_eq!(sweeps(&tick(&mut state, &mut app, almost)), 0);
    assert_eq!(
        sweeps(&tick(&mut state, &mut app, t + DATA_DIR_FIRST_SWEEP_DELAY)),
        1
    );
}

#[test]
fn sweeps_then_repeat_each_interval_and_once_per_interval() {
    let (mut state, mut app) = fresh();
    let t = now();
    tick(&mut state, &mut app, t);
    let first = t + DATA_DIR_FIRST_SWEEP_DELAY;
    assert_eq!(sweeps(&tick(&mut state, &mut app, first)), 1);
    // Another tick at the same instant, or any time before the interval is up.
    assert_eq!(sweeps(&tick(&mut state, &mut app, first)), 0);
    let almost = first + DATA_DIR_SWEEP_INTERVAL - Duration::from_millis(1);
    assert_eq!(sweeps(&tick(&mut state, &mut app, almost)), 0);
    let second = first + DATA_DIR_SWEEP_INTERVAL;
    assert_eq!(sweeps(&tick(&mut state, &mut app, second)), 1);
    assert_eq!(
        sweeps(&tick(
            &mut state,
            &mut app,
            second + DATA_DIR_SWEEP_INTERVAL
        )),
        1
    );
}

#[test]
fn a_tick_after_a_long_gap_sweeps_once_not_once_per_missed_interval() {
    let (mut state, mut app) = fresh();
    let t = now();
    tick(&mut state, &mut app, t);
    let much_later = t + 10 * DATA_DIR_SWEEP_INTERVAL;
    assert_eq!(sweeps(&tick(&mut state, &mut app, much_later)), 1);
    assert_eq!(
        sweeps(&tick(
            &mut state,
            &mut app,
            much_later + Duration::from_secs(1)
        )),
        0
    );
}

#[test]
fn ending_a_member_or_a_brigade_does_not_ask_for_a_sweep() {
    let (mut state, mut app) = fresh();
    let t = now();
    // The clock is running and a sweep is not yet due.
    tick(&mut state, &mut app, t);
    for event in [
        Event::ExitedWorkerDismissed {
            brigade_id: 1,
            token: "worker-1".to_string(),
            repeated: false,
            result: Ok((true, HashSet::new(), HashSet::new())),
        },
        Event::WorkerDismissed {
            brigade_id: 1,
            result: Ok((HashSet::new(), HashSet::new())),
        },
        Event::Disbanded {
            brigade_id: 1,
            result: Ok((HashSet::new(), HashSet::new())),
        },
    ] {
        state.pending_dismiss = Some(SessionKey::from_id("w1"));
        let cmds = send(&mut state, &mut app, event, t);
        assert_eq!(sweeps(&cmds), 0, "{cmds:?}");
    }
    // And none of it moved the clock.
    assert_eq!(
        sweeps(&tick(&mut state, &mut app, t + DATA_DIR_FIRST_SWEEP_DELAY)),
        1
    );
}
