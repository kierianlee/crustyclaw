pub mod heartbeat;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_cron_scheduler::{Job, JobScheduler};
use uuid::Uuid;

use crate::claude::{InvocationQueue, RequestOrigin};
use crate::common::config::DaemonConfig;
use crate::common::status::StatusTracker;
use crate::common::util::atomic_write;
use crate::telegram;

// ---- Types ----

/// A scheduled job record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    /// Internal cron scheduler ID (changes on daemon restart).
    #[serde(skip)]
    pub id: Option<Uuid>,
    /// User-facing stable ID (persisted across restarts).
    pub stable_id: Uuid,
    pub name: String,
    pub cron_expression: String,
    pub action: JobAction,
    #[serde(default)]
    pub one_shot: bool,
}

/// What a scheduled job does when it fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobAction {
    /// Run a prompt through Claude, send the result to a Telegram chat.
    ClaudePrompt { prompt: String, chat_id: i64 },
    /// Send a static message to a Telegram chat.
    TelegramMessage { chat_id: i64, text: String },
    /// Send a static message to the admin.
    TelegramAdmin { text: String },
}

/// Persisted scheduler state.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SchedulerState {
    pub jobs: Vec<JobRecord>,
}

// ---- Scheduler implementation ----

pub struct Scheduler {
    sched: JobScheduler,
    jobs: RwLock<HashMap<Uuid, JobRecord>>,
    state_path: PathBuf,
    queue: Arc<InvocationQueue>,
    bot: Arc<RwLock<teloxide::Bot>>,
    config: Arc<DaemonConfig>,
    status: Arc<StatusTracker>,
}

impl Scheduler {
    pub async fn new(
        state_path: PathBuf,
        queue: Arc<InvocationQueue>,
        bot: Arc<RwLock<teloxide::Bot>>,
        config: Arc<DaemonConfig>,
        status: Arc<StatusTracker>,
    ) -> Result<Arc<Self>> {
        let sched = JobScheduler::new()
            .await
            .context("Failed to create job scheduler")?;

        let scheduler = Arc::new(Self {
            sched,
            jobs: RwLock::new(HashMap::new()),
            state_path,
            queue,
            bot,
            config,
            status,
        });

        scheduler.load_state().await?;

        let job_count = scheduler.jobs.read().await.len();
        scheduler.status.update_scheduler_jobs(job_count);

        scheduler
            .sched
            .start()
            .await
            .context("Failed to start scheduler")?;

        Ok(scheduler)
    }

    /// Create the `Job::new_async` closure for a recurring cron job.
    fn make_cron_job(self: &Arc<Self>, cron: &str, action: JobAction, name: String) -> Result<Job> {
        let weak = Arc::downgrade(self);
        Job::new_async(cron, move |_uuid, _lock| {
            let action = action.clone();
            let name = name.clone();
            let weak = weak.clone();
            Box::pin(async move {
                if let Some(this) = weak.upgrade() {
                    tracing::info!(job = %name, "Cron job fired");
                    this.dispatch(&action, &name).await;
                }
            })
        })
        .with_context(|| format!("Invalid cron expression: {cron}"))
    }

