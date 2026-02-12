use forge::prelude::*;
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;

struct DueCron {
    id: Uuid,
    name: String,
    chat_id: String,
    schedule: String,
    prompt: String,
    timezone: String,
    next_run_at: Option<chrono::DateTime<chrono::Utc>>,
}

// the `cron` crate requires 6-field (second-granularity) expressions,
// so we prepend "0" to standard 5-field minute-granularity inputs
fn normalize_schedule(schedule: &str) -> String {
    let fields: Vec<&str> = schedule.split_whitespace().collect();
    let normalized = fields.join(" ");
    if fields.len() == 5 {
        format!("0 {normalized}")
    } else {
        normalized
    }
}

pub fn compute_next_run_at(
    schedule: &str,
    timezone: &str,
    from: chrono::DateTime<chrono::Utc>,
) -> Result<chrono::DateTime<chrono::Utc>> {
    let tz: chrono_tz::Tz = timezone
        .parse()
        .map_err(|_| ForgeError::Validation(format!("invalid timezone: {timezone}")))?;
    let normalized = normalize_schedule(schedule);
    let parsed = cron::Schedule::from_str(&normalized).map_err(|e| {
        ForgeError::Validation(format!("invalid cron expression `{normalized}`: {e}"))
    })?;

    let from_local = from.with_timezone(&tz);
    let next_local = parsed
        .after(&from_local)
        .next()
        .ok_or_else(|| ForgeError::Validation("cron has no future occurrences".to_string()))?;

    Ok(next_local.with_timezone(&chrono::Utc))
}

