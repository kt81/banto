//! Claude's asyncRewake waiter. Failures must not become hook exit code 2.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use banto_io::store::Store;

pub fn run() {
    let _ = std::thread::Builder::new().spawn(|| {
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
    });
    if guarded(wait) {
        std::process::exit(2);
    }
}

fn guarded(waiter: impl FnOnce() -> bool + std::panic::UnwindSafe) -> bool {
    std::panic::catch_unwind(waiter).unwrap_or(false)
}

fn wait() -> bool {
    let Some((path, brigade, member)) = parse(std::env::args_os().skip(2)) else {
        return false;
    };
    if !crate::startup::wake_db_path_safe(&path) {
        return false;
    }
    let Ok(mut store) = Store::open_current(&path) else {
        return false;
    };
    let Ok(Some(generation)) = store.arm_wake(brigade, &member) else {
        return false;
    };
    loop {
        match store.poll_wake(brigade, &member, generation) {
            Ok(Some(true)) => {
                return writeln!(
                    std::io::stderr(),
                    "{}",
                    banto_core::engine::RELAY_NUDGE_LINE
                )
                .is_ok();
            }
            Ok(Some(false)) => std::thread::sleep(Duration::from_millis(250)),
            _ => return false,
        }
    }
}

fn parse(args: impl Iterator<Item = OsString>) -> Option<(PathBuf, i64, String)> {
    let (mut path, mut brigade, mut member) = (None, None, None);
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--db-path") => path = Some(PathBuf::from(args.next()?)),
            Some("--brigade") => brigade = args.next()?.to_str()?.parse().ok(),
            Some("--member") => member = args.next()?.into_string().ok(),
            _ => {}
        }
    }
    Some((path?, brigade?, member?))
}

#[cfg(test)]
mod tests {
    #[test]
    fn waiter_panic_exits_zero_without_the_instruction() {
        const CHILD: &str = "BANTO_WAKE_PANIC_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            if super::guarded(|| panic!("synthetic waiter failure")) {
                std::process::exit(2);
            }
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "wake::tests::waiter_panic_exits_zero_without_the_instruction",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains(banto_core::engine::RELAY_NUDGE_LINE)
        );
    }
}
