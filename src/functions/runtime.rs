use crate::services::{
    AgentExecutor, AgentRunnerService, ExecutionInput, ExecutionOutcome, OpenRouterAgentRunner,
    RunnerEvent, RunnerHandle, RunnerStartInput,
};
use forge::prelude::*;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

struct PendingJob {
    id: Uuid,
    chat_id: String,
    enriched_prompt: Option<String>,
    prompt: Option<String>,
    resume_input: Option<String>,
    trace_id: Option<Uuid>,
}

fn trace_id_or_new(trace_id: Option<Uuid>) -> Uuid {
    trace_id.unwrap_or_else(Uuid::new_v4)
}

struct JobContext {
    chat_id: String,
    trace_id: Uuid,
}

async fn fetch_job_context(db: &PgPool, job_id: Uuid) -> Result<Option<JobContext>> {
    let row = sqlx::query!("SELECT chat_id, trace_id FROM jobs WHERE id = $1", job_id)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| JobContext {
        chat_id: r.chat_id,
        trace_id: trace_id_or_new(r.trace_id),
    }))
}

async fn insert_runtime_event(
    db: &PgPool,
    trace_id: Uuid,
    action: &str,
    payload: serde_json::Value,
) -> Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO events (trace_id, source, action, payload)
        VALUES ($1, 'runtime', $2, $3)
        "#,
        trace_id,
        action,
        payload
    )
    .execute(db)
    .await?;
    Ok(())
}

async fn insert_outbox_text(
    db: &PgPool,
    chat_id: &str,
    text: &str,
    job_id: Uuid,
    trace_id: Uuid,
) -> Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO outbox (chat_id, content, job_id, trace_id)
        VALUES ($1, $2, $3, $4)
        "#,
        chat_id,
        text,
        job_id,
        trace_id
    )
    .execute(db)
    .await?;
    Ok(())
}

async fn insert_outbox_with_attachments(
    db: &PgPool,
    chat_id: &str,
    text: &str,
    attachments: Vec<serde_json::Value>,
    job_id: Uuid,
    trace_id: Uuid,
) -> Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO outbox (chat_id, content, attachments, job_id, trace_id)
        VALUES ($1, $2, $3, $4, $5)
        "#,
        chat_id,
        text,
        serde_json::Value::Array(attachments),
        job_id,
        trace_id
    )
    .execute(db)
    .await?;
    Ok(())
}

async fn handle_runner_event(db: &PgPool, job_id: Uuid, event: RunnerEvent) -> Result<bool> {
    match event {
        RunnerEvent::Stdout(line) => {
            sqlx::query!(
                "INSERT INTO logs (job_id, stream, line) VALUES ($1, 'stdout', $2)",
                job_id,
                line
            )
            .execute(db)
            .await?;
            Ok(false)
        }
        RunnerEvent::Stderr(line) => {
            sqlx::query!(
                "INSERT INTO logs (job_id, stream, line) VALUES ($1, 'stderr', $2)",
                job_id,
                line
            )
            .execute(db)
            .await?;
            Ok(false)
        }
        RunnerEvent::AskUser { question } => {
            tracing::info!(job_id = %job_id, "runtime: job asking user for input");
            if let Some(ctx) = fetch_job_context(db, job_id).await? {
                sqlx::query!(
                    r#"
                    UPDATE jobs SET status = 'paused', question_pending = $2
                    WHERE id = $1 AND status = 'running'
                    "#,
                    job_id,
                    question
                )
                .execute(db)
                .await?;

                insert_outbox_text(
                    db,
                    &ctx.chat_id,
                    &format!("question: {question}"),
                    job_id,
                    ctx.trace_id,
                )
                .await?;

                insert_runtime_event(
                    db,
                    ctx.trace_id,
                    "job_paused",
                    serde_json::json!({ "job_id": job_id, "question": question }),
                )
                .await?;
            }
            Ok(true)
        }
        RunnerEvent::Completed {
            output,
            attachments,
        } => {
            tracing::info!(
                job_id = %job_id,
                output_len = output.len(),
                attachment_count = attachments.len(),
                "runtime: job completed"
            );

            sqlx::query!(
                r#"
                UPDATE jobs SET status = 'done', output = $2, finished_at = now()
                WHERE id = $1 AND status = 'running'
                "#,
                job_id,
                output
            )
            .execute(db)
            .await?;

            if let Some(ctx) = fetch_job_context(db, job_id).await? {
                insert_outbox_with_attachments(
                    db,
                    &ctx.chat_id,
                    &output,
                    attachments,
                    job_id,
                    ctx.trace_id,
                )
                .await?;

                insert_runtime_event(
                    db,
                    ctx.trace_id,
                    "job_completed",
                    serde_json::json!({ "job_id": job_id }),
                )
                .await?;
            }
            Ok(true)
        }
        RunnerEvent::Failed { error } => {
            tracing::error!(job_id = %job_id, error = %error, "runtime: job failed");

            sqlx::query!(
                r#"
                UPDATE jobs SET status = 'failed', error = $2, finished_at = now()
                WHERE id = $1
                "#,
                job_id,
                error
            )
            .execute(db)
            .await?;

            if let Some(ctx) = fetch_job_context(db, job_id).await? {
                insert_outbox_text(
                    db,
                    &ctx.chat_id,
                    &format!("task failed: {error}"),
                    job_id,
                    ctx.trace_id,
                )
                .await?;

                insert_runtime_event(
                    db,
                    ctx.trace_id,
                    "job_failed",
                    serde_json::json!({ "job_id": job_id, "error": error }),
                )
                .await?;
            }
            Ok(true)
        }
    }
}