    pub async fn add_job(
        self: &Arc<Self>,
        name: String,
        cron: String,
        action: JobAction,
        one_shot: bool,
    ) -> Result<Uuid> {
        const MAX_JOBS: usize = 100;

        let stable_id = Uuid::new_v4();
        let job = if one_shot {
            let action_clone = action.clone();
            let name_clone = name.clone();
            let weak = Arc::downgrade(self);
            let one_shot_stable_id = stable_id;
            // Duration::from_secs(0) fires immediately — one-shot jobs are only
            // used internally (not via /schedule) for immediate-dispatch tasks.
            Job::new_one_shot_async(std::time::Duration::from_secs(0), move |_uuid, _lock| {
                let action = action_clone.clone();
                let name = name_clone.clone();
                let weak = weak.clone();
                Box::pin(async move {
                    if let Some(this) = weak.upgrade() {
                        tracing::info!(job = %name, "One-shot job fired");
                        this.dispatch(&action, &name).await;
                        let job_count = {
                            let mut jobs = this.jobs.write().await;
                            jobs.remove(&one_shot_stable_id);
                            jobs.len()
                        };
                        this.status.update_scheduler_jobs(job_count);
                    }
                })
            })
            .context("Failed to create one-shot job")?
        } else {
            self.make_cron_job(&cron, action.clone(), name.clone())?
        };

        let record = JobRecord {
            id: None,
            stable_id,
            name,
            cron_expression: cron,
            action,
            one_shot,
        };

        let mut jobs = self.jobs.write().await;
        if !record.one_shot {
            if jobs.values().any(|j| j.name == record.name) {
                anyhow::bail!(
                    "A job named '{}' already exists. Remove it first with /unschedule.",
                    record.name
                );
            }
            let recurring_count = jobs.values().filter(|j| !j.one_shot).count();
            if recurring_count >= MAX_JOBS {
                anyhow::bail!(
                    "Job limit reached ({MAX_JOBS}). Remove existing jobs with /unschedule before adding more."
                );
            }
        }
        jobs.insert(stable_id, record);

        let job_id = match self.sched.add(job).await {
            Ok(id) => id,
            Err(e) => {
                // Roll back reservation so in-memory state matches scheduler state.
                jobs.remove(&stable_id);
                self.status.update_scheduler_jobs(jobs.len());
                return Err(anyhow::Error::new(e).context("Failed to add job to scheduler"));
            }
        };

        if let Some(record) = jobs.get_mut(&stable_id) {
            record.id = Some(job_id);
        }
        let job_count = jobs.len();
        drop(jobs);

        self.status.update_scheduler_jobs(job_count);
        self.persist_state().await?;
        Ok(stable_id)
    }

    /// Remove a job by its stable user-facing ID.
    pub async fn remove_job(&self, stable_id: Uuid) -> Result<()> {
        // Look up and remove from the in-memory map under a single write lock.
        // A separate read-lock lookup followed by a write-lock removal creates a
        // TOCTOU race: two concurrent remove_job calls could both see the job,
        // then both try to remove it from the scheduler (the second erroring).
        let (internal_id, job_count) = {
            let mut jobs = self.jobs.write().await;
            let internal_id = jobs
                .get(&stable_id)
                .map(|r| r.id)
                .ok_or_else(|| anyhow::anyhow!("Job not found"))?;
            let internal_id = internal_id.ok_or_else(|| {
                anyhow::anyhow!(
                    "Job is still initializing; try again in a moment"
                )
            })?;
            jobs.remove(&stable_id);
            (internal_id, jobs.len())
        };

        self.sched
            .remove(&internal_id)
            .await
            .context("Failed to remove job from scheduler")?;
        self.status.update_scheduler_jobs(job_count);
        self.persist_state().await
    }

    pub async fn list_jobs(&self) -> Vec<JobRecord> {
        let mut jobs: Vec<JobRecord> = self.jobs.read().await.values().cloned().collect();
        jobs.sort_by(|a, b| a.name.cmp(&b.name));
        jobs
    }

    /// Stop the scheduler from firing new cron jobs.
    ///
    /// `JobScheduler::shutdown` takes `self` by value, but `JobScheduler` uses
    /// internal `Arc` so `clone()` shares the same underlying scheduler.
    pub async fn shutdown(&self) {
        if let Err(e) = self.sched.clone().shutdown().await {
            tracing::error!(error = %e, "Failed to shut down scheduler");
        } else {
            tracing::info!("Scheduler shut down");
        }
    }

