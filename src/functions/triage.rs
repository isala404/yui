use crate::functions::clock::compute_next_run_at;
use crate::services::{
    ActiveCronSummary, ActiveJobSummary, AiService, TriageBatchInput, TriageDecision, TriageMessage,
};
use forge::prelude::*;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

struct UnroutedMessage {
    id: Uuid,
    platform_chat_id: String,
    content: Option<String>,
    attachments: serde_json::Value,
    trace_id: Option<Uuid>,
    updated_at: chrono::DateTime<chrono::Utc>,
    created_at: chrono::DateTime<chrono::Utc>,
}

const AUDIO_ONLY_JOB_PROMPT: &str = "The user sent a voice note without clear text. Transcribe the attached audio and answer the request directly in one concise message. If they ask for the current time, include the current UTC time.";

fn attachment_has_type(raw: &serde_json::Value, target: &str) -> bool {
    raw.as_array().is_some_and(|attachments| {
        attachments.iter().any(|att| {
            att.get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case(target))
        })
    })
}

fn message_has_audio_attachment(raw: &serde_json::Value) -> bool {
    attachment_has_type(raw, "audio")
}

fn message_has_image_attachment(raw: &serde_json::Value) -> bool {
    attachment_has_type(raw, "image")
}

fn is_trivial_chat_text(content: Option<&str>) -> bool {
    let Some(content) = content else {
        return true;
    };

    let lower = content.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return true;
    }

    matches!(
        lower.as_str(),
        "hi" | "hello"
            | "hey"
            | "hey there"
            | "yo"
            | "hiya"
            | "sup"
            | "good morning"
            | "good afternoon"
            | "good evening"
    )
}

fn should_force_audio_transcription_job(
    msgs: &[&UnroutedMessage],
    decisions: &[TriageDecision],
) -> bool {
    let has_audio = msgs
        .iter()
        .any(|m| message_has_audio_attachment(&m.attachments));
    if !has_audio {
        return false;
    }

    let has_non_trivial_text = msgs
        .iter()
        .any(|m| !is_trivial_chat_text(m.content.as_deref()));
    if has_non_trivial_text {
        return false;
    }

    decisions
        .iter()
        .all(|d| matches!(d, TriageDecision::Reply { .. } | TriageDecision::Noop))
}

async fn is_chat_subscribed(db: &PgPool, chat_id: &str) -> Result<bool> {
    let enabled =
        sqlx::query_scalar::<_, bool>("SELECT enabled FROM chat_subscriptions WHERE chat_id = $1")
            .bind(chat_id)
            .fetch_optional(db)
            .await?;
    Ok(enabled.unwrap_or(true))
}

async fn queue_reply(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    chat_id: &str,
    text: &str,
    trace_id: Uuid,
) -> Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO outbox (chat_id, content, trace_id)
        VALUES ($1, $2, $3)
        "#,
        chat_id,
        text,
        trace_id
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn resolve_target_chat_id(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    fallback_chat_id: &str,
    source_ids: &[Uuid],
) -> Result<String> {
    if source_ids.is_empty() {
        return Ok(fallback_chat_id.to_string());
    }

    let resolved = sqlx::query_scalar::<_, String>(
        r#"
        SELECT platform_chat_id
        FROM messages
        WHERE id = ANY($1)
          AND direction = 'in'
        ORDER BY created_at DESC
        LIMIT 1
        "#,
    )
    .bind(source_ids)
    .fetch_optional(&mut **tx)
    .await?;

    Ok(resolved.unwrap_or_else(|| fallback_chat_id.to_string()))
}

