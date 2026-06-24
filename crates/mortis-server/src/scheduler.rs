//! Background scheduling: per-repo sync jobs and the session reaper.
//!
//! A repo's `schedule` is either a human duration (`"15m"`) → a repeating
//! interval job, or anything else → a cron expression. The reaper runs on a
//! fixed interval and deletes idle sessions past their TTL.

use std::sync::Arc;
use std::time::Duration;

use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{info, warn};

use mortis_app::Services;
use mortis_core::RepoId;

/// Build and start the scheduler. The returned [`JobScheduler`] must be kept
/// alive for jobs to keep firing.
pub async fn start(
    services: Arc<Services>,
    reap_ttl: Duration,
    reap_interval: Duration,
) -> anyhow::Result<JobScheduler> {
    let sched = JobScheduler::new().await?;

    for entry in services.registry().all() {
        let Some(schedule) = entry.spec.schedule.clone() else {
            continue;
        };
        let id = entry.spec.id.clone();
        let job = make_sync_job(&schedule, id.clone(), services.clone())?;
        sched.add(job).await?;
        info!("scheduled repo '{id}' ({schedule})");
    }

    let svc = services.clone();
    let reaper = Job::new_repeated_async(reap_interval, move |_uuid, _lock| {
        let svc = svc.clone();
        Box::pin(async move {
            match svc.reap_sessions(reap_ttl).await {
                Ok(n) if n > 0 => info!("reaped {n} expired session(s)"),
                Ok(_) => {}
                Err(e) => warn!("session reap failed: {e}"),
            }
        })
    })?;
    sched.add(reaper).await?;

    sched.start().await?;
    Ok(sched)
}

fn make_sync_job(schedule: &str, id: RepoId, svc: Arc<Services>) -> anyhow::Result<Job> {
    let job = if let Ok(interval) = humantime::parse_duration(schedule) {
        Job::new_repeated_async(interval, move |_uuid, _lock| {
            let (svc, id) = (svc.clone(), id.clone());
            Box::pin(async move { run_sync(&svc, &id).await })
        })?
    } else {
        Job::new_async(schedule, move |_uuid, _lock| {
            let (svc, id) = (svc.clone(), id.clone());
            Box::pin(async move { run_sync(&svc, &id).await })
        })?
    };
    Ok(job)
}

async fn run_sync(svc: &Services, id: &RepoId) {
    match svc.sync_repo(id).await {
        Ok(snap) => info!("synced '{id}' @ {}", snap.head),
        Err(e) => warn!("scheduled sync of '{id}' failed: {e}"),
    }
}