    async fn dispatch(&self, action: &JobAction, job_name: &str) {
        let bot = self.bot.read().await.clone();
        match action {
            JobAction::ClaudePrompt { prompt, chat_id } => {
                if !self.config.is_chat_allowed(*chat_id) {
                    tracing::warn!(
                        job = %job_name,
                        chat_id,
                        "Skipping scheduled job — chat_id no longer authorized"
                    );
                    return;
                }
                let origin = RequestOrigin::Scheduler {
                    job_name: job_name.to_string(),
                };
                match self.queue.submit(prompt.clone(), origin, None).await {
                    Ok(response) => {
                        let output = response.into_display_text();
                        telegram::send_chunked(&bot, *chat_id, &output).await;
                    }
                    Err(e) => {
                        tracing::error!(job = %job_name, error = %e, "Claude prompt job failed");
                    }
                }
            }
            JobAction::TelegramMessage { chat_id, text } => {
                if !self.config.is_chat_allowed(*chat_id) {
                    tracing::warn!(
                        job = %job_name,
                        chat_id,
                        "Skipping scheduled message — chat_id no longer authorized"
                    );
                    return;
                }
                let _ = telegram::send_text(&bot, *chat_id, text).await;
            }
            JobAction::TelegramAdmin { text } => {
                let _ = telegram::send_text(&bot, self.config.admin_chat_id, text).await;
            }
        }
    }

    /// Persist scheduler state atomically.
    ///
    /// Holds the read lock through I/O so concurrent `add_job`/`remove_job`
    /// calls cannot interleave between our snapshot and the disk write.
    /// Trade-off: this can block add/remove operations for the fsync duration.
    /// We accept that latency to avoid stale state writes clobbering newer
    /// scheduler updates.
    /// Only persists recurring jobs — one-shot jobs are ephemeral.
    async fn persist_state(&self) -> Result<()> {
        let jobs = self.jobs.read().await;
        let state = SchedulerState {
            jobs: jobs.values().filter(|j| !j.one_shot).cloned().collect(),
        };

        if let Some(parent) = self.state_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }
        let json = serde_json::to_string(&state).context("Failed to serialize scheduler state")?;
        atomic_write(&self.state_path, json.as_bytes()).await
        // read lock released here, after I/O completes
    }

    async fn load_state(self: &Arc<Self>) -> Result<()> {
        let contents = match tokio::fs::read_to_string(&self.state_path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!(
                    "Failed to read state from {}",
                    self.state_path.display()
                )));
            }
        };

        let state: SchedulerState = match serde_json::from_str(&contents) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %self.state_path.display(),
                    "Corrupted scheduler state, starting with no jobs"
                );
                return Ok(());
            }
        };

        let mut restored_jobs = Vec::new();
        for record in state.jobs {
            if record.one_shot {
                continue;
            }
            match &record.action {
                JobAction::ClaudePrompt { chat_id, .. }
                | JobAction::TelegramMessage { chat_id, .. } => {
                    if !self.config.is_chat_allowed(*chat_id) {
                        tracing::warn!(
                            job = %record.name,
                            chat_id,
                            "Restoring job for currently unauthorized chat; it will be skipped at dispatch"
                        );
                    }
                }
                JobAction::TelegramAdmin { .. } => {}
            }

            let job = match self.make_cron_job(
                &record.cron_expression,
                record.action.clone(),
                record.name.clone(),
            ) {
                Ok(j) => j,
                Err(e) => {
                    tracing::warn!(
                        job = %record.name,
                        cron = %record.cron_expression,
                        error = %e,
                        "Skipping job with invalid cron expression"
                    );
                    continue;
                }
            };

            let internal_id = match self.sched.add(job).await {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        job = %record.name,
                        error = %e,
                        "Failed to re-add restored job, skipping"
                    );
                    continue;
                }
            };

            restored_jobs.push((
                record.stable_id,
                JobRecord {
                    id: Some(internal_id),
                    stable_id: record.stable_id,
                    name: record.name,
                    cron_expression: record.cron_expression,
                    action: record.action,
                    one_shot: false,
                },
            ));
        }

        if !restored_jobs.is_empty() {
            let mut jobs = self.jobs.write().await;
            for (stable_id, record) in restored_jobs {
                jobs.insert(stable_id, record);
            }
        }

        Ok(())
    }
}
