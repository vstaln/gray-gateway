fn cron_store() -> anyhow::Result<crate::cron::CronStore> {
    let home = crate::vendor::gray_home()?;
    crate::cron::CronStore::open(home.join("cron"))
}

fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| ts.to_string())
}

pub(crate) fn fmt_ts_opt(ts: Option<i64>) -> String {
    ts.map(fmt_ts).unwrap_or_else(|| "-".to_string())
}

fn fmt_dur(mut secs: u64) -> String {
    if secs.is_multiple_of(86400) {
        return format!("{}d", secs / 86400);
    }
    if secs.is_multiple_of(3600) {
        return format!("{}h", secs / 3600);
    }

    secs /= 60;
    format!("{secs}m")
}

fn fmt_schedule(s: &crate::cron::Schedule) -> String {
    match s {
        crate::cron::Schedule::Interval { secs } => format!("every {}", fmt_dur(*secs)),
        crate::cron::Schedule::Cron { expr } => expr.clone(),
        crate::cron::Schedule::Once { at } => format!("once {}", fmt_ts(*at)),
    }
}

fn fmt_deliver(d: &crate::cron::Deliver) -> String {
    match d {
        crate::cron::Deliver::Origin => "origin".to_string(),
        crate::cron::Deliver::Local => "local".to_string(),
        crate::cron::Deliver::Target(s) => s.clone(),
    }
}

fn fmt_status(s: Option<crate::cron::RunStatus>) -> &'static str {
    match s {
        None => "-",
        Some(crate::cron::RunStatus::Ok) => "ok",
        Some(crate::cron::RunStatus::Error) => "error",
        Some(crate::cron::RunStatus::DeliveryFailed) => "delivery_failed",
    }
}

fn parse_deliver_flag(raw: Option<&str>) -> crate::cron::Deliver {
    match raw.map(str::trim).unwrap_or("local") {
        s if s.eq_ignore_ascii_case("origin") => crate::cron::Deliver::Origin,
        s if s.eq_ignore_ascii_case("local") || s.is_empty() => crate::cron::Deliver::Local,
        s => crate::cron::Deliver::Target(s.to_string()),
    }
}

fn origin_from_env() -> Option<crate::cron::store::Origin> {
    let raw = std::env::var("GRAY_CRON_ORIGIN").ok()?;
    let v: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
    let platform = v.get("platform")?.as_str()?.trim().to_string();
    let chat = v.get("chat")?.as_str()?.trim().to_string();
    if platform.is_empty() || chat.is_empty() {
        return None;
    }
    let field = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(crate::cron::store::Origin {
        platform,
        chat,
        thread: field("thread"),
        route: field("route"),
    })
}

fn session_origin_from_env() -> Option<crate::cron::store::Origin> {
    let sid = std::env::var("GRAY_SESSION_ID").ok()?;
    let sid = sid.trim();
    (!sid.is_empty()).then(|| crate::cron::store::Origin {
        platform: crate::cron_serve::SESSION_PLATFORM.to_string(),
        chat: sid.to_string(),
        thread: None,
        route: None,
    })
}

fn default_job_name(prompt: &str) -> String {
    let name: String = prompt
        .lines()
        .next()
        .unwrap_or("job")
        .trim()
        .chars()
        .take(40)
        .collect();
    if name.trim().is_empty() {
        "job".to_string()
    } else {
        name.trim().to_string()
    }
}

