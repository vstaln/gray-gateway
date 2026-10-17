use std::str::FromStr;

use chrono::{DateTime, Local, TimeZone, Utc};

pub const MIN_INTERVAL_SECS: u64 = 60;

pub const ONESHOT_GRACE_SECS: i64 = 120;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Schedule {
    Interval { secs: u64 },
    Cron { expr: String },
    Once { at: i64 },
}

fn parse_duration_secs(raw: &str) -> anyhow::Result<u64> {
    let s = raw.trim();
    let (num_str, mult) = if let Some(n) = s.strip_suffix('m') {
        (n, 60u64)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600u64)
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 86400u64)
    } else {
        anyhow::bail!("bad duration {:?}: want <n>m/h/d (e.g. 10m, 1h, 7d)", s);
    };
    let n: u64 = num_str
        .parse()
        .map_err(|_| anyhow::anyhow!("bad duration {:?}: number expected", s))?;
    let secs = n
        .checked_mul(mult)
        .ok_or_else(|| anyhow::anyhow!("bad duration {:?}: overflow", s))?;
    if secs < MIN_INTERVAL_SECS {
        anyhow::bail!("interval {}s below 60s ticker resolution", secs);
    }
    Ok(secs)
}

const DAYS: [&str; 7] = [
    "sunday",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
];

fn parse_clock(raw: &str) -> Option<(u32, u32)> {
    let t = raw.trim();
    match t {
        "noon" => return Some((12, 0)),
        "midnight" => return Some((0, 0)),
        _ => {}
    }
    let (body, half) = match (t.strip_suffix("am"), t.strip_suffix("pm")) {
        (Some(b), _) => (b, Some(0)),
        (_, Some(b)) => (b, Some(12)),
        _ => (t, None),
    };
    let (h, m) = body.split_once(':').unwrap_or((body, "0"));
    let (h, m): (u32, u32) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
    if m >= 60 {
        return None;
    }
    match half {
        Some(off) if (1..=12).contains(&h) => Some((h % 12 + off, m)),
        Some(_) => None,
        None => (h < 24).then_some((h, m)),
    }
}

fn parse_days(raw: &str) -> Option<String> {
    let days = raw
        .split(',')
        .map(|d| {
            let d = d.trim();
            let d = d.strip_suffix('s').filter(|d| d.len() >= 3).unwrap_or(d);
            (d.len() >= 3)
                .then(|| DAYS.iter().position(|full| full.starts_with(d)))
                .flatten()
        })
        .collect::<Option<Vec<_>>>()?;
    Some(
        days.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(","),
    )
}

fn natural_cron(input: &str) -> Option<String> {
    let lower = input.to_ascii_lowercase();
    let s = lower.strip_prefix("every ").unwrap_or(&lower).trim();
    if s == "hourly" || s == "hour" {
        return Some("0 * * * *".to_string());
    }
    let mut words = s.split_whitespace().filter(|w| !matches!(*w, "at" | "on"));
    let dow = match words.next()? {
        "day" | "daily" => "*".to_string(),
        "weekday" | "weekdays" => "1-5".to_string(),
        "weekend" | "weekends" => "0,6".to_string(),
        d => parse_days(d)?,
    };
    let rest: String = words.collect();
    let (h, m) = if rest.is_empty() {
        (0, 0)
    } else {
        parse_clock(&rest)?
    };
    Some(format!("{m} {h} * * {dow}"))
}

pub fn parse_schedule(input: &str) -> anyhow::Result<Schedule> {
    let s = input.trim();
    if s.is_empty() {
        anyhow::bail!("empty schedule");
    }
    if let Some(expr) = natural_cron(s) {
        return Ok(Schedule::Cron { expr });
    }
    if let Some(rest) = s.strip_prefix("in ").map(str::trim) {
        let secs = parse_duration_secs(rest)?;
        let secs_i64 =
            i64::try_from(secs).map_err(|_| anyhow::anyhow!("one-shot time overflow"))?;
        let at = Utc::now()
            .timestamp()
            .checked_add(secs_i64)
            .ok_or_else(|| anyhow::anyhow!("one-shot time overflow"))?;
        return Ok(Schedule::Once { at });
    }
    if let Some(rest) = s.strip_prefix("every ").map(str::trim) {
        return Ok(Schedule::Interval {
            secs: parse_duration_secs(rest)?,
        });
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(Schedule::Once {
            at: dt.with_timezone(&Utc).timestamp(),
        });
    }
    if let Ok(secs) = parse_duration_secs(s) {
        return Ok(Schedule::Interval { secs });
    }
    if s.split_whitespace().count() == 5 {
        cron::Schedule::from_str(&format!("0 {}", s))
            .map_err(|e| anyhow::anyhow!("bad cron {:?}: {}", s, e))?;
        return Ok(Schedule::Cron {
            expr: s.to_string(),
        });
    }
    anyhow::bail!(
        "bad schedule {:?}: want \"in 10m\", \"every 1h\", \"daily 9am\", \"weekdays 9:00\", \"every monday 6pm\", RFC3339, or 5-field cron",
        s
    )
}

pub fn next_run(after: i64, s: &Schedule) -> Option<i64> {
    match s {
        Schedule::Interval { secs } => {
            if *secs < MIN_INTERVAL_SECS {
                return None;
            }
            let secs_i64 = i64::try_from(*secs).ok()?;
            after.checked_add(secs_i64)
        }
        Schedule::Once { at } => Some(*at),
        Schedule::Cron { expr } => next_cron_in(after, expr, &Local),
    }
}

pub(crate) fn next_cron_in<Tz: TimeZone>(after: i64, expr: &str, tz: &Tz) -> Option<i64> {
    let sched = cron::Schedule::from_str(&format!("0 {}", expr)).ok()?;
    let after_dt = tz.timestamp_opt(after, 0).single()?;
    sched.after(&after_dt).next().map(|dt| dt.timestamp())
}
