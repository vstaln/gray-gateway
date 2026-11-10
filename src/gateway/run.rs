use std::time::Duration;

#[cfg(unix)]
use crate::gateway::socket;
use crate::gateway::{pid, state};

const DRAIN_SECS: u64 = 65;

const TICK_KIND: &str = "gateway";

pub async fn run_foreground() -> anyhow::Result<()> {
    let home = crate::vendor::gray_home()?;
    let record = pid::claim(&home)?;
    let started_at = record.started_at;

    let panic_home = home.clone();
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = state::write(
            &panic_home,
            &state::record(
                state::STATE_STOPPED,
                Some(&format!("panic: {info}")),
                started_at,
            ),
        );
        prev_hook(info);
    }));
    log::info!(
        "gateway: starting (pid {}, home {})",
        record.pid,
        home.display()
    );

    let lifecycle_dir = home.join("gateway");
    match crate::gateway::lifecycle::boot(&lifecycle_dir) {
        crate::gateway::lifecycle::Previous::Crashed => {
            log::warn!("gateway: previous run ended unexpectedly (crash, kill, or reboot)")
        }
        previous => log::info!("gateway: previous run: {}", previous.as_str()),
    }
    let _ = state::write(
        &home,
        &state::record(state::STATE_STARTING, None, started_at),
    );

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    #[cfg(unix)]
    let socket_task = {
        let socket_home = home.clone();
        tokio::spawn(async move {
            if let Err(e) = socket::serve(socket_home, stop_rx).await {
                log::warn!("gateway: control socket disabled: {e:#}");
            }
        })
    };
    #[cfg(not(unix))]
    let socket_task = {
        let _ = stop_rx;
        tokio::spawn(async move {
            log::warn!("gateway: control socket unavailable on this platform");
        })
    };
    let _ = state::write(
        &home,
        &state::record(state::STATE_RUNNING, None, started_at),
    );

    let (cron_stop_tx, cron_thread) =
        spawn_ticker(home.clone(), Duration::from_secs(60), TICK_KIND)?;

    let gray_bin = crate::vendor::gray_bin();
    let runner = std::sync::Arc::new(crate::gateway::turn::ChildRunner {
        gray_bin,
        home: home.clone(),
    });
    let mut brain = crate::gateway::brain::Brain::new(&home, runner);
    brain.recover();
    let mut done = brain.take_done();
    let mut poll = tokio::time::interval(Duration::from_secs(POLL_SECS));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let exe_watch = crate::vendor::ExeWatch::capture();
    let mut last_probe = std::time::Instant::now() - Duration::from_secs(30);
    let mut stop_signal = stop_signal();
    let exit_reason: &'static str;
    loop {
        tokio::select! {
            _ = poll.tick() => {
                brain.step(crate::cron::now_secs());
                if last_probe.elapsed() >= Duration::from_secs(30) {
                    last_probe = std::time::Instant::now();
                    if let Some(w) = &exe_watch
                        && w.replaced()
                        && brain.busy_keys() == 0
                    {
                        log::info!("gateway: binary replaced; exiting for restart");
                        exit_reason = "binary replaced";
                        break;
                    }
                }
            }
            Some(finished) = done.recv() => {
                brain.finish(finished);
                brain.step(crate::cron::now_secs());
            }
            reason = stop_signal.recv() => {
                exit_reason = reason.unwrap_or("stop");
                break;
            }
        }
    }

    while let Ok(finished) = done.try_recv() {
        brain.finish(finished);
    }

    brain.abort_all();
    let _ = cron_stop_tx.send(true);
    let _ = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(DRAIN_SECS + 5);
        while !cron_thread.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
    })
    .await;

    let restart = crate::gateway::lifecycle::mark_stopped(&lifecycle_dir);
    let exit_reason = if restart { "restart" } else { exit_reason };
    let _ = state::write(
        &home,
        &state::record(state::STATE_STOPPED, Some(exit_reason), started_at),
    );
    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(3), socket_task).await;
    pid::remove_owned(&home, record.pid);
    log::info!("gateway: stopped ({exit_reason})");
    Ok(())
}

const POLL_SECS: u64 = 2;

pub(crate) fn spawn_ticker(
    home: std::path::PathBuf,
    tick_every: Duration,
    kind: &'static str,
) -> std::io::Result<(
    tokio::sync::watch::Sender<bool>,
    std::thread::JoinHandle<()>,
)> {
    let (tx, rx) = tokio::sync::watch::channel(false);
    let handle = std::thread::Builder::new()
        .name("gray-cron".into())
        .spawn(move || cron_thread(home, rx, tick_every, kind))?;
    Ok((tx, handle))
}

