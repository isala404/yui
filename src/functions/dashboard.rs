use crate::schema::*;
use forge::prelude::*;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
pub struct ListEventsInput {
    pub trace_id: Option<Uuid>,
    pub limit: Option<i64>,
}

#[forge::query(public)]
pub async fn list_events(ctx: &QueryContext, input: ListEventsInput) -> Result<Vec<Event>> {
    let limit = input.limit.unwrap_or(100).min(500);

    if let Some(trace_id) = input.trace_id {
        sqlx::query_as!(
            Event,
            r#"
            SELECT id, trace_id, source, action, payload, created_at
            FROM events
            WHERE trace_id = $1
            ORDER BY created_at, id
            LIMIT $2
            "#,
            trace_id,
            limit
        )
        .fetch_all(ctx.db())
        .await
        .map_err(|e| ForgeError::Database(e.to_string()))
    } else {
        sqlx::query_as!(
            Event,
            r#"
            SELECT id, trace_id, source, action, payload, created_at
            FROM events
            ORDER BY created_at DESC, id DESC
            LIMIT $1
            "#,
            limit
        )
        .fetch_all(ctx.db())
        .await
        .map_err(|e| ForgeError::Database(e.to_string()))
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListJobsInput {
    pub status: Option<String>,
    pub limit: Option<i64>,
}

#[forge::query(public)]
pub async fn list_jobs(ctx: &QueryContext, input: ListJobsInput) -> Result<Vec<Job>> {
    let limit = input.limit.unwrap_or(50).min(200);

    if let Some(ref status) = input.status {
        sqlx::query_as!(
            Job,
            r#"
            SELECT id, kind as "kind: JobKind", chat_id, status as "status: JobStatus",
                   prompt, enriched_prompt, source_ids as "source_ids!", resume_input, output, error,
                   cancel_reason, forge_job_id, session_id, container_id, last_heartbeat_at,
                   question_pending, started_at, finished_at,
                   trace_id, created_at, updated_at
            FROM jobs
            WHERE status = $1
            ORDER BY created_at DESC, id DESC
            LIMIT $2
            "#,
            status,
            limit
        )
        .fetch_all(ctx.db())
        .await
        .map_err(|e| ForgeError::Database(e.to_string()))
    } else {
        sqlx::query_as!(
            Job,
            r#"
            SELECT id, kind as "kind: JobKind", chat_id, status as "status: JobStatus",
                   prompt, enriched_prompt, source_ids as "source_ids!", resume_input, output, error,
                   cancel_reason, forge_job_id, session_id, container_id, last_heartbeat_at,
                   question_pending, started_at, finished_at,
                   trace_id, created_at, updated_at
            FROM jobs
            ORDER BY created_at DESC, id DESC
            LIMIT $1
            "#,
            limit
        )
        .fetch_all(ctx.db())
        .await
        .map_err(|e| ForgeError::Database(e.to_string()))
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListOutboxInput {
    pub pending_only: Option<bool>,
    pub limit: Option<i64>,
}

#[forge::query(public)]
pub async fn list_outbox(ctx: &QueryContext, input: ListOutboxInput) -> Result<Vec<Outbox>> {
    let limit = input.limit.unwrap_or(50).min(200);

    if input.pending_only.unwrap_or(false) {
        sqlx::query_as!(
            Outbox,
            r#"
            SELECT id, chat_id, content, attachments, reply_to, processed_at,
                   attempt_count, last_error, job_id, reply_to_message_id,
                   rewritten_at, trace_id, created_at, updated_at
            FROM outbox
            WHERE processed_at IS NULL
            ORDER BY created_at, id
            LIMIT $1
            "#,
            limit
        )
        .fetch_all(ctx.db())
        .await
        .map_err(|e| ForgeError::Database(e.to_string()))
    } else {
        sqlx::query_as!(
            Outbox,
            r#"
            SELECT id, chat_id, content, attachments, reply_to, processed_at,
                   attempt_count, last_error, job_id, reply_to_message_id,
                   rewritten_at, trace_id, created_at, updated_at
            FROM outbox
            ORDER BY created_at DESC, id DESC
            LIMIT $1
            "#,
            limit
        )
        .fetch_all(ctx.db())
        .await
        .map_err(|e| ForgeError::Database(e.to_string()))
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListCronsInput {
    pub limit: Option<i64>,
}

#[forge::query(public)]
pub async fn list_crons(ctx: &QueryContext, input: ListCronsInput) -> Result<Vec<Cron>> {
    let limit = input.limit.unwrap_or(50).min(200);

    sqlx::query_as!(
        Cron,
        r#"
        SELECT id, name, schedule, timezone, chat_id, prompt, enabled,
               last_run_at, next_run_at, last_job_id, created_at, updated_at
        FROM crons
        ORDER BY created_at DESC, id DESC
        LIMIT $1
        "#,
        limit
    )
    .fetch_all(ctx.db())
    .await
    .map_err(|e| ForgeError::Database(e.to_string()))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListMessagesInput {
    pub chat_id: Option<String>,
    pub limit: Option<i64>,
}

#[forge::query(public)]
pub async fn list_messages(ctx: &QueryContext, input: ListMessagesInput) -> Result<Vec<Message>> {
    let limit = input.limit.unwrap_or(50).min(200);

    if let Some(ref chat_id) = input.chat_id {
        sqlx::query_as!(
            Message,
            r#"
            SELECT id, platform_id, platform_chat_id, platform_sender_id,
                   direction as "direction: Direction", content, attachments, content_version, audit_processed_version,
                   routed_at, audit_processed_at, is_deleted, reply_to_id, job_id, trace_id,
                   created_at, updated_at
            FROM messages
            WHERE platform_chat_id = $1
            ORDER BY created_at DESC, id DESC
            LIMIT $2
            "#,
            chat_id,
            limit
        )
        .fetch_all(ctx.db())
        .await
        .map_err(|e| ForgeError::Database(e.to_string()))
    } else {
        sqlx::query_as!(
            Message,
            r#"
            SELECT id, platform_id, platform_chat_id, platform_sender_id,
                   direction as "direction: Direction", content, attachments, content_version, audit_processed_version,
                   routed_at, audit_processed_at, is_deleted, reply_to_id, job_id, trace_id,
                   created_at, updated_at
            FROM messages
            ORDER BY created_at DESC, id DESC
            LIMIT $1
            "#,
            limit
        )
        .fetch_all(ctx.db())
        .await
        .map_err(|e| ForgeError::Database(e.to_string()))
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GetTraceInput {
    pub trace_id: Uuid,
}

#[derive(Debug, Serialize)]
pub struct TraceView {
    pub events: Vec<Event>,
    pub jobs: Vec<Job>,
    pub messages: Vec<Message>,
}

#[forge::query(public)]
pub async fn get_trace(ctx: &QueryContext, input: GetTraceInput) -> Result<TraceView> {
    let events = sqlx::query_as!(
        Event,
        r#"
        SELECT id, trace_id, source, action, payload, created_at
        FROM events
        WHERE trace_id = $1
        ORDER BY created_at, id
        "#,
        input.trace_id
    )
    .fetch_all(ctx.db())
    .await
    .map_err(|e| ForgeError::Database(e.to_string()))?;

    let jobs = sqlx::query_as!(
        Job,
        r#"
        SELECT id, kind as "kind: JobKind", chat_id, status as "status: JobStatus",
               prompt, enriched_prompt, source_ids as "source_ids!", resume_input, output, error,
               cancel_reason, forge_job_id, session_id, container_id, last_heartbeat_at,
               question_pending, started_at, finished_at,
               trace_id, created_at, updated_at
        FROM jobs
        WHERE trace_id = $1
        ORDER BY created_at, id
        "#,
        input.trace_id
    )
    .fetch_all(ctx.db())
    .await
    .map_err(|e| ForgeError::Database(e.to_string()))?;

    let messages = sqlx::query_as!(
        Message,
        r#"
        SELECT id, platform_id, platform_chat_id, platform_sender_id,
               direction as "direction: Direction", content, attachments, content_version, audit_processed_version,
               routed_at, audit_processed_at, is_deleted, reply_to_id, job_id, trace_id,
               created_at, updated_at
        FROM messages
        WHERE trace_id = $1
        ORDER BY created_at, id
        "#,
        input.trace_id
    )
    .fetch_all(ctx.db())
    .await
    .map_err(|e| ForgeError::Database(e.to_string()))?;

    Ok(TraceView {
        events,
        jobs,
        messages,
    })
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GetHealthInput {}

#[derive(Debug, Serialize)]
pub struct HealthView {
    pub pending_jobs: i64,
    pub running_jobs: i64,
    pub paused_jobs: i64,
    pub pending_outbox: i64,
    pub dead_letter_outbox: i64,
    pub stuck_jobs: i64,
}

#[forge::query(public)]
pub async fn get_health(ctx: &QueryContext, _input: GetHealthInput) -> Result<HealthView> {
    let jobs = sqlx::query!(
        r#"
        SELECT
            COUNT(*) FILTER (WHERE status = 'pending') as "pending!",
            COUNT(*) FILTER (WHERE status = 'running') as "running!",
            COUNT(*) FILTER (WHERE status = 'paused') as "paused!",
            COUNT(*) FILTER (WHERE status = 'running'
                AND last_heartbeat_at < now() - interval '5 minutes') as "stuck!"
        FROM jobs
        "#
    )
    .fetch_one(ctx.db())
    .await
    .map_err(|e| ForgeError::Database(e.to_string()))?;

    let outbox = sqlx::query!(
        r#"
        SELECT
            COUNT(*) FILTER (WHERE attempt_count < 5) as "pending!",
            COUNT(*) FILTER (WHERE attempt_count >= 5) as "dead_letter!"
        FROM outbox
        WHERE processed_at IS NULL
        "#
    )
    .fetch_one(ctx.db())
    .await
    .map_err(|e| ForgeError::Database(e.to_string()))?;

    Ok(HealthView {
        pending_jobs: jobs.pending,
        running_jobs: jobs.running,
        paused_jobs: jobs.paused,
        stuck_jobs: jobs.stuck,
        pending_outbox: outbox.pending,
        dead_letter_outbox: outbox.dead_letter,
    })
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CancelJobInput {
    pub job_id: Uuid,
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CancelJobOutput {
    pub cancelled: bool,
}

#[forge::mutation(public)]
pub async fn cancel_job(ctx: &MutationContext, input: CancelJobInput) -> Result<CancelJobOutput> {
    let reason = input
        .reason
        .unwrap_or_else(|| "cancelled via dashboard".into());
    let db = ctx.db();

    let result = db
        .execute(sqlx::query!(
            r#"
            UPDATE jobs SET status = 'cancelled', cancel_reason = $2, finished_at = now()
            WHERE id = $1 AND status IN ('draft', 'pending', 'running', 'paused')
            "#,
            input.job_id,
            reason
        ))
        .await?;

    if result.rows_affected() > 0 {
        db.execute(sqlx::query!(
            r#"
            INSERT INTO events (source, action, payload)
            VALUES ('dashboard', 'job_cancelled', $1)
            "#,
            serde_json::json!({ "job_id": input.job_id, "reason": reason })
        ))
        .await?;
    }

    Ok(CancelJobOutput {
        cancelled: result.rows_affected() > 0,
    })
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ToggleCronInput {
    pub cron_id: Uuid,
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
pub struct ToggleCronOutput {
    pub updated: bool,
}

#[forge::mutation(public)]
pub async fn toggle_cron(
    ctx: &MutationContext,
    input: ToggleCronInput,
) -> Result<ToggleCronOutput> {
    let db = ctx.db();

    let result = db
        .execute(sqlx::query!(
            "UPDATE crons SET enabled = $2 WHERE id = $1",
            input.cron_id,
            input.enabled
        ))
        .await?;

    if result.rows_affected() > 0 {
        db.execute(sqlx::query!(
            r#"
            INSERT INTO events (source, action, payload)
            VALUES ('dashboard', 'cron_toggled', $1)
            "#,
            serde_json::json!({ "cron_id": input.cron_id, "enabled": input.enabled })
        ))
        .await?;
    }

    Ok(ToggleCronOutput {
        updated: result.rows_affected() > 0,
    })
}