async fn apply_decisions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    chat_id: &str,
    decisions: Vec<TriageDecision>,
    source_ids: &[Uuid],
    trace_id: Uuid,
    is_subscribed: &mut bool,
) -> Result<()> {
    let target_chat_id = resolve_target_chat_id(tx, chat_id, source_ids).await?;

    for decision in decisions {
        match decision {
            TriageDecision::Reply { text } => {
                queue_reply(tx, &target_chat_id, &text, trace_id).await?;
            }
            TriageDecision::CreateJob { prompt, kind } => {
                if !*is_subscribed {
                    queue_reply(
                        tx,
                        &target_chat_id,
                        "you're currently unsubscribed, so tasks are paused. let me know if you want to re-enable them",
                        trace_id,
                    )
                    .await?;
                    continue;
                }

                let job_id = Uuid::new_v4();
                sqlx::query!(
                    r#"
                    INSERT INTO jobs (id, kind, chat_id, status, prompt, source_ids, trace_id)
                    VALUES ($1, $2, $3, 'draft', $4, $5, $6)
                    "#,
                    job_id,
                    kind,
                    target_chat_id,
                    prompt,
                    source_ids,
                    trace_id
                )
                .execute(&mut **tx)
                .await?;

                sqlx::query!(
                    r#"
                    INSERT INTO events (trace_id, source, action, payload)
                    VALUES ($1, 'triage', 'job_created', $2)
                    "#,
                    trace_id,
                    serde_json::json!({ "job_id": job_id, "kind": kind, "chat_id": target_chat_id })
                )
                .execute(&mut **tx)
                .await?;
            }
            TriageDecision::CreateCron {
                name,
                schedule,
                prompt,
            } => {
                let timezone = "UTC";
                let next_run_at = match compute_next_run_at(&schedule, timezone, chrono::Utc::now())
                {
                    Ok(next) => next,
                    Err(err) => {
                        queue_reply(
                            tx,
                            &target_chat_id,
                            &format!("invalid schedule `{schedule}`: {err}"),
                            trace_id,
                        )
                        .await?;
                        continue;
                    }
                };

                sqlx::query!(
                    r#"
                    INSERT INTO crons (name, schedule, timezone, chat_id, prompt, next_run_at)
                    VALUES ($1, $2, $3, $4, $5, $6)
                    "#,
                    name,
                    schedule,
                    timezone,
                    target_chat_id,
                    prompt,
                    next_run_at
                )
                .execute(&mut **tx)
                .await?;

                queue_reply(
                    tx,
                    &target_chat_id,
                    &format!("scheduled `{name}` ({schedule})"),
                    trace_id,
                )
                .await?;
            }
            TriageDecision::CancelCron { name } => {
                let deleted = sqlx::query_scalar::<_, String>(
                    r#"
                    DELETE FROM crons
                    WHERE name = $1 AND chat_id = $2
                    RETURNING name
                    "#,
                )
                .bind(&name)
                .bind(&target_chat_id)
                .fetch_optional(&mut **tx)
                .await?;

                let reply = match deleted {
                    Some(n) => format!("cancelled cron: {n}"),
                    None => format!("no cron named `{name}` found"),
                };
                queue_reply(tx, &target_chat_id, &reply, trace_id).await?;
            }
            TriageDecision::CancelJob { job_id, reason } => {
                sqlx::query!(
                    r#"
                    UPDATE jobs SET status = 'cancelled', cancel_reason = $2, finished_at = now()
                    WHERE id = $1 AND status IN ('draft', 'pending', 'running', 'paused')
                    "#,
                    job_id,
                    reason
                )
                .execute(&mut **tx)
                .await?;

                queue_reply(
                    tx,
                    &target_chat_id,
                    &format!("cancelled job: {reason}"),
                    trace_id,
                )
                .await?;
            }
            TriageDecision::ResumeJob { job_id, input } => {
                sqlx::query!(
                    r#"
                    UPDATE jobs SET status = 'pending', resume_input = $2
                    WHERE id = $1 AND status = 'paused'
                    "#,
                    job_id,
                    input
                )
                .execute(&mut **tx)
                .await?;
            }
            TriageDecision::SetSubscription { enabled } => {
                sqlx::query(
                    r#"
                    INSERT INTO chat_subscriptions (chat_id, enabled)
                    VALUES ($1, $2)
                    ON CONFLICT (chat_id) DO UPDATE
                    SET enabled = EXCLUDED.enabled,
                        updated_at = now()
                    "#,
                )
                .bind(&target_chat_id)
                .bind(enabled)
                .execute(&mut **tx)
                .await?;

                *is_subscribed = enabled;

                let status = if enabled {
                    "subscribed"
                } else {
                    "unsubscribed"
                };
                queue_reply(tx, &target_chat_id, status, trace_id).await?;
            }
            TriageDecision::Noop => {}
        }
    }
    Ok(())
}