pub async fn clock_tick(db: &PgPool) -> Result<u32> {
    let due = sqlx::query_as!(
        DueCron,
        r#"
        SELECT id, name, chat_id, schedule, prompt, timezone, next_run_at
        FROM crons
        WHERE enabled = true AND (next_run_at IS NULL OR next_run_at <= now())
        ORDER BY next_run_at NULLS FIRST
        LIMIT 20
        FOR UPDATE SKIP LOCKED
        "#
    )
    .fetch_all(db)
    .await?;

    if due.is_empty() {
        return Ok(0);
    }

    let mut processed = 0u32;

    tracing::debug!(count = due.len(), "clock: processing due crons");

    for cron in &due {
        if let Some(limit) = parse_auto_stop_limit(&cron.prompt) {
            let fired_count = sqlx::query_scalar::<_, i64>(
                r#"
                SELECT COUNT(*)::bigint
                FROM events
                WHERE source = 'clock'
                  AND action = 'cron_fired'
                  AND payload->>'cron_id' = $1
                "#,
            )
            .bind(cron.id.to_string())
            .fetch_one(db)
            .await
            .unwrap_or(0);

            if fired_count >= limit {
                tracing::info!(
                    cron_id = %cron.id,
                    cron_name = %cron.name,
                    fired_count,
                    limit,
                    "clock: auto-stopping cron (limit reached)"
                );
                let mut tx = db.begin().await?;
                sqlx::query!("UPDATE crons SET enabled = false WHERE id = $1", cron.id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query!(
                    r#"
                    INSERT INTO events (source, action, payload)
                    VALUES ('clock', 'cron_auto_stopped', $1)
                    "#,
                    serde_json::json!({
                        "cron_id": cron.id,
                        "limit": limit,
                        "fired_count": fired_count
                    })
                )
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                processed += 1;
                continue;
            }
        }

        let now = chrono::Utc::now();
        let next = match compute_next_run_at(&cron.schedule, &cron.timezone, now) {
            Ok(next) => next,
            Err(err) => {
                let mut tx = db.begin().await?;
                sqlx::query!(
                    r#"
                    UPDATE crons SET enabled = false
                    WHERE id = $1
                    "#,
                    cron.id
                )
                .execute(&mut *tx)
                .await?;

                sqlx::query!(
                    r#"
                    INSERT INTO events (source, action, payload)
                    VALUES ('clock', 'cron_disabled_invalid_schedule', $1)
                    "#,
                    serde_json::json!({
                        "cron_id": cron.id,
                        "name": cron.name,
                        "schedule": cron.schedule,
                        "timezone": cron.timezone,
                        "error": err.to_string(),
                    })
                )
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                processed += 1;
                continue;
            }
        };

        // Recovery path for older rows created without next_run_at:
        // set first scheduled run and let a future tick execute it.
        if cron.next_run_at.is_none() {
            let mut tx = db.begin().await?;
            sqlx::query!(
                r#"
                UPDATE crons SET next_run_at = $2
                WHERE id = $1
                "#,
                cron.id,
                next
            )
            .execute(&mut *tx)
            .await?;

            sqlx::query!(
                r#"
                INSERT INTO events (source, action, payload)
                VALUES ('clock', 'cron_scheduled', $1)
                "#,
                serde_json::json!({
                    "cron_id": cron.id,
                    "name": cron.name,
                    "next_run_at": next
                })
            )
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            processed += 1;
            continue;
        }

        let trace_id = Uuid::new_v4();
        let job_id = Uuid::new_v4();

        tracing::info!(
            cron_id = %cron.id,
            cron_name = %cron.name,
            schedule = %cron.schedule,
            job_id = %job_id,
            "clock: firing cron, creating job"
        );

        let mut tx = db.begin().await?;

        sqlx::query!(
            r#"
            INSERT INTO jobs (id, kind, chat_id, status, prompt, trace_id)
            VALUES ($1, 'schedule', $2, 'draft', $3, $4)
            "#,
            job_id,
            cron.chat_id,
            cron.prompt,
            trace_id
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query!(
            r#"
            UPDATE crons SET last_run_at = now(), next_run_at = $2, last_job_id = $3
            WHERE id = $1
            "#,
            cron.id,
            next,
            job_id
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query!(
            r#"
            INSERT INTO events (trace_id, source, action, payload)
            VALUES ($1, 'clock', 'cron_fired', $2)
            "#,
            trace_id,
            serde_json::json!({ "cron_id": cron.id, "cron_name": cron.schedule, "job_id": job_id })
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        processed += 1;
    }

    Ok(processed)
}

fn parse_auto_stop_limit(prompt: &str) -> Option<i64> {
    let marker = "AUTO_STOP_AFTER=";
    let start = prompt.find(marker)?;
    let rest = &prompt[start + marker.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse::<i64>().ok()
    }
}

#[forge::daemon]
pub async fn clock(ctx: &DaemonContext) -> Result<()> {
    let poll_ms: u64 = ctx.env_parse("YUI_LOOP_POLL_MS_CLOCK").unwrap_or(1000);

    loop {
        tokio::select! {
            _ = ctx.shutdown_signal() => break,
            _ = tokio::time::sleep(std::time::Duration::from_millis(poll_ms)) => {
                match clock_tick(ctx.db()).await {
                    Ok(n) if n > 0 => tracing::info!(processed = n, "clock tick"),
                    Err(e) => tracing::error!(error = %e, "clock tick failed"),
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
        let db = base.isolated("clock").await.unwrap();
        db.run_sql(&forge::get_internal_sql()).await.unwrap();
        db.run_sql(
            r#"
            CREATE TABLE crons (
                id uuid PRIMARY KEY,
                name text NOT NULL UNIQUE,
                schedule text NOT NULL,
                timezone text NOT NULL DEFAULT 'UTC',
                chat_id text NOT NULL,
                prompt text NOT NULL,
                enabled bool NOT NULL DEFAULT true,
                last_run_at timestamptz,
                next_run_at timestamptz,
                last_job_id uuid
            );

            CREATE TABLE jobs (
                id uuid PRIMARY KEY,
                kind text NOT NULL,
                chat_id text NOT NULL,
                status text NOT NULL,
                prompt text,
                trace_id uuid,
                created_at timestamptz NOT NULL DEFAULT now()
            );

            CREATE TABLE events (
                id uuid PRIMARY KEY DEFAULT (md5(random()::text || clock_timestamp()::text)::uuid),
                trace_id uuid,
                source text NOT NULL,
                action text NOT NULL,
                payload jsonb NOT NULL DEFAULT '{}'::jsonb,
                created_at timestamptz NOT NULL DEFAULT now()
            );
            "#,
        )
        .await
        .unwrap();
        let pool = db.pool().clone();
        (db, pool)
    }

    #[test]
    fn computes_next_run_for_second_granularity_cron() {
        let now = chrono::Utc::now();
        let next = compute_next_run_at("* * * * * *", "UTC", now).unwrap();
        assert!(next > now);
        assert!(next <= now + chrono::Duration::minutes(1));
    }

    #[test]
    fn parses_auto_stop_limit_from_prompt() {
        assert_eq!(
            parse_auto_stop_limit("get ISS location AUTO_STOP_AFTER=5"),
            Some(5)
        );
        assert_eq!(parse_auto_stop_limit("no marker"), None);
    }

    #[tokio::test]
    async fn initializes_missing_next_run_without_firing_job() {
        let (_db, pool) = setup().await;
        let cron_id = Uuid::new_v4();

        sqlx::query!(
            r#"
            INSERT INTO crons (id, name, schedule, timezone, chat_id, prompt, enabled, next_run_at)
            VALUES ($1, 'every_second', '* * * * * *', 'UTC', 'chat', 'echo test', true, NULL)
            "#,
            cron_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let processed = clock_tick(&pool).await.unwrap();
        assert_eq!(processed, 1);

        let job_count: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap()
            .unwrap_or(0);
        assert_eq!(job_count, 0);

        let next_run = sqlx::query_scalar!("SELECT next_run_at FROM crons WHERE id = $1", cron_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(next_run.is_some());
    }

    #[tokio::test]
    async fn fires_due_cron_and_advances_next_run() {
        let (_db, pool) = setup().await;
        let cron_id = Uuid::new_v4();
        let due_at = chrono::Utc::now() - chrono::Duration::seconds(2);

        sqlx::query!(
            r#"
            INSERT INTO crons (id, name, schedule, timezone, chat_id, prompt, enabled, next_run_at)
            VALUES ($1, 'due_cron', '* * * * * *', 'UTC', 'chat', 'echo test', true, $2)
            "#,
            cron_id,
            due_at
        )
        .execute(&pool)
        .await
        .unwrap();

        let processed = clock_tick(&pool).await.unwrap();
        assert_eq!(processed, 1);

        let job_count: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap()
            .unwrap_or(0);
        assert_eq!(job_count, 1);

        let row = sqlx::query!(
            "SELECT last_run_at, next_run_at FROM crons WHERE id = $1",
            cron_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(row.last_run_at.is_some());
        assert!(row.next_run_at.is_some());
        assert!(row.next_run_at.unwrap() > due_at);
    }
}
