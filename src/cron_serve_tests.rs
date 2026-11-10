use super::*;

struct StubRunner {
    text: String,
    fail: bool,
    seen: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait(?Send)]
impl AsyncRunner for StubRunner {
    async fn run(&self, prompt: String, _cwd: PathBuf) -> anyhow::Result<String> {
        self.seen.lock().unwrap().push(prompt);
        if self.fail {
            anyhow::bail!("boom")
        } else {
            Ok(self.text.clone())
        }
    }
}

fn out(text: &str) -> FireOutput {
    FireOutput {
        transcript: text.to_string(),
        final_text: text.to_string(),
    }
}

fn due_store(home: &tempfile::TempDir, records: serde_json::Value) -> crate::cron::CronStore {
    let store = crate::cron::CronStore::open(home.path().join("cron")).unwrap();
    std::fs::write(
        home.path().join("cron").join("jobs.json"),
        serde_json::to_string_pretty(&records).unwrap(),
    )
    .unwrap();
    store
}

fn one_due(id: &str, deliver: serde_json::Value) -> serde_json::Value {
    serde_json::json!([{
        "id": id, "name": id, "prompt": "say hi",
        "schedule": {"Interval": {"secs": 3600}}, "enabled": true,
        "created_at": 1, "next_run_at": 1, "deliver": deliver,
    }])
}

fn make_session(home: &std::path::Path, chat: &str) {
    let dir = home.join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{chat}.jsonl")),
        "{\"type\":\"meta\",\"id\":\"t\"}\n{\"entry_id\":0,\"parent_id\":null,\"timestamp\":1700000000000,\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n",
    )
    .unwrap();
}