pub async fn run_cron(cmd: crate::CronCmd) -> anyhow::Result<()> {
    use crate::CronCmd;
    if cfg!(windows)
        && matches!(
            &cmd,
            CronCmd::Tick { .. } | CronCmd::Serve | CronCmd::Run { .. }
        )
    {
        anyhow::bail!(
            "cron execution is not supported on native Windows; use a WSL execution host"
        );
    }
    match cmd {
        CronCmd::List => {
            let store = cron_store()?;
            let jobs = store.list()?;
            if jobs.is_empty() {
                println!("no cron jobs");
                return Ok(());
            }
            for j in &jobs {
                println!(
                    "{} {} {} next={} last={}",
                    j.id,
                    j.name,
                    fmt_schedule(&j.schedule),
                    fmt_ts_opt(j.next_run_at),
                    fmt_status(j.last_status)
                );
            }

            let now = crate::cron::now_secs();
            println!(
                "{}",
                crate::cron_status::ticker_line(&store.health(now)?, now)
            );
            Ok(())
        }
        CronCmd::Add {
            schedule,
            prompt,
            deliver,
            origin_session,
            name,
            workdir,
            skills,
            script,
            reminder,
        } => {
            let store = cron_store()?;
            if reminder && (script.is_some() || skills.is_some()) {
                anyhow::bail!("--reminder cannot be combined with --script or --skills");
            }
            let name = name.unwrap_or_else(|| {
                if reminder {
                    crate::cron_fire::reminder_name(&prompt)
                } else {
                    default_job_name(&prompt)
                }
            });
            let base: std::path::PathBuf = workdir
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let skill_names: Vec<String> = skills
                .as_deref()
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            for s in &skill_names {
                if crate::vendor::resolve_skill_name(&base, s).is_none() {
                    anyhow::bail!("unknown skill {s:?}");
                }
            }

            let env_origin = origin_from_env().or_else(session_origin_from_env);
            let deliver_kind = match (&deliver, &env_origin) {
                (None, Some(_)) => crate::cron::Deliver::Origin,
                _ => parse_deliver_flag(deliver.as_deref()),
            };
            let origin = if matches!(deliver_kind, crate::cron::Deliver::Origin) {
                match (origin_session.as_deref(), &env_origin) {
                    (Some(chat), _) if !chat.trim().is_empty() => {
                        Some(crate::cron::store::Origin {
                            platform: env_origin
                                .as_ref()
                                .map(|o| o.platform.clone())
                                .unwrap_or_else(|| "local".to_string()),
                            chat: chat.trim().to_string(),
                            thread: None,
                            route: env_origin.as_ref().and_then(|o| o.route.clone()),
                        })
                    }
                    (_, Some(o)) => Some(o.clone()),
                    _ => {
                        return Err(anyhow::anyhow!(
                            "--deliver origin requires --origin-session <session-id>"
                        ));
                    }
                }
            } else {
                None
            };

            let chat_bound = origin.as_ref().is_some_and(crate::cron_serve::is_hosted);
            let id = store.add_full(
                &name,
                &schedule,
                &prompt,
                deliver_kind,
                origin,
                workdir,
                skill_names,
                script,
                reminder,
            )?;
            let next = store
                .get(&id)?
                .map(|j| fmt_ts_opt(j.next_run_at))
                .unwrap_or_else(|| "-".to_string());
            println!("added {id} next {next}");
            if cfg!(windows) {
                println!(
                    "Stored only: cron execution is not supported on native Windows. Use a supported execution host."
                );
            }
            let stamp = store.last_tick()?;
            if let Some(warn) =
                crate::cron_status::add_warning(stamp.as_ref(), crate::cron::now_secs(), chat_bound)
            {
                println!("{warn}");
            }
            Ok(())
        }
        CronCmd::Show { id } => {
            let Some(j) = cron_store()?.get(&id)? else {
                anyhow::bail!("unknown cron job {id:?}");
            };
            println!("id: {}", j.id);
            println!("name: {}", j.name);
            println!("schedule: {}", fmt_schedule(&j.schedule));
            println!("deliver: {}", fmt_deliver(&j.deliver));
            println!("enabled: {} state: {:?}", j.enabled, j.state);
            println!("created: {}", fmt_ts(j.created_at));
            println!("next run: {}", fmt_ts_opt(j.next_run_at));
            println!(
                "last run: {} status: {}",
                fmt_ts_opt(j.last_run_at),
                fmt_status(j.last_status)
            );
            if let Some(e) = &j.last_error {
                println!("last error: {e}");
            }
            if let Some(e) = &j.last_delivery_error {
                println!("last delivery error: {e}");
            }
            if let Some(o) = &j.origin {
                let thread = o.thread.as_deref().unwrap_or("-");
                let route = o.route.as_deref().unwrap_or("-");
                println!(
                    "origin: {}:{} thread:{thread} route:{route}",
                    o.platform, o.chat
                );
            }
            if let Some(w) = &j.workdir {
                println!("workdir: {}", w.display());
            }
            if !j.skills.is_empty() {
                println!("skills: {}", j.skills.join(", "));
            }
            if let Some(s) = &j.script {
                println!("script: {}", s.display());
            }
            println!("prompt: {}", j.prompt);
            Ok(())
        }
        CronCmd::Remove { id } => {
            if cron_store()?.remove(&id)? {
                println!("removed {id}");
                Ok(())
            } else {
                anyhow::bail!("unknown cron job {id:?}");
            }
        }
        CronCmd::Tick { json } => {
            let store = cron_store()?;
            let home = crate::vendor::gray_home()?;
            let runner = crate::cron_serve::SubprocessRunner {
                gray_bin: crate::vendor::gray_bin(),
                home: home.clone(),
            };
            let deliver = crate::cron_serve::SaveLocalDeliver { home: home.clone() };
            let rep = crate::cron_serve::tick_once(&store, &runner, &deliver, "cli").await?;

            if json {
                for line in crate::cron_serve::drain_outbox(&home) {
                    println!("{line}");
                }
            } else {
                for saved in rep.delivered.iter().filter(|d| d.to_chat) {
                    println!("{}", crate::cron_serve::format_fire_chat(saved));
                }
            }
            if json {
                println!(
                    "{}",
                    serde_json::json!({"type": "cron_tick", "fired": rep.fired, "errors": rep.errors})
                );
            } else {
                println!("tick: fired={} errors={}", rep.fired, rep.errors);
            }
            Ok(())
        }
        CronCmd::Serve => {
            let store = cron_store()?;
            let home = crate::vendor::gray_home()?;
            let runner = crate::cron_serve::SubprocessRunner {
                gray_bin: crate::vendor::gray_bin(),
                home: home.clone(),
            };
            crate::cron_serve::serve_loop(
                store,
                crate::cron_serve::SaveLocalDeliver { home },
                runner,
            )
            .await
        }
        CronCmd::Pause { id } => {
            if cron_store()?.set_paused(&id, true)? {
                println!("paused {id}");
                Ok(())
            } else {
                anyhow::bail!("unknown cron job {id:?}");
            }
        }
        CronCmd::Resume { id } => {
            let store = cron_store()?;
            if store.set_paused(&id, false)? {
                let next = store
                    .get(&id)?
                    .map(|j| fmt_ts_opt(j.next_run_at))
                    .unwrap_or_else(|| "-".to_string());
                println!("resumed {id} next {next}");
                Ok(())
            } else {
                anyhow::bail!("unknown cron job {id:?}");
            }
        }
        CronCmd::Run { id } => {
            let store = cron_store()?;
            let home = crate::vendor::gray_home()?;
            let now = crate::cron::now_secs();
            let owner = crate::cron_serve::owner_stamp();
            let Some(job) = store.claim_one(now, &owner, &id)? else {
                anyhow::bail!("job {id:?} is not runnable (unknown, paused, or already claimed)");
            };
            let runner = crate::cron_serve::SubprocessRunner {
                gray_bin: crate::vendor::gray_bin(),
                home: home.clone(),
            };
            let deliver = crate::cron_serve::SaveLocalDeliver { home };
            let (status, saved) =
                crate::cron_serve::fire_one(&store, &runner, job, now, &deliver).await;
            if let Some(saved) = saved.filter(|d| d.to_chat) {
                println!("{}", crate::cron_serve::format_fire_chat(&saved));
            }
            println!("ran {id} status={status:?}");
            Ok(())
        }
    }
}
