use std::io::Write;
use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str], home: &Path) -> Output {
    // moved from gray core's tests/it with `gray read` → `gateway session read`
    Command::new(env!("CARGO_BIN_EXE_gray-gateway"))
        .args(["session", "read"])
        .args(args)
        .env("GRAY_USER_HOME", home)
        .env("GRAY_HOME", home)
        .output()
        .unwrap()
}

fn write_session(sessions: &Path, id: &str) {
    let body = format!(
        "{}\n{}\n{}\n{}\n",
        format!(
            "{{\"version\":1,\"id\":\"{id}\",\"timestamp\":1791932042949,\"cwd\":\"/tmp/work\",\"model\":\"swe-2\"}}"
        ),
        "{\"entry_id\":0,\"parent_id\":null,\"timestamp\":1791932043564,\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello from test\"}]}}",
        "{\"entry_id\":1,\"parent_id\":0,\"timestamp\":1791932044000,\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"bash\",\"args\":{\"command\":\"cargo test --release --test session\"}},{\"type\":\"tool_result\",\"id\":\"t1\",\"content\":\"ok 12 passed\",\"is_error\":false}]}}",
        "{\"entry_id\":2,\"parent_id\":1,\"timestamp\":1791932045000,\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"done fixing\"}]}}",
    );
    std::fs::write(sessions.join(format!("{id}.jsonl")), body).unwrap();
}

fn write_open(sessions: &Path, id: &str, pid: u32) {
    std::fs::write(
        sessions.join(format!("{id}.open")),
        format!("{{\"pid\":{pid},\"started_ms\":1791951059067}}\n"),
    )
    .unwrap();
}

#[test]
fn read_lists_only_locked_sessions_as_live() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();

    write_session(&sessions, "stale-session");
    write_open(&sessions, "stale-session", std::process::id());

    write_session(&sessions, "live-session");
    let live_open = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(sessions.join("live-session.open"))
        .unwrap();
    let holder = format!(
        "{{\"pid\":{},\"started_ms\":1791951059067}}\n",
        std::process::id()
    );
    write!(&live_open, "{holder}").unwrap();
    live_open.try_lock().unwrap();
    #[cfg(windows)]
    std::fs::write(sessions.join("live-session.open-holder"), holder).unwrap();

    let out = run(&["--live"], dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("live-session"), "{text}");
    assert!(!text.contains("stale-session"), "{text}");
    assert!(text.contains("swe-2"), "{text}");

    let out = run(&["--live", "--json"], dir.path());
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ids: Vec<&str> = json
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["id"].as_str())
        .collect();
    assert_eq!(ids, ["live-session"]);

    let out = run(&["stale-session"], dir.path());
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("not live"), "{text}");
    assert!(text.contains("hello from test"), "{text}");
    assert!(
        text.contains("→ bash: cargo test --release --test session"),
        "{text}"
    );
    assert!(text.contains("done fixing"), "{text}");

    let out = run(&["live-session"], dir.path());
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("live (pid"), "{text}");

    let out = run(&["no-such-session"], dir.path());
    assert_eq!(out.status.code(), Some(2));

    let out = run(&["../escape"], dir.path());
    assert_eq!(out.status.code(), Some(2));
}