#[tokio::test]
async fn tick_fires_due_job_and_marks_ok() {
    let home = tempfile::tempdir().unwrap();
    let store = due_store(&home, one_due("j1", serde_json::json!("local")));
    let runner = StubRunner {
        text: "hello".to_string(),
        fail: false,
        seen: Default::default(),
    };
    let rep = tick_once_with(
        &store,
        &runner,
        &SaveLocalDeliver {
            home: home.path().to_path_buf(),
        },
        "test",
        true,
    )
    .await
    .unwrap();
    assert_eq!(rep.fired, 1);
    assert_eq!(rep.errors, 0);
    let job = store.get("j1").unwrap().unwrap();
    assert_eq!(job.last_status, Some(crate::cron::RunStatus::Ok));
    assert!(job.fire_claim.is_none());
    assert_eq!(runner.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn tick_agent_failure_records_error_and_continues() {
    let home = tempfile::tempdir().unwrap();
    let store = due_store(
        &home,
        serde_json::json!([
            {"id": "a", "name": "a", "prompt": "x",
             "schedule": {"Interval": {"secs": 3600}}, "enabled": true,
             "created_at": 1, "next_run_at": 1},
            {"id": "b", "name": "b", "prompt": "y",
             "schedule": {"Interval": {"secs": 3600}}, "enabled": true,
             "created_at": 1, "next_run_at": 1},
        ]),
    );
    let runner = StubRunner {
        text: String::new(),
        fail: true,
        seen: Default::default(),
    };
    let rep = tick_once_with(
        &store,
        &runner,
        &SaveLocalDeliver {
            home: home.path().to_path_buf(),
        },
        "test",
        true,
    )
    .await
    .unwrap();
    assert_eq!(rep.fired, 2);
    assert_eq!(rep.errors, 2);
    for id in ["a", "b"] {
        let job = store.get(id).unwrap().unwrap();
        assert_eq!(job.last_status, Some(crate::cron::RunStatus::Error));
        assert!(job.fire_claim.is_none());
    }
}

#[tokio::test]
async fn tick_fires_two_due_jobs_and_keeps_claim_order() {
    let home = tempfile::tempdir().unwrap();
    let store = due_store(
        &home,
        serde_json::json!([
            {"id": "a", "name": "a", "prompt": "x",
             "schedule": {"Interval": {"secs": 3600}}, "enabled": true,
             "created_at": 1, "next_run_at": 1},
            {"id": "b", "name": "b", "prompt": "y",
             "schedule": {"Interval": {"secs": 3600}}, "enabled": true,
             "created_at": 1, "next_run_at": 1},
        ]),
    );
    let runner = StubRunner {
        text: "hi".to_string(),
        fail: false,
        seen: Default::default(),
    };
    let rep = tick_once_with(
        &store,
        &runner,
        &SaveLocalDeliver {
            home: home.path().to_path_buf(),
        },
        "test",
        true,
    )
    .await
    .unwrap();
    assert_eq!(rep.fired, 2);
    assert_eq!(rep.errors, 0);
    assert_eq!(rep.delivered.len(), 2);
    assert_eq!(rep.delivered[0].id, "a");
    assert_eq!(rep.delivered[1].id, "b");
    for id in ["a", "b"] {
        let job = store.get(id).unwrap().unwrap();
        assert_eq!(job.last_status, Some(crate::cron::RunStatus::Ok));
        assert!(job.fire_claim.is_none());
    }
}

#[tokio::test]
async fn tick_silent_response_skips_write_but_ok() {
    let home = tempfile::tempdir().unwrap();
    let store = due_store(&home, one_due("s1", serde_json::json!("local")));
    let runner = StubRunner {
        text: "nothing to report\n[SILENT]".to_string(),
        fail: false,
        seen: Default::default(),
    };
    let rep = tick_once_with(
        &store,
        &runner,
        &SaveLocalDeliver {
            home: home.path().to_path_buf(),
        },
        "test",
        true,
    )
    .await
    .unwrap();
    assert_eq!((rep.fired, rep.errors), (1, 0));
    let job = store.get("s1").unwrap().unwrap();
    assert_eq!(job.last_status, Some(crate::cron::RunStatus::Ok));
    assert!(!home.path().join("cron").join("output").join("s1").exists());
}

#[test]
fn claim_outlives_max_fire_time() {
    let max_fire =
        (crate::cron_serve::FIRE_TIMEOUT_SECS + crate::cron_fire::SCRIPT_TIMEOUT_SECS) as i64;
    assert!(
        crate::cron::store::FIRE_CLAIM_TTL_SECS > max_fire,
        "a claim must outlive the longest supported fire (script + agent)"
    );
}

#[tokio::test]
async fn origin_delivery_appends_to_session_and_keeps_file_on_failure() {
    let home = tempfile::tempdir().unwrap();
    let chat = "origin-chat-1";
    make_session(home.path(), chat);
    let mk_job = |id: &str, chat: &str| crate::cron::CronJob {
        id: id.to_string(),
        name: "nightly".to_string(),
        prompt: "p".to_string(),
        schedule: crate::cron::Schedule::Interval { secs: 3600 },
        enabled: true,
        state: Default::default(),
        created_at: 1,
        next_run_at: Some(1),
        last_run_at: None,
        last_status: None,
        last_error: None,
        last_delivery_error: None,
        deliver: crate::cron::Deliver::Origin,
        origin: Some(crate::cron::store::Origin {
            platform: "local".to_string(),
            chat: chat.to_string(),
            thread: None,
            route: None,
        }),
        workdir: None,
        fire_claim: None,
        reminder: false,
        repeat: Default::default(),
        skills: vec![],
        script: None,
    };
    let deliver = SaveLocalDeliver {
        home: home.path().to_path_buf(),
    };

    let saved = deliver
        .deliver_full(
            &mk_job("j-origin", chat),
            1_700_000_001,
            &out("hello output"),
            0,
        )
        .await
        .unwrap();
    assert!(saved.to_chat);
    assert_eq!(saved.id, "j-origin");
    assert_eq!(saved.excerpt, "hello output");
    let file_text = std::fs::read_to_string(
        home.path()
            .join("cron")
            .join("output")
            .join("j-origin")
            .join("1700000001.md"),
    )
    .unwrap();
    assert!(file_text.contains("hello output"));
    let session_text =
        std::fs::read_to_string(home.path().join("sessions").join(format!("{chat}.jsonl")))
            .unwrap();
    assert!(
        session_text.contains("[Cron delivery: nightly]"),
        "origin session missing hermes mirror label"
    );
    assert!(session_text.contains("hello output"));
    assert!(
        !session_text.contains("Cronjob Response:"),
        "mirror must not carry the chat wrapper"
    );
    assert!(
        !session_text.contains("1700000001.md"),
        "mirror must not carry the file path"
    );
    assert!(
        session_text.contains("\"role\":\"user\""),
        "mirror must be a USER turn, never assistant"
    );

    let saved = deliver
        .deliver_full(
            &mk_job("j-bad", "no-such-session"),
            1_700_000_002,
            &out("kept output"),
            0,
        )
        .await
        .expect("a missing session must not swallow the delivery");
    assert!(saved.to_chat, "the route still gets the message");
    let kept = std::fs::read_to_string(
        home.path()
            .join("cron")
            .join("output")
            .join("j-bad")
            .join("1700000002.md"),
    )
    .unwrap();
    assert!(kept.contains("kept output"));
}

#[tokio::test]
async fn cron_runner_receives_job_workdir() {
    struct CwdRunner(PathBuf);
    #[async_trait::async_trait(?Send)]
    impl AsyncRunner for CwdRunner {
        async fn run(&self, _prompt: String, cwd: PathBuf) -> anyhow::Result<String> {
            assert_eq!(cwd, self.0);
            Ok("ok".into())
        }
    }
    let home = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let mut record = one_due("wd1", serde_json::json!("local"));
    record[0]["workdir"] = serde_json::json!(workdir.path());
    let store = due_store(&home, record);
    let report = tick_once_with(
        &store,
        &CwdRunner(workdir.path().to_path_buf()),
        &SaveLocalDeliver {
            home: home.path().to_path_buf(),
        },
        "test",
        true,
    )
    .await
    .unwrap();
    assert_eq!(report.errors, 0);
}

#[tokio::test]
async fn local_and_target_jobs_are_save_only() {
    for (id, d) in [
        ("l1", serde_json::json!("local")),
        ("t1", serde_json::json!({"target": "somewhere"})),
        ("o1", serde_json::json!("origin")),
    ] {
        let home = tempfile::tempdir().unwrap();
        let deliver = SaveLocalDeliver {
            home: home.path().to_path_buf(),
        };
        let mut v = one_due(id, d);
        v[0]["name"] = serde_json::json!("n");
        let store = due_store(&home, v);
        let job = store.list().unwrap().into_iter().next().unwrap();
        assert_eq!(job.id, id, "store lost {id}");
        let saved = deliver
            .deliver_full(&job, 1_700_000_001, &out("body"), 0)
            .await
            .unwrap();
        assert!(!saved.to_chat, "{id} must be save-only");
        assert!(
            home.path()
                .join("cron")
                .join("output")
                .join(id)
                .join("1700000001.md")
                .exists()
        );
    }
}

#[tokio::test]
async fn tick_fires_nothing_while_the_switch_is_off_but_keeps_ticking() {
    let home = tempfile::tempdir().unwrap();
    let store = due_store(&home, one_due("j1", serde_json::json!("local")));
    let runner = StubRunner {
        text: "hello".to_string(),
        fail: false,
        seen: Default::default(),
    };
    let rep = tick_once_with(
        &store,
        &runner,
        &SaveLocalDeliver {
            home: home.path().to_path_buf(),
        },
        "test",
        false,
    )
    .await
    .unwrap();
    assert_eq!(rep.fired, 0);
    assert_eq!(rep.errors, 0);
    assert!(rep.delivered.is_empty());

    let job = store.get("j1").unwrap().unwrap();
    assert!(job.fire_claim.is_none());
    assert_eq!(job.last_status, None);
    assert!(runner.seen.lock().unwrap().is_empty());

    let now = crate::cron::now_secs();
    let health = store.health(now).unwrap();
    assert!(health.ticker_live(now));

    let rep = tick_once_with(
        &store,
        &runner,
        &SaveLocalDeliver {
            home: home.path().to_path_buf(),
        },
        "test",
        true,
    )
    .await
    .unwrap();
    assert_eq!(rep.fired, 1);
}

fn delivered() -> DeliveredFire {
    DeliveredFire {
        id: "job1".into(),
        name: "daily check".into(),
        path: std::path::PathBuf::from("/tmp/out.md"),
        excerpt: "all clear".into(),
        to_chat: true,
        reminder: false,
        failed: false,
        elapsed_ms: 0,
    }
}

#[test]
fn delivery_json_carries_the_route_and_the_hermes_frame() {
    let origin = crate::cron::store::Origin {
        platform: "discord".into(),
        chat: "chat:one".into(),
        thread: None,
        route: Some("1234567890".into()),
    };
    let v: serde_json::Value = serde_json::from_str(&crate::cron_serve::delivery_json(
        &delivered(),
        Some(&origin),
    ))
    .unwrap();
    assert_eq!(v["type"], "cron_delivery");
    assert_eq!(v["job_id"], "job1");
    assert_eq!(v["platform"], "discord");
    assert_eq!(v["chat"], "chat:one");
    assert_eq!(v["route"], "1234567890");

    assert_eq!(v["kind"], "task");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["final_text"], "all clear");
    assert_eq!(v["path"], "/tmp/out.md");
    assert_eq!(v["text"].as_str().unwrap(), "daily check\n\nall clear");
}

#[test]
fn delivery_json_without_an_origin_still_renders() {
    let v: serde_json::Value =
        serde_json::from_str(&crate::cron_serve::delivery_json(&delivered(), None)).unwrap();
    assert_eq!(v["route"], serde_json::Value::Null);
    assert!(v["text"].as_str().unwrap().contains("all clear"));
}

#[tokio::test]
async fn a_platform_chat_id_gets_its_own_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    make_session(&home, "4f3c2b1a9d8e7f6a5b4c3d2e1f0a9b8c");
    let job = crate::cron::CronJob {
        id: "job1".into(),
        name: "nightly".into(),
        prompt: "p".into(),
        schedule: crate::cron::Schedule::Interval { secs: 3600 },
        enabled: true,
        state: Default::default(),
        created_at: 1,
        next_run_at: Some(1),
        last_run_at: None,
        last_status: None,
        last_error: None,
        last_delivery_error: None,
        deliver: crate::cron::Deliver::Origin,
        origin: Some(crate::cron::store::Origin {
            platform: "discord".into(),
            chat: "4f3c2b1a9d8e7f6a5b4c3d2e1f0a9b8c".into(),
            thread: None,
            route: Some("42".into()),
        }),
        workdir: None,
        fire_claim: None,
        reminder: false,
        repeat: Default::default(),
        skills: vec![],
        script: None,
    };
    let deliver = SaveLocalDeliver { home: home.clone() };
    let saved = deliver
        .deliver_full(&job, 1, &out("hello"), 0)
        .await
        .unwrap();
    assert!(saved.to_chat, "a routed delivery is still a chat delivery");
    let transcript = std::fs::read_to_string(
        home.join("sessions")
            .join("4f3c2b1a9d8e7f6a5b4c3d2e1f0a9b8c.jsonl"),
    )
    .unwrap();
    assert!(
        transcript.contains("[Cron delivery: nightly]"),
        "the mirrored turn is what makes a reply continue in context"
    );
}

