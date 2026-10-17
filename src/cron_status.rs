use crate::cron::{CronHealth, TICKER_STALE_SECS, TickStamp};

const DRIVERS: &str = "install the gateway (`gray gateway install`), or run `gray cron serve`, `gray cron tick`, or a REPL";

pub fn ago_short(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

pub fn ticker_line(health: &CronHealth, now: i64) -> String {
    let mut out = match &health.last_tick {
        Some(t) if health.ticker_live(now) => format!(
            "ticker: live — last tick {} ago ({})",
            ago_short(now.saturating_sub(t.at)),
            t.kind
        ),
        Some(t) => format!(
            "⚠ ticker: stale — last tick {} ago ({}) — jobs will NOT fire; {DRIVERS}",
            ago_short(now.saturating_sub(t.at)),
            t.kind
        ),
        None => format!("⚠ ticker: no tick has ever run — jobs will NOT fire; {DRIVERS}"),
    };
    if let Some(worst) = health
        .overdue
        .iter()
        .map(|j| now.saturating_sub(j.next_run_at))
        .max()
    {
        out.push_str(&format!(
            "\n⚠ {} job(s) overdue (worst {} behind) — not firing",
            health.overdue.len(),
            ago_short(worst)
        ));
    }
    out
}

pub fn format_cron_dashboard(
    jobs: &[crate::cron::CronJob],
    health: Option<&crate::cron::CronHealth>,
    now: i64,
) -> String {
    if jobs.is_empty() {
        return "no cron jobs — `gray cron add \"every 1h\" \"prompt\"` to create one".to_string();
    }
    let mut out = String::new();
    for j in jobs {
        let short_id: String = j.id.chars().take(8).collect();
        out.push_str(&format!(
            "• {} ({}) — {:?} — next {} — last {}\n",
            j.name,
            short_id,
            j.schedule,
            j.next_run_at
                .map(|t| t.to_string())
                .as_deref()
                .unwrap_or("-"),
            j.last_status
                .map(|s| format!("{s:?}"))
                .as_deref()
                .unwrap_or("-"),
        ));
    }
    match health {
        Some(h) => out.push_str(&ticker_line(h, now)),
        None => out.push_str("due jobs fire automatically in this session"),
    }
    out
}

pub fn add_warning(last_tick: Option<&TickStamp>, now: i64, chat_bound: bool) -> Option<String> {
    if chat_bound {
        return Some(
            "delivers back to this conversation on the host's next tick (~60s); nothing further to do"
                .to_string(),
        );
    }
    let detail = match last_tick {
        None => "no cron ticker has ever run".to_string(),
        Some(t) => {
            let age = now.saturating_sub(t.at);
            if age <= TICKER_STALE_SECS {
                return None;
            }
            format!("last cron tick was {} ago", ago_short(age))
        }
    };
    Some(format!(
        "⚠ {detail} — this job will not fire until one does; {DRIVERS}"
    ))
}
