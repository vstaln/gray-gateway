use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use crate::cron::schedule::{ONESHOT_GRACE_SECS, Schedule, next_run, parse_schedule};

pub const FIRE_CLAIM_TTL_SECS: i64 = 1200;

pub const MAX_NAME_LEN: usize = 50;

const JOB_ID_HEX_LEN: usize = 12;

pub const TICKER_STALE_SECS: i64 = 300;

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Claim {
    pub at: i64,
    pub by: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TickStamp {
    pub at: i64,
    pub pid: u32,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverdueJob {
    pub id: String,
    pub name: String,
    pub next_run_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CronHealth {
    pub last_tick: Option<TickStamp>,
    pub overdue: Vec<OverdueJob>,
}

impl CronHealth {
    pub fn ticker_live(&self, now: i64) -> bool {
        self.last_tick
            .as_ref()
            .is_some_and(|t| now.saturating_sub(t.at) <= TICKER_STALE_SECS)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Deliver {
    Origin,
    #[default]
    Local,
    Target(String),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Origin {
    pub platform: String,

    pub chat: String,
    #[serde(default)]
    pub thread: Option<String>,

    #[serde(default)]
    pub route: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    #[default]
    Active,

    #[serde(alias = "awaiting_approval")]
    Paused,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RunStatus {
    #[serde(rename = "ok")]
    Ok,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "delivery_failed")]
    DeliveryFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Repeat {
    #[serde(default)]
    pub times: Option<u32>,
    #[serde(default)]
    pub completed: u32,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub schedule: Schedule,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub state: JobState,
    pub created_at: i64,
    #[serde(default)]
    pub next_run_at: Option<i64>,
    #[serde(default)]
    pub last_run_at: Option<i64>,
    #[serde(default)]
    pub last_status: Option<RunStatus>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_delivery_error: Option<String>,
    #[serde(default)]
    pub deliver: Deliver,
    #[serde(default)]
    pub origin: Option<Origin>,
    #[serde(default)]
    pub workdir: Option<PathBuf>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub script: Option<PathBuf>,
    #[serde(default)]
    pub fire_claim: Option<Claim>,

    #[serde(default)]
    pub reminder: bool,
    #[serde(default)]
    pub repeat: Repeat,
}

fn default_enabled() -> bool {
    true
}

pub fn atomic_write_json(path: &Path, value: &impl serde::Serialize) -> anyhow::Result<()> {
    use std::io::Write as _;
    let body = serde_json::to_string_pretty(value)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let tmp = parent.join(format!(".gray-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> anyhow::Result<()> {
        let mut file = options.open(&tmp)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn with_jobs_lock<T>(lock_path: &Path, f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "cron lock timeout on {}",
                    lock_path.display()
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }

    f()
}

fn validate_new_job(
    name: &str,
    prompt: &str,
    workdir: Option<&Path>,
    skills: &[String],
    script: Option<&Path>,
) -> anyhow::Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!("job name is empty");
    }
    if name.chars().count() > MAX_NAME_LEN {
        anyhow::bail!("job name exceeds {MAX_NAME_LEN} chars");
    }
    if prompt.trim().is_empty() {
        anyhow::bail!("job prompt is empty");
    }
    if let Some(dir) = workdir {
        if !dir.is_absolute() {
            anyhow::bail!("workdir must be absolute: {}", dir.display());
        }
        if !dir.is_dir() {
            anyhow::bail!("workdir is not an existing dir: {}", dir.display());
        }
    }
    for s in skills {
        anyhow::ensure!(!s.trim().is_empty(), "job skill is empty");
        anyhow::ensure!(
            s.chars().count() <= MAX_NAME_LEN,
            "job skill exceeds {MAX_NAME_LEN} chars"
        );
    }
    if let Some(path) = script {
        anyhow::ensure!(
            path.is_absolute(),
            "script must be absolute: {}",
            path.display()
        );
        anyhow::ensure!(
            path.is_file(),
            "script is not an existing file: {}",
            path.display()
        );
    }
    Ok(())
}

fn new_job_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..JOB_ID_HEX_LEN].to_owned()
}

#[derive(Debug, Clone)]
pub struct CronStore {
    cron_dir: PathBuf,
}

impl CronStore {
    pub fn open(dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let cron_dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&cron_dir)?;
        Ok(Self { cron_dir })
    }

    fn jobs_path(&self) -> PathBuf {
        self.cron_dir.join("jobs.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.cron_dir.join(".jobs.lock")
    }

    fn tick_stamp_path(&self) -> PathBuf {
        self.cron_dir.join(".last_tick")
    }

    pub fn record_tick(&self, kind: &str) -> anyhow::Result<()> {
        atomic_write_json(
            &self.tick_stamp_path(),
            &TickStamp {
                at: now_secs(),
                pid: std::process::id(),
                kind: kind.to_string(),
            },
        )
    }

    pub fn last_tick(&self) -> anyhow::Result<Option<TickStamp>> {
        let body = match std::fs::read_to_string(self.tick_stamp_path()) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match serde_json::from_str(&body) {
            Ok(stamp) => Ok(Some(stamp)),
            Err(e) => {
                log::warn!(
                    "cron: unreadable tick stamp {} ({e}), treating as never ticked",
                    self.tick_stamp_path().display()
                );
                Ok(None)
            }
        }
    }

    pub fn health(&self, now: i64) -> anyhow::Result<CronHealth> {
        let last_tick = self.last_tick()?;
        let mut overdue = Vec::new();
        for job in self.list()? {
            if !job.enabled || job.state != JobState::Active {
                continue;
            }
            let Some(next) = job.next_run_at else {
                continue;
            };
            if next + TICKER_STALE_SECS >= now {
                continue;
            }
            if let Some(claim) = &job.fire_claim
                && now.saturating_sub(claim.at) <= FIRE_CLAIM_TTL_SECS
            {
                continue;
            }
            overdue.push(OverdueJob {
                id: job.id.clone(),
                name: job.name.clone(),
                next_run_at: next,
            });
        }
        Ok(CronHealth { last_tick, overdue })
    }

    fn load_raw(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        let data = match std::fs::read_to_string(self.jobs_path()) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
            Ok(d) => d,
        };
        if data.trim().is_empty() {
            return Ok(Vec::new());
        }
        match serde_json::from_str::<serde_json::Value>(data.trim()) {
            Ok(serde_json::Value::Array(items)) => Ok(items),
            Ok(serde_json::Value::Object(mut obj)) => match obj.remove("jobs") {
                Some(serde_json::Value::Array(items)) => Ok(items),
                _ => anyhow::bail!("cron store has no jobs array"),
            },
            Ok(_) => anyhow::bail!("cron store is not a JSON array"),
            Err(e) => Err(anyhow::anyhow!("cron store is not JSON ({e})")),
        }
    }

    fn parse_jobs(raw: &[serde_json::Value]) -> Vec<CronJob> {
        let mut jobs = Vec::new();
        for v in raw {
            match serde_json::from_value::<CronJob>(v.clone()) {
                Ok(j) => jobs.push(j),
                Err(e) => log::warn!("cron store: skipping malformed record: {e}"),
            }
        }
        jobs
    }

    fn save_jobs(&self, raw: &mut Vec<serde_json::Value>, jobs: &[CronJob]) -> anyhow::Result<()> {
        for job in jobs {
            let v = serde_json::to_value(job)?;
            match raw
                .iter_mut()
                .find(|x| x.get("id").and_then(|i| i.as_str()) == Some(job.id.as_str()))
            {
                Some(slot) => *slot = v,
                None => raw.push(v),
            }
        }
        atomic_write_json(&self.jobs_path(), raw)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_full(
        &self,
        name: &str,
        schedule: &str,
        prompt: &str,
        deliver: Deliver,
        origin: Option<Origin>,
        workdir: Option<PathBuf>,
        skills: Vec<String>,
        script: Option<PathBuf>,
        reminder: bool,
    ) -> anyhow::Result<String> {
        self.insert_job(
            name, schedule, prompt, deliver, origin, workdir, skills, script, reminder,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_job(
        &self,
        name: &str,
        schedule: &str,
        prompt: &str,
        deliver: Deliver,
        origin: Option<Origin>,
        workdir: Option<PathBuf>,
        skills: Vec<String>,
        script: Option<PathBuf>,
        reminder: bool,
    ) -> anyhow::Result<String> {
        validate_new_job(name, prompt, workdir.as_deref(), &skills, script.as_deref())?;
        let sched = parse_schedule(schedule)?;
        let now = now_secs();
        if let Schedule::Once { at } = &sched
            && *at < now - ONESHOT_GRACE_SECS
        {
            anyhow::bail!("one-shot time is more than {ONESHOT_GRACE_SECS}s in the past");
        }
        let next = next_run(now, &sched);
        let job = CronJob {
            id: new_job_id(),
            name: name.trim().to_string(),
            prompt: prompt.to_string(),
            schedule: sched,
            enabled: true,
            state: JobState::Active,
            created_at: now,
            next_run_at: next,
            last_run_at: None,
            last_status: None,
            last_error: None,
            last_delivery_error: None,
            deliver,
            origin,
            workdir,
            skills: skills.into_iter().map(|s| s.trim().to_string()).collect(),
            script,
            fire_claim: None,
            reminder,
            repeat: Repeat::default(),
        };
        let id = job.id.clone();
        with_jobs_lock(&self.lock_path(), || {
            let mut raw = self.load_raw()?;
            self.save_jobs(&mut raw, &[job])
        })?;
        Ok(id)
    }

    pub fn list(&self) -> anyhow::Result<Vec<CronJob>> {
        let lock = self.lock_path();
        with_jobs_lock(&lock, || {
            let mut jobs = Self::parse_jobs(&self.load_raw()?);
            jobs.sort_by_key(|j| j.next_run_at.unwrap_or(i64::MAX));
            Ok(jobs)
        })
    }

    pub fn get(&self, id_or_name: &str) -> anyhow::Result<Option<CronJob>> {
        let lock = self.lock_path();
        with_jobs_lock(&lock, || {
            Ok(Self::parse_jobs(&self.load_raw()?)
                .into_iter()
                .find(|j| j.id == id_or_name || j.name == id_or_name))
        })
    }

    pub fn remove(&self, id_or_name: &str) -> anyhow::Result<bool> {
        with_jobs_lock(&self.lock_path(), || {
            let mut raw = self.load_raw()?;
            let before = raw.len();
            raw.retain(|x| {
                let id_match = x.get("id").and_then(|i| i.as_str()) == Some(id_or_name);
                let name_match = x.get("name").and_then(|n| n.as_str()) == Some(id_or_name);
                !(id_match || name_match)
            });
            if raw.len() == before {
                return Ok(false);
            }
            atomic_write_json(&self.jobs_path(), &raw)?;
            Ok(true)
        })
    }

    pub(crate) fn claim_due_limited(
        &self,
        now: i64,
        owner: &str,
        limit: usize,
        exclude: &[String],
    ) -> anyhow::Result<Vec<CronJob>> {
        with_jobs_lock(&self.lock_path(), || {
            let mut raw = self.load_raw()?;
            let mut jobs = Self::parse_jobs(&raw);
            let mut due = Vec::new();
            let mut dirty = false;
            for job in jobs.iter_mut() {
                if due.len() >= limit || exclude.contains(&job.id) {
                    continue;
                }
                if !job.enabled || job.state != JobState::Active {
                    continue;
                }

                if let Some(claim) = &job.fire_claim {
                    if now.saturating_sub(claim.at) <= FIRE_CLAIM_TTL_SECS {
                        continue;
                    }
                    log::warn!(
                        "cron job {}: reclaiming stale fire claim by {} ({}s old)",
                        job.id,
                        claim.by,
                        now.saturating_sub(claim.at)
                    );
                    job.fire_claim = None;
                    dirty = true;
                }
                if matches!(job.schedule, Schedule::Once { .. }) && job.last_run_at.is_some() {
                    continue;
                }
                let next = match job.next_run_at {
                    Some(n) => n,
                    None => match next_run(now, &job.schedule) {
                        Some(n) => {
                            job.next_run_at = Some(n);
                            dirty = true;
                            n
                        }
                        None => {
                            log::warn!(
                                "cron job {}: schedule no longer resolves; skipping",
                                job.id
                            );
                            continue;
                        }
                    },
                };
                if next > now {
                    continue;
                }
                if matches!(job.schedule, Schedule::Once { .. }) {
                    if next < now - ONESHOT_GRACE_SECS {
                        job.enabled = false;
                        job.state = JobState::Done;
                        job.last_error = Some("missed one-shot window".to_string());
                        dirty = true;
                        continue;
                    }
                } else {
                    match next_run(now, &job.schedule) {
                        Some(advanced) => {
                            job.next_run_at = Some(advanced);
                        }
                        None => {
                            log::warn!("cron job {}: cannot advance schedule; skipping", job.id);
                            continue;
                        }
                    }
                }
                job.fire_claim = Some(Claim {
                    at: now,
                    by: owner.to_string(),
                });
                dirty = true;
                due.push(job.clone());
            }
            if dirty {
                self.save_jobs(&mut raw, &jobs)?;
            }
            Ok(due)
        })
    }

    pub fn set_paused(&self, id_or_name: &str, paused: bool) -> anyhow::Result<bool> {
        with_jobs_lock(&self.lock_path(), || {
            let mut raw = self.load_raw()?;
            let mut jobs = Self::parse_jobs(&raw);
            let Some(job) = jobs
                .iter_mut()
                .find(|j| j.id == id_or_name || j.name == id_or_name)
            else {
                return Ok(false);
            };
            if paused {
                job.state = JobState::Paused;
            } else {
                job.state = JobState::Active;
                job.next_run_at = next_run(now_secs(), &job.schedule);
            }
            self.save_jobs(&mut raw, &jobs)?;
            Ok(true)
        })
    }

    pub fn claim_one(
        &self,
        now: i64,
        owner: &str,
        id_or_name: &str,
    ) -> anyhow::Result<Option<CronJob>> {
        with_jobs_lock(&self.lock_path(), || {
            let mut raw = self.load_raw()?;
            let mut jobs = Self::parse_jobs(&raw);
            let Some(job) = jobs
                .iter_mut()
                .find(|j| j.id == id_or_name || j.name == id_or_name)
            else {
                return Ok(None);
            };
            if !job.enabled || job.state != JobState::Active {
                return Ok(None);
            }
            if let Some(claim) = &job.fire_claim
                && now.saturating_sub(claim.at) <= FIRE_CLAIM_TTL_SECS
            {
                return Ok(None);
            }
            job.fire_claim = None;
            if matches!(job.schedule, Schedule::Once { .. }) && job.last_run_at.is_some() {
                return Ok(None);
            }
            if matches!(job.schedule, Schedule::Once { at } if at < now - ONESHOT_GRACE_SECS) {
                job.enabled = false;
                job.state = JobState::Done;
                job.last_error = Some("missed one-shot window".to_string());
                self.save_jobs(&mut raw, &jobs)?;
                return Ok(None);
            }
            if !matches!(job.schedule, Schedule::Once { .. }) {
                match next_run(now, &job.schedule) {
                    Some(advanced) => job.next_run_at = Some(advanced),
                    None => return Ok(None),
                }
            }
            job.fire_claim = Some(Claim {
                at: now,
                by: owner.to_string(),
            });
            let out = job.clone();
            self.save_jobs(&mut raw, &jobs)?;
            Ok(Some(out))
        })
    }

    pub fn mark_done(
        &self,
        id_or_name: &str,
        claim: Option<&Claim>,
        status: RunStatus,
        err: Option<&str>,
    ) -> anyhow::Result<()> {
        with_jobs_lock(&self.lock_path(), || {
            let mut raw = self.load_raw()?;
            let mut jobs = Self::parse_jobs(&raw);
            let Some(job) = jobs
                .iter_mut()
                .find(|j| j.id == id_or_name || j.name == id_or_name)
            else {
                anyhow::bail!("unknown cron job {id_or_name:?}");
            };
            anyhow::ensure!(
                job.fire_claim.as_ref() == claim,
                "cron claim no longer owned by this worker"
            );
            job.fire_claim = None;
            job.last_status = Some(status);
            job.last_run_at = Some(now_secs());
            match status {
                RunStatus::DeliveryFailed => job.last_delivery_error = err.map(str::to_string),
                _ => job.last_error = err.map(str::to_string),
            }
            job.repeat.completed = job.repeat.completed.saturating_add(1);
            let exhausted = job.repeat.times.is_some_and(|t| job.repeat.completed >= t);
            if exhausted || matches!(job.schedule, Schedule::Once { .. }) {
                job.state = JobState::Done;
            }
            self.save_jobs(&mut raw, &jobs)
        })
    }

    pub fn update(
        &self,
        id_or_name: &str,
        f: impl FnOnce(&mut CronJob) -> anyhow::Result<()>,
    ) -> anyhow::Result<bool> {
        with_jobs_lock(&self.lock_path(), || {
            let mut raw = self.load_raw()?;
            let mut jobs = Self::parse_jobs(&raw);
            let Some(job) = jobs
                .iter_mut()
                .find(|j| j.id == id_or_name || j.name == id_or_name)
            else {
                return Ok(false);
            };
            let before = job.schedule.clone();
            f(job)?;
            validate_new_job(
                &job.name,
                &job.prompt,
                job.workdir.as_deref(),
                &job.skills,
                job.script.as_deref(),
            )?;
            if job.schedule != before && job.state == JobState::Active {
                job.next_run_at = next_run(now_secs(), &job.schedule);
            }
            self.save_jobs(&mut raw, &jobs)?;
            Ok(true)
        })
    }
}