async fn start_pending_jobs(
    db: &PgPool,
    runner: &dyn AgentRunnerService,
    active_runs: &mut HashMap<Uuid, RunnerHandle>,
) -> Result<()> {
    let pending = sqlx::query_as!(
        PendingJob,
        r#"
        SELECT id, chat_id, enriched_prompt, prompt, resume_input, trace_id
        FROM jobs
        WHERE status = 'pending'
          AND id != ALL($1::uuid[])
        ORDER BY created_at
        LIMIT 10
        FOR UPDATE SKIP LOCKED
        "#,
        &active_runs.keys().copied().collect::<Vec<_>>()
    )
    .fetch_all(db)
    .await?;

    if !pending.is_empty() {
        tracing::debug!(count = pending.len(), "runtime: starting pending jobs");
    }

    for job in &pending {
        let prompt = job
            .enriched_prompt
            .clone()
            .or_else(|| job.prompt.clone())
            .unwrap_or_default();

        let is_resume = job.resume_input.is_some();
        let full_prompt = if let Some(ref input) = job.resume_input {
            format!("{prompt}\n\nUser response: {input}")
        } else {
            prompt
        };

        tracing::info!(
            job_id = %job.id,
            chat_id = %job.chat_id,
            prompt_len = full_prompt.len(),
            is_resume,
            "runtime: launching job"
        );

        match runner
            .start(RunnerStartInput {
                job_id: job.id,
                prompt: full_prompt,
            })
            .await
        {
            Ok(handle) => {
                let trace_id = trace_id_or_new(job.trace_id);
                sqlx::query!(
                    r#"
                    UPDATE jobs SET status = 'running', started_at = now(), last_heartbeat_at = now()
                    WHERE id = $1 AND status = 'pending'
                    "#,
                    job.id
                )
                .execute(db)
                .await?;

                sqlx::query!(
                    r#"
                    INSERT INTO events (trace_id, source, action, payload)
                    VALUES ($1, 'runtime', 'job_started', $2)
                    "#,
                    trace_id,
                    serde_json::json!({ "job_id": job.id })
                )
                .execute(db)
                .await?;

                active_runs.insert(job.id, handle);
            }
            Err(e) => {
                tracing::error!(job_id = %job.id, error = %e, "failed to start job");
            }
        }
    }
    Ok(())
}

