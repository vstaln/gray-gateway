/// With only a session sidecar alive (no `gray-gateway run` daemon), a due
/// reminder job must still reach `cron/inbox/<sid>` — the file the open REPL's
/// 2-second poller wakes on. This is the contract that lets core drop its own
/// in-REPL ticker.
#[test]
fn the_session_sidecar_ticker_delivers_a_reminder_to_the_session_inbox() {
    let home = tempfile::tempdir().unwrap();
    // vendor::cron_auto_enabled() reads <GRAY_HOME>/config.json — point the
    // whole vendor layer at the temp home so the tick is enabled (absent flag
    // defaults to true) and nothing touches the user's real ~/.gray.
    let prev_home = std::env::var_os("GRAY_HOME");
    unsafe { std::env::set_var("GRAY_HOME", home.path()) };

    let cron = home.path().join("cron");
    let store = crate::cron::CronStore::open(&cron).unwrap();
    let jobs = serde_json::json!([{
        "id": "r1", "name": "r1", "prompt": "drink water",
        "schedule": {"Interval": {"secs": 3600}}, "enabled": true,
        "created_at": 1, "next_run_at": 1,
        "deliver": "origin", "reminder": true,
        "origin": {"platform": "repl", "chat": "sess-9"},
    }]);
    std::fs::write(
        cron.join("jobs.json"),
        serde_json::to_string_pretty(&jobs).unwrap(),
    )
    .unwrap();
    drop(store);

    let (stop, handle) = crate::gateway::run::spawn_ticker(
        home.path().to_path_buf(),
        std::time::Duration::from_millis(50),
        "sidecar",
    )
    .unwrap();

    let inbox = home.path().join("cron").join("inbox").join("sess-9");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let entry = loop {
        let hit = std::fs::read_dir(&inbox).ok().and_then(|d| {
            d.filter_map(|e| e.ok())
                .find(|e| !e.file_name().to_string_lossy().starts_with('.'))
        });
        if let Some(e) = hit {
            break e.path();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sidecar ticker never delivered the reminder"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };

    // same schema core's drain_session_inbox consumes
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(entry).unwrap()).unwrap();
    assert_eq!(v["name"], "r1");
    assert_eq!(v["body"], "drink water");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["reminder"], true);
    assert!(v["card"].is_string() && v["prompt"].is_string());

    let _ = stop.send(true);
    handle.join().unwrap();
    match prev_home {
        Some(v) => unsafe { std::env::set_var("GRAY_HOME", v) },
        None => unsafe { std::env::remove_var("GRAY_HOME") },
    }
}