#[tokio::test]
async fn a_missing_origin_session_still_delivers_to_the_route() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    let job = crate::cron::CronJob {
        id: "job1".into(),
        name: "nightly".into(),
        prompt: "p".into(),
        schedule: crate::cron::Schedule::Interval { secs: 3600 },
        enabled: true,
        state: Default::default(),
        created_at: 1,
        next_run_at: Some(1),
        last_run_at: None,
        last_status: None,
        last_error: None,
        last_delivery_error: None,
        deliver: crate::cron::Deliver::Origin,
        origin: Some(crate::cron::store::Origin {
            platform: "discord".into(),
            chat: "4f3c2b1a9d8e7f6a".into(),
            thread: None,
            route: Some("1234567890".into()),
        }),
        workdir: None,
        fire_claim: None,
        reminder: false,
        repeat: Default::default(),
        skills: vec![],
        script: None,
    };
    let deliver = SaveLocalDeliver { home: home.clone() };
    let saved = deliver
        .deliver_full(&job, 1, &out("the deploy is green"), 0)
        .await
        .expect("a missing session is not a delivery failure");
    assert!(saved.to_chat);
    assert!(saved.excerpt.contains("the deploy is green"));

    let out = home.join("cron").join("output").join("job1").join("1.md");
    assert!(
        std::fs::read_to_string(&out)
            .unwrap()
            .contains("the deploy is green")
    );
}