async fn poll_active_runs(
    db: &PgPool,
    runner: &dyn AgentRunnerService,
    active_runs: &mut HashMap<Uuid, RunnerHandle>,
) -> Result<()> {
    let run_ids: Vec<Uuid> = active_runs.keys().copied().collect();
    for job_id in run_ids {
        let handle = match active_runs.get(&job_id) {
            Some(h) => h,
            None => continue,
        };

        let events = match runner.poll(handle).await {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(job_id = %job_id, error = %e, "poll failed");
                continue;
            }
        };

        sqlx::query!(
            "UPDATE jobs SET last_heartbeat_at = now() WHERE id = $1",
            job_id
        )
        .execute(db)
        .await?;

        for event in events {
            if handle_runner_event(db, job_id, event).await? {
                active_runs.remove(&job_id);
            }
        }
    }
    Ok(())
}

async fn cleanup_cancelled_runs(
    db: &PgPool,
    runner: &dyn AgentRunnerService,
    active_runs: &mut HashMap<Uuid, RunnerHandle>,
) -> Result<()> {
    let cancelled = sqlx::query_scalar!(
        r#"
        SELECT id FROM jobs
        WHERE status = 'cancelled' AND id = ANY($1::uuid[])
        "#,
        &active_runs.keys().copied().collect::<Vec<_>>()
    )
    .fetch_all(db)
    .await?;

    for job_id in cancelled {
        if let Some(handle) = active_runs.remove(&job_id) {
            let _ = runner.cancel(&handle).await;
        }
    }
    Ok(())
}

async fn recover_orphaned_jobs(db: &PgPool) -> Result<()> {
    let orphaned = sqlx::query_scalar!(
        r#"
        SELECT id FROM jobs
        WHERE status = 'running'
          AND last_heartbeat_at < now() - interval '5 minutes'
        LIMIT 10
        FOR UPDATE SKIP LOCKED
        "#
    )
    .fetch_all(db)
    .await?;

    for job_id in orphaned {
        tracing::warn!(job_id = %job_id, "recovering orphaned running job");
        sqlx::query!(
            r#"
            UPDATE jobs SET status = 'pending', last_heartbeat_at = NULL
            WHERE id = $1 AND status = 'running'
            "#,
            job_id
        )
        .execute(db)
        .await?;

        sqlx::query!(
            r#"
            INSERT INTO events (source, action, payload)
            VALUES ('runtime', 'orphan_recovered', $1)
            "#,
            serde_json::json!({ "job_id": job_id })
        )
        .execute(db)
        .await?;
    }
    Ok(())
}

pub async fn runtime_tick(
    db: &PgPool,
    runner: &dyn AgentRunnerService,
    active_runs: &mut HashMap<Uuid, RunnerHandle>,
) -> Result<()> {
    start_pending_jobs(db, runner, active_runs).await?;
    poll_active_runs(db, runner, active_runs).await?;
    cleanup_cancelled_runs(db, runner, active_runs).await?;
    recover_orphaned_jobs(db).await?;
    Ok(())
}

#[forge::daemon]
pub async fn runtime(ctx: &DaemonContext) -> Result<()> {
    let runtime_enabled = ctx
        .env_parse::<String>("YUI_RUNTIME_ENABLED")
        .map(|v| v != "false")
        .unwrap_or(true);

    let backend = std::env::var("YUI_RUNTIME_BACKEND").unwrap_or_default();
    let runner: Arc<dyn AgentRunnerService> = match backend.as_str() {
        "docker" if std::env::var("YUI_DOCKER_IMAGE").is_ok() => {
            tracing::info!("runtime using Docker agent executor");
            Arc::new(DockerAgentRunner::new())
        }
        _ if runtime_enabled && std::env::var("OPENROUTER_API_KEY").is_ok() => {
            tracing::info!("runtime using OpenRouter agent runner");
            Arc::new(OpenRouterAgentRunner::from_env())
        }
        _ if runtime_enabled && std::env::var("YUI_DOCKER_IMAGE").is_ok() => {
            tracing::info!("runtime using Docker agent executor");
            Arc::new(DockerAgentRunner::new())
        }
        _ => {
            tracing::warn!("no runtime backend configured, runtime daemon idle");
            return Ok(());
        }
    };

    let poll_ms: u64 = ctx.env_parse("YUI_LOOP_POLL_MS_RUNTIME").unwrap_or(500);
    let mut active_runs: HashMap<Uuid, RunnerHandle> = HashMap::new();

    loop {
        tokio::select! {
            _ = ctx.shutdown_signal() => break,
            _ = tokio::time::sleep(std::time::Duration::from_millis(poll_ms)) => {
                if let Err(e) = runtime_tick(ctx.db(), runner.as_ref(), &mut active_runs).await {
                    tracing::error!(error = %e, "runtime tick failed");
                }
            }
        }
    }
    Ok(())
}