pub async fn triage_tick(db: &PgPool, ai: &dyn AiService) -> Result<u32> {
    let rows = sqlx::query_as!(
        UnroutedMessage,
        r#"
        SELECT id, platform_chat_id, content, trace_id,
               attachments, updated_at, created_at
        FROM messages
        WHERE direction = 'in' AND routed_at IS NULL
        ORDER BY created_at
        LIMIT 50
        "#
    )
    .fetch_all(db)
    .await?;

    if rows.is_empty() {
        return Ok(0);
    }

    let mut by_chat: HashMap<String, Vec<&UnroutedMessage>> = HashMap::new();
    for row in &rows {
        by_chat
            .entry(row.platform_chat_id.clone())
            .or_default()
            .push(row);
    }

    let mut processed = 0u32;

    for (chat_id, msgs) in &by_chat {
        let mut is_subscribed = is_chat_subscribed(db, chat_id).await?;

        let active_jobs = sqlx::query_as!(
            ActiveJobSummary,
            r#"
            SELECT id, status, prompt
            FROM jobs
            WHERE chat_id = $1 AND status IN ('draft', 'pending', 'running', 'paused')
            ORDER BY created_at DESC
            "#,
            chat_id
        )
        .fetch_all(db)
        .await?;

        let active_crons = sqlx::query_as!(
            ActiveCronSummary,
            r#"
            SELECT name, schedule, prompt
            FROM crons
            WHERE chat_id = $1 AND enabled = true
            ORDER BY name
            "#,
            chat_id
        )
        .fetch_all(db)
        .await?;

        let triage_msgs: Vec<TriageMessage> = msgs
            .iter()
            .map(|m| TriageMessage {
                id: m.id,
                content: m.content.clone(),
                is_edit: m.updated_at > m.created_at,
                has_audio: message_has_audio_attachment(&m.attachments),
                has_image: message_has_image_attachment(&m.attachments),
            })
            .collect();

        let source_ids: Vec<Uuid> = msgs.iter().map(|m| m.id).collect();

        // fetch recent conversation history for context recall
        let history = sqlx::query_scalar::<_, String>(
            r#"
            SELECT content FROM messages
            WHERE platform_chat_id = $1
              AND content IS NOT NULL
              AND routed_at IS NOT NULL
            ORDER BY created_at DESC
            LIMIT 20
            "#,
        )
        .bind(chat_id)
        .fetch_all(db)
        .await
        .unwrap_or_default();

        let input = TriageBatchInput {
            chat_id: chat_id.clone(),
            messages: triage_msgs,
            active_jobs,
            active_crons,
            history,
        };

        tracing::info!(
            chat_id = %chat_id,
            message_count = msgs.len(),
            active_jobs = input.active_jobs.len(),
            active_crons = input.active_crons.len(),
            "triage: routing batch"
        );

        let result = ai
            .triage_batch(input)
            .await
            .map_err(|e| ForgeError::Internal(e.to_string()))?;

        for (i, d) in result.decisions.iter().enumerate() {
            let action = match d {
                TriageDecision::Reply { .. } => "reply",
                TriageDecision::CreateJob { kind, .. } => kind.as_str(),
                TriageDecision::CreateCron { .. } => "create_cron",
                TriageDecision::CancelJob { .. } => "cancel_job",
                TriageDecision::CancelCron { .. } => "cancel_cron",
                TriageDecision::ResumeJob { .. } => "resume_job",
                TriageDecision::SetSubscription { .. } => "set_subscription",
                TriageDecision::Noop => "noop",
            };
            tracing::info!(chat_id = %chat_id, decision_index = i, action, "triage: decision");
        }

        let decisions = if should_force_audio_transcription_job(msgs, &result.decisions) {
            vec![TriageDecision::CreateJob {
                prompt: AUDIO_ONLY_JOB_PROMPT.to_string(),
                kind: "action".to_string(),
            }]
        } else {
            result.decisions
        };

        let trace_id = msgs
            .iter()
            .find_map(|m| m.trace_id)
            .unwrap_or_else(Uuid::new_v4);
        let mut tx = db.begin().await?;

        apply_decisions(
            &mut tx,
            chat_id,
            decisions,
            &source_ids,
            trace_id,
            &mut is_subscribed,
        )
        .await?;

        sqlx::query!(
            r#"
            UPDATE messages SET routed_at = now()
            WHERE id = ANY($1)
            "#,
            &source_ids
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query!(
            r#"
            INSERT INTO events (trace_id, source, action, payload)
            VALUES ($1, 'triage', 'batch_routed', $2)
            "#,
            trace_id,
            serde_json::json!({ "chat_id": chat_id, "count": msgs.len() })
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        processed += msgs.len() as u32;
    }

    Ok(processed)
}

#[forge::daemon]
pub async fn triage(ctx: &DaemonContext) -> Result<()> {
    let ai: Arc<dyn AiService> = crate::get_ai_service();
    let poll_ms: u64 = ctx.env_parse("YUI_LOOP_POLL_MS_TRIAGE").unwrap_or(500);

    loop {
        tokio::select! {
            _ = ctx.shutdown_signal() => break,
            _ = tokio::time::sleep(std::time::Duration::from_millis(poll_ms)) => {
                match triage_tick(ctx.db(), ai.as_ref()).await {
                    Ok(n) if n > 0 => tracing::info!(processed = n, "triage tick"),
                    Err(e) => tracing::error!(error = %e, "triage tick failed"),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::{EnrichInput, EnrichOutput, TriageBatchDecision};
    use forge::testing::*;

    struct GreetingAiService;

    #[async_trait::async_trait]
    impl AiService for GreetingAiService {
        async fn triage_batch(
            &self,
            _input: TriageBatchInput,
        ) -> anyhow::Result<TriageBatchDecision> {
            Ok(TriageBatchDecision {
                decisions: vec![TriageDecision::Reply {
                    text: "Hey there! 👋 How can I help you today?".to_string(),
                }],
            })
        }

        async fn enrich_job(&self, input: EnrichInput) -> anyhow::Result<EnrichOutput> {
            Ok(EnrichOutput {
                enriched_prompt: input.prompt,
            })
        }

        async fn embed_text(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![])
        }

        async fn rewrite_reply(
            &self,
            content: &str,
            _history: &[String],
        ) -> anyhow::Result<String> {
            Ok(content.to_string())
        }
    }

    async fn setup() -> (IsolatedTestDb, PgPool) {
        let base = TestDatabase::embedded().await.unwrap();
        let db = base.isolated("triage").await.unwrap();
        db.run_sql(&forge::get_internal_sql()).await.unwrap();
        db.run_sql(
            r#"
            CREATE TABLE messages (
                id uuid PRIMARY KEY,
                platform_id text,
                platform_chat_id text NOT NULL,
                platform_sender_id text,
                direction text NOT NULL,
                content text,
                attachments jsonb NOT NULL DEFAULT '[]'::jsonb,
                trace_id uuid,
                routed_at timestamptz,
                created_at timestamptz NOT NULL DEFAULT now(),
                updated_at timestamptz NOT NULL DEFAULT now()
            );

            CREATE TABLE jobs (
                id uuid PRIMARY KEY,
                kind text NOT NULL,
                chat_id text NOT NULL,
                status text NOT NULL,
                prompt text,
                source_ids uuid[] NOT NULL DEFAULT '{}',
                trace_id uuid,
                cancel_reason text,
                finished_at timestamptz,
                resume_input text,
                created_at timestamptz NOT NULL DEFAULT now()
            );

            CREATE TABLE outbox (
                id uuid PRIMARY KEY DEFAULT (md5(random()::text || clock_timestamp()::text)::uuid),
                chat_id text NOT NULL,
                content text,
                attachments jsonb NOT NULL DEFAULT '[]'::jsonb,
                trace_id uuid
            );

            CREATE TABLE events (
                id uuid PRIMARY KEY DEFAULT (md5(random()::text || clock_timestamp()::text)::uuid),
                trace_id uuid,
                source text NOT NULL,
                action text NOT NULL,
                payload jsonb NOT NULL DEFAULT '{}'::jsonb
            );

            CREATE TABLE crons (
                id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
                name text NOT NULL UNIQUE,
                schedule text NOT NULL,
                timezone text NOT NULL DEFAULT 'UTC',
                chat_id text NOT NULL,
                prompt text NOT NULL,
                enabled bool NOT NULL DEFAULT true,
                next_run_at timestamptz
            );

            CREATE TABLE chat_subscriptions (
                chat_id text PRIMARY KEY,
                enabled bool NOT NULL DEFAULT true,
                created_at timestamptz NOT NULL DEFAULT now(),
                updated_at timestamptz NOT NULL DEFAULT now()
            );
            "#,
        )
        .await
        .unwrap();
        let pool = db.pool().clone();
        (db, pool)
    }

    #[tokio::test]
    async fn audio_plus_small_talk_forces_transcription_job() {
        let (_db, pool) = setup().await;
        let chat_id = "25491067@s.whatsapp.net";

        sqlx::query(
            "INSERT INTO messages (id, platform_chat_id, direction, content) VALUES ($1, $2, 'in', $3)",
        )
        .bind(Uuid::new_v4())
        .bind(chat_id)
        .bind("hi")
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO messages (id, platform_chat_id, direction, attachments)
            VALUES ($1, $2, 'in', $3)
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(chat_id)
        .bind(serde_json::json!([{
            "type": "audio",
            "path": "storage/media/sample.ogg",
            "mime": "audio/ogg",
            "name": "sample.ogg"
        }]))
        .execute(&pool)
        .await
        .unwrap();

        triage_tick(&pool, &GreetingAiService).await.unwrap();

        let prompt: String = sqlx::query_scalar("SELECT prompt FROM jobs LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(prompt.contains("Transcribe the attached audio"));

        let outbox_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(outbox_count, 0);
    }

}