pub(crate) fn cron_thread(
    home: std::path::PathBuf,
    mut stop: tokio::sync::watch::Receiver<bool>,
    tick_every: Duration,
    kind: &'static str,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::warn!("gateway: cron thread has no runtime, cron disabled: {e:#}");
            return;
        }
    };
    rt.block_on(async move {
        let store = match crate::cron::CronStore::open(home.join("cron")) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("gateway: cron store unavailable, cron disabled: {e:#}");
                return;
            }
        };
        let runner = crate::cron_serve::SubprocessRunner {
            gray_bin: crate::vendor::gray_bin(),
            home: home.clone(),
        };
        let deliver = crate::cron_serve::SaveLocalDeliver { home: home.clone() };

        let first = if tick_every >= Duration::from_secs(60) {
            tokio::time::Instant::now()
                + Duration::from_secs(60 - (crate::cron::now_secs() as u64 % 60))
        } else {
            tokio::time::Instant::now() + tick_every
        };
        let mut interval = tokio::time::interval_at(first, tick_every);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let tick = crate::cron_serve::tick_once(&store, &runner, &deliver, kind);
                    tokio::pin!(tick);
                    tokio::select! {
                        report = &mut tick => match report {
                            Ok(rep) => {
                                log::info!("gateway: cron tick fired={} errors={}", rep.fired, rep.errors);
                                forward_cron_outbox(&home);
                            }
                            Err(e) => log::warn!("gateway: cron tick failed: {e:#}"),
                        },
                        _ = stop.changed() => {
                            drain(tick).await;
                            return;
                        }
                    }
                }
                _ = stop.changed() => return,
            }
        }
    });
}

#[cfg(unix)]
fn stop_signal() -> tokio::sync::mpsc::UnboundedReceiver<&'static str> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .expect("SIGINT handler");
    tokio::spawn(async move {
        tokio::select! {
            _ = sigterm.recv() => { let _ = tx.send("SIGTERM"); }
            _ = sigint.recv() => { let _ = tx.send("SIGINT"); }
        }
    });
    rx
}

#[cfg(not(unix))]
fn stop_signal() -> tokio::sync::mpsc::UnboundedReceiver<&'static str> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = tx.send("stop");
    });
    rx
}

async fn drain<F: std::future::Future<Output = anyhow::Result<crate::cron_serve::TickReport>>>(
    tick: std::pin::Pin<&mut F>,
) {
    log::info!("gateway: draining in-flight cron tick (up to {DRAIN_SECS}s)…");
    match tokio::time::timeout(Duration::from_secs(DRAIN_SECS), tick).await {
        Ok(Ok(rep)) => log::info!(
            "gateway: drained — fired={} errors={}",
            rep.fired,
            rep.errors
        ),
        Ok(Err(e)) => log::warn!("gateway: drained with tick error: {e:#}"),
        Err(_) => log::warn!(
            "gateway: drain window elapsed; abandoning the fire (its claim expires within 300s)"
        ),
    }
}

/// Chat-bound cron fires land in `$GRAY_HOME/cron/outbox`, the contract a
/// `gray cron tick --json` host drains. The daemon is its own host, and
/// platform plugins pull from the gateway outbox instead, so without this
/// hop a scheduled job meant for Discord/Telegram/... was fired, marked ok,
/// and never posted. Each entry becomes a trigger intent on its route.
pub(crate) fn forward_cron_outbox(home: &std::path::Path) -> usize {
    let dir = crate::gateway::state_dir(home);
    let mut forwarded = 0;
    for line in crate::cron_serve::drain_outbox(home) {
        let Some(intent) = cron_intent(&line) else {
            log::warn!("gateway: cron delivery without a chat route dropped");
            continue;
        };
        match crate::gateway::outbox::enqueue(&dir, &intent) {
            Ok(()) => forwarded += 1,
            Err(e) => log::warn!("gateway: cannot queue cron delivery {}: {e:#}", intent.key),
        }
    }
    forwarded
}

fn cron_intent(line: &str) -> Option<crate::gateway::outbox::Intent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let field = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(str::to_string)
    };
    let route = crate::cron::store::Origin {
        platform: field("platform")?,
        chat: field("chat")?,
        thread: field("thread"),
        route: field("route"),
    };
    let key = format!("cron:{}", field("job_id").unwrap_or_default());
    let text = field("text").or_else(|| field("final_text"))?;
    Some(crate::gateway::outbox::Intent::new(
        &key,
        crate::gateway::event::Kind::Trigger,
        Some(route),
        &text,
    ))
}

#[cfg(test)]
mod cron_forward_tests {
    use super::*;

    #[test]
    fn hosted_cron_delivery_reaches_the_gateway_outbox() {
        let home = tempfile::tempdir().unwrap();
        let line = serde_json::json!({
            "type": "cron_delivery", "job_id": "abc", "name": "morning",
            "platform": "discord", "chat": "11", "thread": null, "route": "11",
            "kind": "reminder", "status": "ok", "final_text": "wake up",
            "text": "wake up", "path": "/x",
        });
        let outbox = home.path().join("cron").join("outbox");
        std::fs::create_dir_all(&outbox).unwrap();
        std::fs::write(outbox.join("0001.json"), line.to_string()).unwrap();

        assert_eq!(forward_cron_outbox(home.path()), 1);
        assert!(crate::cron_serve::drain_outbox(home.path()).is_empty());
        let dir = crate::gateway::state_dir(home.path());
        let pulled = crate::gateway::outbox::pull(&dir, "discord", crate::cron::now_secs(), 10);
        assert_eq!(pulled.len(), 1);
        assert_eq!(pulled[0].text, "wake up");
        assert_eq!(pulled[0].route.as_ref().unwrap().chat, "11");
        assert_eq!(pulled[0].key, "cron:abc");
    }

    #[test]
    fn routeless_lines_are_dropped_not_queued() {
        assert!(cron_intent(r#"{"platform":"","chat":"","text":"x"}"#).is_none());
        assert!(cron_intent("not json").is_none());
    }
}