struct DockerAgentRunner;

impl DockerAgentRunner {
    fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl AgentRunnerService for DockerAgentRunner {
    async fn start(&self, input: RunnerStartInput) -> anyhow::Result<RunnerHandle> {
        // the actual execution happens asynchronously, we return a handle immediately
        let handle = RunnerHandle {
            run_id: Uuid::new_v4(),
            job_id: input.job_id,
        };
        // store the prompt for when poll is called, execution is lazy
        DOCKER_RUNS
            .lock()
            .unwrap()
            .insert(handle.run_id, DockerRun::Pending(input.prompt));
        Ok(handle)
    }

    async fn poll(&self, handle: &RunnerHandle) -> anyhow::Result<Vec<RunnerEvent>> {
        let state = {
            let runs = DOCKER_RUNS.lock().unwrap();
            runs.get(&handle.run_id).cloned()
        };

        match state {
            Some(DockerRun::Pending(prompt)) => {
                // start execution
                {
                    let mut runs = DOCKER_RUNS.lock().unwrap();
                    runs.insert(handle.run_id, DockerRun::Running);
                }

                let (log_tx, mut log_rx) =
                    tokio::sync::mpsc::unbounded_channel::<(String, String)>();
                let executor_input = ExecutionInput {
                    job_id: handle.job_id,
                    trace_id: Uuid::new_v4(),
                    prompt: prompt.clone(),
                    attachments: vec![],
                    session_id: None,
                    resume_input: None,
                };

                let run_id = handle.run_id;
                let executor = AgentExecutor::from_env();
                tokio::spawn(async move {
                    let outcome = executor.execute(executor_input, log_tx).await;
                    let mut runs = DOCKER_RUNS.lock().unwrap();
                    runs.insert(run_id, DockerRun::Done(outcome));
                });

                // collect any immediate log output
                let mut events = vec![RunnerEvent::Stdout("starting container...".to_string())];
                while let Ok((stream, line)) = log_rx.try_recv() {
                    match stream.as_str() {
                        "stderr" => events.push(RunnerEvent::Stderr(line)),
                        _ => events.push(RunnerEvent::Stdout(line)),
                    }
                }
                Ok(events)
            }
            Some(DockerRun::Running) => {
                // still running
                Ok(vec![])
            }
            Some(DockerRun::Done(outcome)) => {
                let mut runs = DOCKER_RUNS.lock().unwrap();
                runs.remove(&handle.run_id);
                drop(runs);

                match outcome {
                    ExecutionOutcome::Completed {
                        output,
                        attachments,
                        ..
                    } => Ok(vec![RunnerEvent::Completed {
                        output,
                        attachments,
                    }]),
                    ExecutionOutcome::Paused { question, .. } => {
                        Ok(vec![RunnerEvent::AskUser { question }])
                    }
                    ExecutionOutcome::Failed { error, .. } => {
                        Ok(vec![RunnerEvent::Failed { error }])
                    }
                }
            }
            None => Ok(vec![]),
        }
    }

    async fn cancel(&self, handle: &RunnerHandle) -> anyhow::Result<()> {
        let container_name = format!("yui-job-{}", handle.job_id.as_simple());
        let _ = tokio::process::Command::new("docker")
            .args(["kill", &container_name])
            .output()
            .await;
        let mut runs = DOCKER_RUNS.lock().unwrap();
        runs.remove(&handle.run_id);
        Ok(())
    }
}

#[derive(Clone)]
enum DockerRun {
    Pending(String),
    Running,
    Done(ExecutionOutcome),
}

static DOCKER_RUNS: std::sync::LazyLock<std::sync::Mutex<HashMap<Uuid, DockerRun>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

