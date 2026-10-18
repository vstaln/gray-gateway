use super::*;

#[test]
fn parse_all_kinds() {
    assert!(matches!(
        parse_schedule("in 10m").unwrap(),
        Schedule::Once { .. }
    ));
    assert!(matches!(
        parse_schedule("every 1h").unwrap(),
        Schedule::Interval { secs: 3600 }
    ));
    assert!(matches!(
        parse_schedule("30m").unwrap(),
        Schedule::Interval { secs: 1800 }
    ));
    assert!(matches!(
        parse_schedule("0 9 * * *").unwrap(),
        Schedule::Cron { .. }
    ));
    assert!(
        parse_schedule("every 30s").is_err(),
        "below ticker resolution"
    );
}

#[test]
fn interval_below_resolution_has_no_next_run() {
    assert_eq!(next_run(0, &Schedule::Interval { secs: 0 }), None);
    assert_eq!(next_run(1, &Schedule::Interval { secs: 59 }), None);
    assert_eq!(next_run(1, &Schedule::Interval { secs: 60 }), Some(61));
}

#[test]
fn cron_fields_are_wall_clock_in_the_given_zone() {
    use chrono::FixedOffset;

    let midnight_utc = 1_791_676_800;
    let wib = FixedOffset::east_opt(7 * 3600).unwrap();
    let pst = FixedOffset::west_opt(8 * 3600).unwrap();

    assert_eq!(
        next_cron_in(midnight_utc, "0 9 * * *", &wib),
        Some(midnight_utc + 2 * 3600)
    );
    assert_eq!(
        next_cron_in(midnight_utc, "0 9 * * *", &pst),
        Some(midnight_utc + 17 * 3600)
    );
    assert_eq!(
        next_cron_in(midnight_utc, "0 9 * * *", &chrono::Utc),
        Some(midnight_utc + 9 * 3600)
    );
}

#[test]
fn natural_schedules_compile_to_cron() {
    let cron = |s: &str| match parse_schedule(s).unwrap() {
        Schedule::Cron { expr } => expr,
        other => panic!("{s:?} -> {other:?}"),
    };
    assert_eq!(cron("daily 9am"), "0 9 * * *");
    assert_eq!(cron("every day at 9:30pm"), "30 21 * * *");
    assert_eq!(cron("Daily"), "0 0 * * *");
    assert_eq!(cron("hourly"), "0 * * * *");
    assert_eq!(cron("weekdays 9:00"), "0 9 * * 1-5");
    assert_eq!(cron("weekends noon"), "0 12 * * 0,6");
    assert_eq!(cron("every monday 18:00"), "0 18 * * 1");
    assert_eq!(cron("mondays 12am"), "0 0 * * 1");
    assert_eq!(cron("mon,wed 6:30pm"), "30 18 * * 1,3");
    assert_eq!(cron("0 9 * * *"), "0 9 * * *");

    assert_eq!(
        parse_schedule("every 1h").unwrap(),
        Schedule::Interval { secs: 3600 }
    );
    for bad in [
        "daily 25:00",
        "daily 13pm",
        "every mo 9am",
        "weekdays 9:75",
        "blursday 9am",
    ] {
        assert!(parse_schedule(bad).is_err(), "{bad:?} should fail");
    }
}
