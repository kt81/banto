use std::process::Command;

fn isolated_command() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_banto"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("BANTO_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

#[test]
fn invalid_wakes_exit_zero_without_creating_a_store() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.db");
    let wrong = dir.path().join("wrong.db");
    let db = rusqlite::Connection::open(&wrong).unwrap();
    db.pragma_update(None, "user_version", 999).unwrap();
    drop(db);
    let run = |args: &[&std::ffi::OsStr]| {
        let mut cmd = isolated_command();
        cmd.args(args).status().unwrap()
    };
    assert!(run(&["_wake".as_ref(), "--garbage".as_ref()]).success());
    for path in [&missing, &wrong] {
        assert!(
            run(&[
                "_wake".as_ref(),
                "--db-path".as_ref(),
                path.as_os_str(),
                "--brigade".as_ref(),
                "1".as_ref(),
                "--member".as_ref(),
                "worker-1".as_ref()
            ])
            .success()
        );
    }
    assert!(!missing.exists());
}

#[test]
fn issued_unread_mail_wakes_with_the_fixed_instruction() {
    use banto_core::model::BrigadeRole;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("banto.db");
    let mut store = banto_io::store::Store::open(&path).unwrap();
    let brigade = store.create_brigade("synthetic").unwrap();
    store
        .add_brigade_member(brigade, "worker-1", BrigadeRole::Worker, None)
        .unwrap();
    store
        .enqueue_brigade_message(brigade, "director", BrigadeRole::Worker, None, "synthetic")
        .unwrap();
    store.issue_wake(brigade, "worker-1").unwrap();
    let output = isolated_command()
        .arg("_wake")
        .arg("--db-path")
        .arg(&path)
        .args(["--brigade", &brigade.to_string(), "--member", "worker-1"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().trim(),
        banto_core::engine::RELAY_NUDGE_LINE
    );
    assert!(!store.wake_pending(brigade, "worker-1").unwrap());
}

#[test]
fn silent_receipt_keeps_waiting_for_a_later_issue_with_mail() {
    use banto_core::model::BrigadeRole;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("banto.db");
    let mut store = banto_io::store::Store::open(&path).unwrap();
    let brigade = store.create_brigade("synthetic").unwrap();
    store
        .add_brigade_member(brigade, "worker-1", BrigadeRole::Worker, None)
        .unwrap();
    store.issue_wake(brigade, "worker-1").unwrap();
    let mut child = isolated_command()
        .arg("_wake")
        .arg("--db-path")
        .arg(&path)
        .args(["--brigade", &brigade.to_string(), "--member", "worker-1"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while store.wake_pending(brigade, "worker-1").unwrap() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "waiter exited before receipt"
        );
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("waiter did not consume the silent issue");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(350));
    assert!(
        child.try_wait().unwrap().is_none(),
        "waiter exited after silent receipt"
    );
    store
        .enqueue_brigade_message(brigade, "director", BrigadeRole::Worker, None, "synthetic")
        .unwrap();
    store.issue_wake(brigade, "worker-1").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("waiter did not wake for the later issue");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(2));
}

#[test]
fn large_hook_input_does_not_delay_an_issued_wake() {
    use banto_core::model::BrigadeRole;
    use std::io::Write;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("banto.db");
    let mut store = banto_io::store::Store::open(&path).unwrap();
    let brigade = store.create_brigade("synthetic").unwrap();
    store
        .add_brigade_member(brigade, "worker-1", BrigadeRole::Worker, None)
        .unwrap();
    store
        .enqueue_brigade_message(brigade, "director", BrigadeRole::Worker, None, "synthetic")
        .unwrap();
    store.issue_wake(brigade, "worker-1").unwrap();
    let mut child = isolated_command()
        .arg("_wake")
        .arg("--db-path")
        .arg(&path)
        .args(["--brigade", &brigade.to_string(), "--member", "worker-1"])
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&vec![b'x'; 8 * 1024 * 1024]);
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("wake did not exit promptly with large hook input");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    writer.join().unwrap();
    assert_eq!(status.code(), Some(2));
}

#[test]
fn database_inside_agent_home_is_not_opened() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("claude home");
    std::fs::create_dir_all(&home).unwrap();
    let db = home.join("banto.db");
    {
        let mut store = banto_io::store::Store::open(&db).unwrap();
        let brigade = store.create_brigade("synthetic").unwrap();
        store
            .add_brigade_member(
                brigade,
                "worker-1",
                banto_core::model::BrigadeRole::Worker,
                None,
            )
            .unwrap();
        store
            .enqueue_brigade_message(
                brigade,
                "director",
                banto_core::model::BrigadeRole::Worker,
                None,
                "synthetic",
            )
            .unwrap();
        store.issue_wake(brigade, "worker-1").unwrap();
    }
    let status = isolated_command()
        .env("CLAUDE_CONFIG_DIR", &home)
        .arg("_wake")
        .arg("--db-path")
        .arg(&db)
        .args(["--brigade", "1", "--member", "worker-1"])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(
        banto_io::store::Store::open_current(&db)
            .unwrap()
            .wake_pending(1, "worker-1")
            .unwrap()
    );
}