#[tokio::test]
async fn a_reminder_fires_without_a_model_turn_and_delivers_verbatim() {
    let home = tempfile::tempdir().unwrap();
    let mut record = one_due("r1", serde_json::json!("origin"));
    record[0]["reminder"] = serde_json::json!(true);
    record[0]["prompt"] = serde_json::json!("clean my roo");
    record[0]["origin"] = serde_json::json!({"platform": "discord", "chat": "4f3c2b1a9d8e7f6a5b4c3d2e1f0a9b8c", "route": "42"});
    let store = due_store(&home, record);
    let runner = StubRunner {
        text: "MUST NOT RUN".to_string(),
        fail: false,
        seen: Default::default(),
    };
    let rep = tick_once_with(
        &store,
        &runner,
        &SaveLocalDeliver {
            home: home.path().to_path_buf(),
        },
        "test",
        true,
    )
    .await
    .unwrap();
    assert_eq!(rep.fired, 1);
    assert_eq!(rep.errors, 0);
    assert!(
        runner.seen.lock().unwrap().is_empty(),
        "a reminder must not reach the agent"
    );
    let d = &rep.delivered[0];
    assert!(d.reminder && d.to_chat && !d.failed);
    assert_eq!(d.excerpt, "clean my roo");

    assert_eq!(format_fire_chat(d), "clean my roo");
}

