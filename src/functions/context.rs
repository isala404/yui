use crate::services::{AiService, EnrichInput, MediaPreprocessor};
use forge::prelude::*;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

struct DraftJob {
    id: Uuid,
    chat_id: String,
    prompt: Option<String>,
    trace_id: Option<Uuid>,
    source_ids: Vec<Uuid>,
}

struct HistoryRow {
    content: Option<String>,
}

struct AttachmentRow {
    attachments: serde_json::Value,
}

/// Enriches the prompt with attachment contents so the agent has full context.
async fn collect_attachment_contents(
    db: &PgPool,
    source_ids: &[Uuid],
    prompt: &str,
    preprocessor: Option<&MediaPreprocessor>,
) -> Vec<String> {
    if source_ids.is_empty() {
        return vec![];
    }

    let rows = sqlx::query_as!(
        AttachmentRow,
        r#"
        SELECT attachments
        FROM messages
        WHERE id = ANY($1)
          AND attachments <> '[]'::jsonb
        "#,
        source_ids
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();

    let mut contents = vec![];
    for row in rows {
        let arr = match row.attachments.as_array() {
            Some(a) => a,
            None => continue,
        };
        for att in arr {
            let mime = att["mime"].as_str().unwrap_or("");
            let path = att["path"].as_str().unwrap_or("");
            let name = att["name"].as_str().unwrap_or("file");
            let kind = att["type"].as_str().unwrap_or("file");

            if path.is_empty() {
                continue;
            }

            let media_ref = || {
                let container_path = path.replace("storage/media/", "/storage/media/");
                format!("[Attached {kind}: {name}] (available at {container_path}, mime: {mime})")
            };

            if mime.starts_with("text/") || mime.contains("json") || mime.contains("xml") {
                match tokio::fs::read_to_string(path).await {
                    Ok(text) => {
                        let truncated: String = text.chars().take(4000).collect();
                        contents.push(format!("[Attached file: {name}]\n{truncated}"));
                    }
                    Err(e) => {
                        tracing::warn!(path, error = %e, "failed to read attachment");
                    }
                }
            } else if mime.starts_with("audio/") {
                let preprocessed = match preprocessor {
                    Some(pp) => pp.transcribe_audio(path).await.ok(),
                    None => None,
                };
                if let Some(transcript) = preprocessed {
                    tracing::info!(path, "transcribed audio attachment");
                    contents.push(format!(
                        "[Transcription of voice note: {name}]\n{transcript}"
                    ));
                } else {
                    contents.push(media_ref());
                }
            } else if mime.starts_with("image/") {
                let preprocessed = match preprocessor {
                    Some(pp) => pp.describe_image(path, prompt).await.ok(),
                    None => None,
                };
                if let Some(description) = preprocessed {
                    tracing::info!(path, "described image attachment");
                    contents.push(format!("[Description of image: {name}]\n{description}"));
                } else {
                    contents.push(media_ref());
                }
            } else {
                contents.push(media_ref());
            }
        }
    }
    contents
}

pub async fn context_tick(db: &PgPool, ai: &dyn AiService) -> Result<u32> {
    let drafts = sqlx::query_as!(
        DraftJob,
        r#"
        SELECT id, chat_id, prompt, trace_id, source_ids as "source_ids!"
        FROM jobs
        WHERE status = 'draft'
        ORDER BY created_at
        LIMIT 10
        FOR UPDATE SKIP LOCKED
        "#
    )
    .fetch_all(db)
    .await?;

    if drafts.is_empty() {
        return Ok(0);
    }

    let mut processed = 0u32;

    tracing::debug!(count = drafts.len(), "context: enriching draft jobs");

    for draft in &drafts {
        let prompt = draft.prompt.clone().unwrap_or_default();

        tracing::info!(
            job_id = %draft.id,
            chat_id = %draft.chat_id,
            prompt_len = prompt.len(),
            source_ids = draft.source_ids.len(),
            "context: enriching job"
        );

        let embedding = ai
            .embed_text(&prompt)
            .await
            .map_err(|e| ForgeError::Internal(e.to_string()))?;

        let recent_rows = sqlx::query_as!(
            HistoryRow,
            r#"
            SELECT content
            FROM messages
            WHERE platform_chat_id = $1
              AND content IS NOT NULL
            ORDER BY created_at DESC
            LIMIT 50
            "#,
            draft.chat_id,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default();

        let recent: Vec<String> = recent_rows.into_iter().filter_map(|r| r.content).collect();

        // retrieve additional relevant history via vector similarity
        let rag_rows = sqlx::query_as!(
            HistoryRow,
            r#"
            SELECT content
            FROM messages
            WHERE platform_chat_id = $1
              AND embedding IS NOT NULL
              AND content IS NOT NULL
              AND id != ALL($3::uuid[])
            ORDER BY embedding <=> $2::vector
            LIMIT 10
            "#,
            draft.chat_id,
            &embedding as &[f32],
            &draft.source_ids
        )
        .fetch_all(db)
        .await
        .unwrap_or_default();

        let rag: Vec<String> = rag_rows.into_iter().filter_map(|r| r.content).collect();

        // merge: recent messages first, then RAG results (deduplicated)
        let mut history = recent;
        for item in rag {
            if !history.contains(&item) {
                history.push(item);
            }
        }

        let preprocessor = crate::get_media_preprocessor();
        let attachment_contents =
            collect_attachment_contents(db, &draft.source_ids, &prompt, preprocessor).await;
        let attachment_count = attachment_contents.len();
        let prompt_with_attachments = if attachment_contents.is_empty() {
            prompt.clone()
        } else {
            format!("{}\n\n{}", prompt, attachment_contents.join("\n\n"))
        };

        let history_count = history.len();
        let enriched = ai
            .enrich_job(EnrichInput {
                job_id: draft.id,
                prompt: prompt_with_attachments,
                history,
            })
            .await
            .map_err(|e| ForgeError::Internal(e.to_string()))?;

        let trace_id = draft.trace_id.unwrap_or_else(Uuid::new_v4);

        tracing::info!(
            job_id = %draft.id,
            history_count,
            attachment_count,
            enriched_len = enriched.enriched_prompt.len(),
            "context: enrichment complete, promoting to pending"
        );

        let mut tx = db.begin().await?;

        sqlx::query!(
            r#"
            UPDATE jobs SET status = 'pending', enriched_prompt = $2
            WHERE id = $1 AND status = 'draft'
            "#,
            draft.id,
            enriched.enriched_prompt
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query!(
            r#"
            INSERT INTO events (trace_id, source, action, payload)
            VALUES ($1, 'context', 'job_enriched', $2)
            "#,
            trace_id,
            serde_json::json!({ "job_id": draft.id, "enriched": true })
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        processed += 1;
    }

    Ok(processed)
}

#[forge::daemon]
pub async fn context_loop(ctx: &DaemonContext) -> Result<()> {
    let ai: Arc<dyn AiService> = crate::get_ai_service();
    let poll_ms: u64 = ctx.env_parse("YUI_LOOP_POLL_MS_CONTEXT").unwrap_or(500);

    loop {
        tokio::select! {
            _ = ctx.shutdown_signal() => break,
            _ = tokio::time::sleep(std::time::Duration::from_millis(poll_ms)) => {
                match context_tick(ctx.db(), ai.as_ref()).await {
                    Ok(n) if n > 0 => tracing::info!(processed = n, "context tick"),
                    Err(e) => tracing::error!(error = %e, "context tick failed"),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

