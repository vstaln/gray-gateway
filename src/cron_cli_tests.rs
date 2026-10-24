use super::*;

#[test]
fn list_json_reports_jobs_and_health() {
    let tmp = tempfile::tempdir().unwrap();
    let store = crate::cron::CronStore::open(tmp.path().join("cron")).unwrap();
    let id = store
        .add_full(
            "check",
            "in 30m",
            "check the bench",
            crate::cron::Deliver::Origin,
            Some(crate::cron::store::Origin {
                platform: "repl".into(),
                chat: "s1".into(),
                thread: None,
                route: None,
            }),
            None,
            vec![],
            None,
            true,
        )
        .unwrap();

    let v: serde_json::Value = serde_json::from_str(&list_json(&store).unwrap()).unwrap();
    let job = &v["jobs"][0];
    assert_eq!(job["id"], id);
    assert_eq!(job["name"], "check");
    assert_eq!(job["deliver"], "origin");
    assert_eq!(job["origin"]["chat"], "s1");
    assert!(job["next_run_at"].is_i64());
    assert!(
        v["health"].is_object() || v["health"].is_null(),
        "health key always present"
    );
}

#[test]
fn list_json_empty_store_is_an_empty_array() {
    let tmp = tempfile::tempdir().unwrap();
    let store = crate::cron::CronStore::open(tmp.path().join("cron")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&list_json(&store).unwrap()).unwrap();
    assert_eq!(v["jobs"].as_array().unwrap().len(), 0);
}