#[tokio::test]
async fn a_failed_origin_fire_delivers_a_short_failure_line() {
    let home = tempfile::tempdir().unwrap();
    let mut record = one_due("f1", serde_json::json!("origin"));
    record[0]["origin"] = serde_json::json!({"platform": "discord", "chat": "4f3c2b1a9d8e7f6a5b4c3d2e1f0a9b8c", "route": "42"});
    let store = due_store(&home, record);
    let runner = StubRunner {
        text: String::new(),
        fail: true,
        seen: Default::default(),
    };
    let rep = tick_once_with(
        &store,
        &runner,
        &SaveLocalDeliver {
            home: home.path().to_path_buf(),
        },
        "test",
        true,
    )
    .await
    .unwrap();
    assert_eq!(rep.errors, 1);
    let d = &rep.delivered[0];
    assert!(d.failed && d.to_chat && !d.reminder);
    assert_eq!(d.excerpt, "agent run failed: boom");
    let job = store.get("f1").unwrap().unwrap();
    assert_eq!(job.last_status, Some(crate::cron::RunStatus::Error));
}

#[tokio::test]
async fn chat_gets_only_the_final_text_while_the_transcript_stays_on_disk() {
    struct SplitRunner;
    #[async_trait::async_trait(?Send)]
    impl AsyncRunner for SplitRunner {
        async fn run(&self, _p: String, _c: PathBuf) -> anyhow::Result<String> {
            unreachable!("fire_one calls run_full")
        }
        async fn run_full(&self, _p: String, _c: PathBuf) -> anyhow::Result<FireOutput> {
            Ok(FireOutput {
                transcript: "[tool:bash]\n[result:exit 0]\n<untrusted-output>\nls\n</untrusted-output>\nclean my roo".into(),
                final_text: "clean my roo".into(),
            })
        }
    }
    let home = tempfile::tempdir().unwrap();
    let mut record = one_due("s2", serde_json::json!("origin"));
    record[0]["origin"] = serde_json::json!({"platform": "discord", "chat": "4f3c2b1a9d8e7f6a5b4c3d2e1f0a9b8c", "route": "42"});
    let store = due_store(&home, record);
    let rep = tick_once_with(
        &store,
        &SplitRunner,
        &SaveLocalDeliver {
            home: home.path().to_path_buf(),
        },
        "test",
        true,
    )
    .await
    .unwrap();
    let d = &rep.delivered[0];
    assert_eq!(
        d.excerpt, "clean my roo",
        "the tool transcript leaked into chat"
    );
    let saved = std::fs::read_to_string(&d.path).unwrap();
    assert!(
        saved.contains("[tool:bash]"),
        "full transcript stays on disk"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&d.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[tokio::test]
async fn a_repl_origin_fire_writes_the_session_inbox_schema_core_drains() {
    let home = tempfile::tempdir().unwrap();
    let mut record = one_due("s3", serde_json::json!("origin"));
    record[0]["origin"] = serde_json::json!({"platform": SESSION_PLATFORM, "chat": "sess-1"});
    let store = due_store(&home, record);
    let runner = StubRunner {
        text: "toilets: a history".into(),
        fail: false,
        seen: Default::default(),
    };
    let deliver = SaveLocalDeliver {
        home: home.path().to_path_buf(),
    };
    let rep = tick_once_with(&store, &runner, &deliver, "test", true)
        .await
        .unwrap();
    assert_eq!(rep.fired, 1);
    assert!(rep.delivered.iter().all(|d| !d.to_chat), "no chat route");
    assert!(!home.path().join("sessions").exists(), "no mirror");

    // Exactly the entry core's drain_session_inbox reads:
    //   card: String (required)            -> SessionDelivery.card
    //   prompt: String (required)          -> SessionDelivery.prompt
    //   name: String (optional)            -> CronCard.name
    //   body: String (optional)            -> CronCard.body
    //   status: String (optional)          -> CronCard.failed = (status == "failed")
    //   reminder: bool (optional)          -> CronCard.reminder
    //   elapsed_ms: u64 (optional)         -> CronCard.elapsed_ms
    let dir = home.path().join("cron").join("inbox").join("sess-1");
    let entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .collect();
    assert_eq!(entries.len(), 1, "exactly one inbox entry");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(entries[0].path()).unwrap()).unwrap();
    for field in ["card", "prompt", "name", "body", "status"] {
        assert!(
            v.get(field).is_some_and(|x| x.is_string()),
            "field {field:?} must be a string: {v}"
        );
    }
    assert!(
        v.get("reminder").is_some_and(|x| x.is_boolean()),
        "field \"reminder\" must be a bool: {v}"
    );
    assert!(
        v.get("elapsed_ms").is_some_and(|x| x.is_u64()),
        "field \"elapsed_ms\" must be a u64: {v}"
    );
    assert_eq!(v["name"], "s3");
    assert_eq!(v["body"], "toilets: a history");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["reminder"], false);
    assert_eq!(v["prompt"], "[Cron delivery: s3]\ntoilets: a history");
    assert!(
        v["card"].as_str().unwrap().contains("toilets: a history"),
        "card: {}",
        v["card"]
    );
    assert!(v.get("kind").is_none(), "not a send entry: {v}");
}

