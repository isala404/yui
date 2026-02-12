use forge::prelude::*;
use sqlx::PgPool;
use uuid::Uuid;

struct AuditableMessage {
    id: Uuid,
    platform_chat_id: String,
    content: Option<String>,
    is_deleted: bool,
    content_version: i32,
}

struct LinkedJob {
    id: Uuid,
    chat_id: String,
}

pub async fn audit_tick(db: &PgPool) -> Result<u32> {
    let changed = sqlx::query_as!(
        AuditableMessage,
        r#"
        SELECT id, platform_chat_id, content, is_deleted, content_version
        FROM messages
        WHERE audit_processed_version < content_version
           OR (is_deleted = true AND audit_processed_at IS NULL)
        ORDER BY updated_at
        LIMIT 20
        FOR UPDATE SKIP LOCKED
        "#
    )
    .fetch_all(db)
    .await?;

    if changed.is_empty() {
        return Ok(0);
    }

    let mut processed = 0u32;

    for msg in &changed {
        let trace_id = Uuid::new_v4();
        let mut tx = db.begin().await?;

        let linked_jobs = sqlx::query_as!(
            LinkedJob,
            r#"
            SELECT id, chat_id
            FROM jobs
            WHERE $1 = ANY(source_ids)
              AND status IN ('draft', 'pending', 'running', 'paused')
            "#,
            msg.id
        )
        .fetch_all(&mut *tx)
        .await?;

        for job in &linked_jobs {
            let reason = if msg.is_deleted {
                "source message deleted"
            } else {
                "source message edited"
            };

            sqlx::query!(
                r#"
                UPDATE jobs SET status = 'cancelled', cancel_reason = $2, finished_at = now()
                WHERE id = $1 AND status IN ('draft', 'pending', 'running', 'paused')
                "#,
                job.id,
                reason
            )
            .execute(&mut *tx)
            .await?;

            sqlx::query!(
                r#"
                INSERT INTO outbox (chat_id, content, trace_id)
                VALUES ($1, $2, $3)
                "#,
                job.chat_id,
                format!("task cancelled: {reason}"),
                trace_id
            )
            .execute(&mut *tx)
            .await?;

            sqlx::query!(
                r#"
                INSERT INTO events (trace_id, source, action, payload)
                VALUES ($1, 'audit', 'job_cancelled', $2)
                "#,
                trace_id,
                serde_json::json!({ "job_id": job.id, "reason": reason, "message_id": msg.id })
            )
            .execute(&mut *tx)
            .await?;
        }

        if !msg.is_deleted
            && let Some(ref content) = msg.content
            && !linked_jobs.is_empty()
        {
            let job_id = Uuid::new_v4();
            sqlx::query!(
                r#"
                INSERT INTO jobs (id, kind, chat_id, status, prompt, source_ids, trace_id)
                VALUES ($1, 'action', $2, 'draft', $3, $4, $5)
                "#,
                job_id,
                msg.platform_chat_id,
                content,
                &[msg.id],
                trace_id
            )
            .execute(&mut *tx)
            .await?;

            sqlx::query!(
                r#"
                INSERT INTO events (trace_id, source, action, payload)
                VALUES ($1, 'audit', 'job_recreated', $2)
                "#,
                trace_id,
                serde_json::json!({ "job_id": job_id, "reason": "message_edited", "message_id": msg.id })
            )
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query!(
            "UPDATE messages SET audit_processed_at = now(), audit_processed_version = $2 WHERE id = $1",
            msg.id,
            msg.content_version
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        processed += 1;
    }

    Ok(processed)
}

#[forge::daemon]
pub async fn audit(ctx: &DaemonContext) -> Result<()> {
    let poll_ms: u64 = ctx.env_parse("YUI_LOOP_POLL_MS_AUDIT").unwrap_or(500);

    loop {
        tokio::select! {
            _ = ctx.shutdown_signal() => break,
            _ = tokio::time::sleep(std::time::Duration::from_millis(poll_ms)) => {
                match audit_tick(ctx.db()).await {
                    Ok(n) if n > 0 => tracing::info!(processed = n, "audit tick"),
                    Err(e) => tracing::error!(error = %e, "audit tick failed"),
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
    use forge::testing::*;

    async fn setup() -> (IsolatedTestDb, PgPool) {
        let base = TestDatabase::embedded().await.unwrap();
        let db = base.isolated("audit").await.unwrap();
        db.run_sql(&forge::get_internal_sql()).await.unwrap();
        db.run_sql(
            r#"
            CREATE TABLE messages (
                id uuid PRIMARY KEY,
                platform_chat_id text NOT NULL,
                content text,
                is_deleted bool NOT NULL DEFAULT false,
                content_version int NOT NULL DEFAULT 1,
                audit_processed_version int NOT NULL DEFAULT 1,
                audit_processed_at timestamptz,
                created_at timestamptz NOT NULL DEFAULT now(),
                updated_at timestamptz NOT NULL DEFAULT now()
            );

            CREATE TABLE jobs (
                id uuid PRIMARY KEY,
                kind text NOT NULL DEFAULT 'action',
                chat_id text NOT NULL,
                status text NOT NULL,
                prompt text,
                source_ids uuid[] NOT NULL DEFAULT '{}',
                trace_id uuid,
                cancel_reason text,
                finished_at timestamptz
            );

            CREATE TABLE outbox (
                id uuid PRIMARY KEY DEFAULT (md5(random()::text || clock_timestamp()::text)::uuid),
                chat_id text NOT NULL,
                content text,
                trace_id uuid
            );

            CREATE TABLE events (
                id uuid PRIMARY KEY DEFAULT (md5(random()::text || clock_timestamp()::text)::uuid),
                trace_id uuid,
                source text NOT NULL,
                action text NOT NULL,
                payload jsonb DEFAULT '{}'::jsonb
            );
            "#,
        )
        .await
        .unwrap();
        let pool = db.pool().clone();
        (db, pool)
    }

    #[tokio::test]
    async fn does_not_treat_routed_marker_update_as_edit() {
        let (_db, pool) = setup().await;
        let message_id = Uuid::new_v4();
        let job_id = Uuid::new_v4();
        let chat_id = "25491067@s.whatsapp.net";

        sqlx::query(
            r#"
            INSERT INTO messages (
                id, platform_chat_id, content,
                content_version, audit_processed_version
            )
            VALUES ($1, $2, $3, 1, 1)
            "#,
        )
        .bind(message_id)
        .bind(chat_id)
        .bind("hello")
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO jobs (id, kind, chat_id, status, prompt, source_ids)
            VALUES ($1, 'action', $2, 'running', $3, $4)
            "#,
        )
        .bind(job_id)
        .bind(chat_id)
        .bind("hello")
        .bind(vec![message_id])
        .execute(&pool)
        .await
        .unwrap();

        let processed = audit_tick(&pool).await.unwrap();
        assert_eq!(processed, 0);

        let status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id = $1")
            .bind(job_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "running");
    }

    #[tokio::test]
    async fn edited_message_cancels_linked_job_and_recreates_draft() {
        let (_db, pool) = setup().await;
        let message_id = Uuid::new_v4();
        let job_id = Uuid::new_v4();
        let chat_id = "25491067@s.whatsapp.net";

        sqlx::query(
            r#"
            INSERT INTO messages (
                id, platform_chat_id, content,
                content_version, audit_processed_version
            )
            VALUES ($1, $2, $3, 1, 1)
            "#,
        )
        .bind(message_id)
        .bind(chat_id)
        .bind("hello")
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO jobs (id, kind, chat_id, status, prompt, source_ids)
            VALUES ($1, 'action', $2, 'running', $3, $4)
            "#,
        )
        .bind(job_id)
        .bind(chat_id)
        .bind("hello")
        .bind(vec![message_id])
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "UPDATE messages SET content = $2, content_version = content_version + 1 WHERE id = $1",
        )
        .bind(message_id)
        .bind("hello edited")
        .execute(&pool)
        .await
        .unwrap();

        let processed = audit_tick(&pool).await.unwrap();
        assert_eq!(processed, 1);

        let old_status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id = $1")
            .bind(job_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(old_status, "cancelled");

        let recreated_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE status = 'draft' AND $1 = ANY(source_ids) AND prompt = $2",
        )
        .bind(message_id)
        .bind("hello edited")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(recreated_count, 1);

        let processed_version: i32 =
            sqlx::query_scalar("SELECT audit_processed_version FROM messages WHERE id = $1")
                .bind(message_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(processed_version, 2);
    }
}
