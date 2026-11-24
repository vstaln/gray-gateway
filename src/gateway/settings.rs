use std::path::Path;

use crate::cron::store::Origin as Route;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub owners: Vec<Route>,

    pub max_turns: usize,

    pub turn_timeout_secs: u64,

    pub workdir: Option<std::path::PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            owners: Vec::new(),
            max_turns: 2,
            turn_timeout_secs: 1800,
            workdir: None,
        }
    }
}

impl Settings {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("config.json");
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                log::warn!(
                    "gateway: {} unreadable ({e}); using defaults",
                    path.display()
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ResetMode {
    None,
    Idle,
    Daily,
    Both,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ResetPolicy {
    mode: ResetMode,
    idle_minutes: u64,
    at_hour: u32,
}

impl ResetPolicy {
    fn path(home: &Path) -> std::path::PathBuf {
        home.join("gateway/session-reset.json")
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=10_080).contains(&self.idle_minutes),
            "Idle minutes must be between 1 and 10080."
        );
        anyhow::ensure!(self.at_hour <= 23, "Daily hour must be between 0 and 23.");
        Ok(())
    }

    fn load(home: &Path) -> anyhow::Result<Option<Self>> {
        let bytes = match std::fs::read(Self::path(home)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let policy: Self = serde_json::from_slice(&bytes)?;
        policy.validate()?;
        Ok(Some(policy))
    }

    pub fn display(home: &Path) -> anyhow::Result<String> {
        Ok(match Self::load(home)? {
            Some(policy) => policy.summary(),
            None => "No gateway override. The transport's existing policy is unchanged.".into(),
        })
    }

    fn summary(&self) -> String {
        match self.mode {
            ResetMode::None => "Manual only.".into(),
            ResetMode::Idle => format!("After {} minutes idle.", self.idle_minutes),
            ResetMode::Daily => format!("Daily at {:02}:00 (gateway local time).", self.at_hour),
            ResetMode::Both => format!(
                "Daily at {:02}:00 (gateway local time) or after {} minutes idle.",
                self.at_hour, self.idle_minutes
            ),
        }
    }

    fn reason(&self, last_activity: Option<f64>, now: f64) -> Option<&'static str> {
        use chrono::{Local, TimeZone, Timelike};
        if matches!(self.mode, ResetMode::None) {
            return None;
        }
        let Some(last) = last_activity else {
            return Some("legacy");
        };
        if matches!(self.mode, ResetMode::Idle | ResetMode::Both)
            && now - last >= (self.idle_minutes * 60) as f64
        {
            return Some("idle");
        }
        if !matches!(self.mode, ResetMode::Daily | ResetMode::Both) {
            return None;
        }
        let now = Local.timestamp_opt(now.floor() as i64, 0).single()?;
        let last = Local.timestamp_opt(last.floor() as i64, 0).single()?;
        let date = if now.hour() >= self.at_hour {
            now.date_naive()
        } else {
            now.date_naive().pred_opt()?
        };
        let boundary = date.and_hms_opt(self.at_hour, 0, 0)?;
        (last.naive_local() < boundary).then_some("daily")
    }
}

#[derive(Debug, Clone, clap::Args)]
pub struct ResetPolicyArgs {
    #[arg(value_enum)]
    mode: Option<ResetMode>,
    #[arg(long, requires = "mode")]
    idle_minutes: Option<u64>,
    #[arg(long, requires = "mode")]
    at_hour: Option<u32>,
    #[arg(long, hide = true, conflicts_with = "mode")]
    check: bool,
    #[arg(long, hide = true, requires = "check")]
    last_activity: Option<f64>,
}

impl ResetPolicyArgs {
    pub fn run(self, home: &Path) -> anyhow::Result<()> {
        if self.check {
            let policy = ResetPolicy::load(home)?
                .ok_or_else(|| anyhow::anyhow!("No gateway reset policy is configured."))?;
            let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
            println!(
                "{}",
                serde_json::json!(policy.reason(self.last_activity, now))
            );
        } else if let Some(mode) = self.mode {
            let path = ResetPolicy::path(home);
            let Some(_lock) = crate::vendor::lock_file_at(
                &path.with_extension("lock"),
                std::time::Duration::from_secs(2),
            )?
            else {
                anyhow::bail!("Cannot lock gateway reset policy.");
            };
            let previous = ResetPolicy::load(home)?;
            let policy = ResetPolicy {
                mode,
                idle_minutes: self
                    .idle_minutes
                    .or_else(|| previous.as_ref().map(|p| p.idle_minutes))
                    .unwrap_or(1440),
                at_hour: self
                    .at_hour
                    .or_else(|| previous.as_ref().map(|p| p.at_hour))
                    .unwrap_or(4),
            };
            policy.validate()?;
            crate::cron::store::atomic_write_json(&path, &policy)?;
            println!("{}", policy.summary());
        } else {
            println!("{}", ResetPolicy::display(home)?);
        }
        Ok(())
    }
}