#[tokio::test]
async fn hosted_delivery_survives_a_non_host_driver() {
    let home = tempfile::tempdir().unwrap();
    let origin = serde_json::json!({"platform": "discord", "chat": "c1", "route": "chan-9"});
    let mut hosted = one_due("d1", serde_json::json!("origin"));
    hosted[0]["origin"] = origin;
    let mut local = one_due("l1", serde_json::json!("origin"));
    local[0]["origin"] = serde_json::json!({"platform": "local", "chat": "c2"});
    hosted.as_array_mut().unwrap().push(local[0].clone());
    let store = due_store(&home, hosted);
    let runner = StubRunner {
        text: "time to stretch".to_string(),
        fail: false,
        seen: Default::default(),
    };
    let deliver = SaveLocalDeliver {
        home: home.path().to_path_buf(),
    };
    let rep = tick_once_with(&store, &runner, &deliver, "gateway", true)
        .await
        .unwrap();
    assert_eq!(rep.fired, 2);
    let lines = drain_outbox(home.path());
    assert_eq!(lines.len(), 1, "only the hosted job spools: {lines:?}");
    let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(v["type"], "cron_delivery");
    assert_eq!(v["job_id"], "d1");
    assert_eq!(v["route"], "chan-9");
    assert_eq!(v["final_text"], "time to stretch");
    assert!(drain_outbox(home.path()).is_empty(), "drained exactly once");
}

#[tokio::test]
async fn repl_bound_job_in_a_chat_home_goes_to_the_chat() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("route.json"),
        r#"{"platform":"discord","chat":"c1","route":"chan-7"}"#,
    )
    .unwrap();
    let mut jobs = one_due("m1", serde_json::json!("origin"));
    jobs[0]["origin"] = serde_json::json!({"platform": "repl", "chat": "one-turn-session"});
    let store = due_store(&home, jobs);
    let runner = StubRunner {
        text: "good morning".to_string(),
        fail: false,
        seen: Default::default(),
    };
    let deliver = SaveLocalDeliver {
        home: home.path().to_path_buf(),
    };
    let rep = tick_once_with(&store, &runner, &deliver, "gateway", true)
        .await
        .unwrap();
    assert_eq!(rep.fired, 1);
    let lines = drain_outbox(home.path());
    assert_eq!(lines.len(), 1, "rerouted to the chat: {lines:?}");
    let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(v["platform"], "discord");
    assert_eq!(v["route"], "chan-7");
    assert_eq!(v["final_text"], "good morning");
}

#[test]
fn schedule_tool_binds_jobs_to_the_chat_home() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("route.json"),
        r#"{"platform":"discord","chat":"c1","route":"chan-7"}"#,
    )
    .unwrap();
    let tool = crate::cron_tool::ScheduleTool {
        home: home.path().to_path_buf(),
    };
    tool.apply(
        Some("sid-1"),
        &serde_json::json!({"name": "wake", "when": "daily 8am", "prompt": "hi"}),
    )
    .unwrap();
    let store = crate::cron::CronStore::open(home.path().join("cron")).unwrap();
    let job = store.get("wake").unwrap().unwrap();
    let origin = job.origin.unwrap();
    assert_eq!(origin.platform, "discord");
    assert_eq!(origin.route.as_deref(), Some("chan-7"));
}
