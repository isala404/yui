use crate::services::AiService;
use forge::prelude::*;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

struct PendingRewrite {
    id: Uuid,
    chat_id: String,
    content: Option<String>,
}

fn should_skip_rewrite(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return true;
    }

    // Preserve strict-format replies.
    if trimmed == "OK" {
        return true;
    }
    if trimmed.starts_with('{') && trimmed.ends_with('}') && !trimmed.contains('\n') {
        return true;
    }
    if trimmed.len() <= 8 && !trimmed.chars().any(char::is_whitespace) {
        return true;
    }

    false
}

fn strip_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("|--") || trimmed.starts_with("| --") {
            continue;
        }

        let line = if trimmed.starts_with('#') {
            trimmed.trim_start_matches('#').trim()
        } else {
            line
        };

        let line = if line.contains('|') && line.starts_with('|') {
            line.trim_matches('|')
                .split('|')
                .map(|cell| cell.trim())
                .collect::<Vec<_>>()
                .join(" - ")
        } else {
            line.to_string()
        };

        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line);
    }

    out = out.replace("**", "").replace("__", "");
    out = out.replace('`', "");

    out
}

fn strip_internal_paths(text: &str) -> String {
    let mut result = text.to_string();
    for prefix in ["/workspace/", "/storage/media/", "/tmp/"] {
        while let Some(start) = result.find(prefix) {
            let end = result[start..]
                .find(|c: char| c.is_whitespace() || c == ')' || c == '`' || c == '"' || c == '\'')
                .map(|i| start + i)
                .unwrap_or(result.len());
            let filename = result[start..end]
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string();
            result.replace_range(start..end, &filename);
        }
    }
    result
}

fn sanitize_reply_text(content: &str) -> String {
    let mut text = content.replace("\r\n", "\n").trim().to_string();

    // Strip markdown code fences from accidental tool-style output.
    if text.starts_with("```") && text.ends_with("```") {
        text = text
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim()
            .to_string();
    }

    // Strip markdown formatting for WhatsApp-friendly plain text.
    text = strip_markdown(&text);

    text = strip_internal_paths(&text);

    // Avoid huge raw dumps in chat.
    const MAX_LEN: usize = 1200;
    if text.chars().count() > MAX_LEN {
        text = text.chars().take(MAX_LEN).collect::<String>();
        text.push_str("\n\n(abridged)");
    }

    text
}

pub async fn reply_tick(db: &PgPool, ai: &dyn AiService) -> Result<u32> {
    let skip_rewrite = std::env::var("YUI_REPLY_SKIP_LLM")
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false);

    let pending = sqlx::query_as!(
        PendingRewrite,
        r#"
        SELECT id, chat_id, content
        FROM outbox
        WHERE rewritten_at IS NULL AND processed_at IS NULL
        ORDER BY created_at
        LIMIT 10
        FOR UPDATE SKIP LOCKED
        "#
    )
    .fetch_all(db)
    .await?;

    if pending.is_empty() {
        return Ok(0);
    }

    tracing::debug!(
        count = pending.len(),
        "reply: processing pending outbox entries"
    );

    let mut processed = 0u32;

    for entry in &pending {
        // media-only entries don't need rewriting
        let Some(ref content) = entry.content else {
            tracing::debug!(outbox_id = %entry.id, "reply: media-only entry, skipping rewrite");
            sqlx::query!(
                "UPDATE outbox SET rewritten_at = now() WHERE id = $1",
                entry.id
            )
            .execute(db)
            .await?;
            processed += 1;
            continue;
        };

        if should_skip_rewrite(content) || skip_rewrite {
            let final_text = sanitize_reply_text(content);
            let reason = if skip_rewrite { "llm_disabled" } else { "format_preserved" };
            tracing::debug!(outbox_id = %entry.id, reason, "reply: skipping rewrite");
            sqlx::query!(
                "UPDATE outbox SET content = $2, rewritten_at = now() WHERE id = $1",
                entry.id,
                final_text
            )
            .execute(db)
            .await?;
            processed += 1;
            continue;
        }

        // gather trace context for personalized rewriting
        let history = sqlx::query_scalar::<_, String>(
            r#"
            SELECT content FROM messages
            WHERE platform_chat_id = $1 AND content IS NOT NULL
            ORDER BY created_at DESC
            LIMIT 10
            "#,
        )
        .bind(&entry.chat_id)
        .fetch_all(db)
        .await
        .unwrap_or_default();

        tracing::info!(
            outbox_id = %entry.id,
            chat_id = %entry.chat_id,
            content_len = content.len(),
            history_count = history.len(),
            "reply: rewriting with LLM"
        );

        let rewritten = match ai.rewrite_reply(content, &history).await {
            Ok(text) => text,
            Err(e) => {
                tracing::warn!(outbox_id = %entry.id, error = %e, "reply rewrite failed, using raw content");
                content.clone()
            }
        };
        let rewritten = sanitize_reply_text(&rewritten);

        // LLM can request multiple WhatsApp bubbles via separator
        let segments: Vec<&str> = rewritten
            .split("\n---\n")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        if segments.len() <= 1 {
            let final_text = segments.first().copied().unwrap_or(content);
            let final_text = sanitize_reply_text(final_text);
            sqlx::query!(
                "UPDATE outbox SET content = $2, rewritten_at = now() WHERE id = $1",
                entry.id,
                final_text
            )
            .execute(db)
            .await?;
        } else {
            // multi-message: update first, insert rest
            let trace_id =
                sqlx::query_scalar!("SELECT trace_id FROM outbox WHERE id = $1", entry.id)
                    .fetch_one(db)
                    .await?;

            sqlx::query!(
                "UPDATE outbox SET content = $2, rewritten_at = now() WHERE id = $1",
                entry.id,
                segments[0]
            )
            .execute(db)
            .await?;

            for segment in &segments[1..] {
                let segment = sanitize_reply_text(segment);
                sqlx::query!(
                    r#"
                    INSERT INTO outbox (chat_id, content, trace_id, rewritten_at)
                    VALUES ($1, $2, $3, now())
                    "#,
                    entry.chat_id,
                    segment,
                    trace_id
                )
                .execute(db)
                .await?;
            }
        }

        processed += 1;
    }

    Ok(processed)
}

#[forge::daemon]
pub async fn reply(ctx: &DaemonContext) -> Result<()> {
    let ai: Arc<dyn AiService> = crate::get_ai_service();
    let poll_ms: u64 = ctx.env_parse("YUI_LOOP_POLL_MS_REPLY").unwrap_or(300);

    loop {
        tokio::select! {
            _ = ctx.shutdown_signal() => break,
            _ = tokio::time::sleep(std::time::Duration::from_millis(poll_ms)) => {
                match reply_tick(ctx.db(), ai.as_ref()).await {
                    Ok(n) if n > 0 => tracing::info!(processed = n, "reply tick"),
                    Err(e) => tracing::error!(error = %e, "reply tick failed"),
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

    #[test]
    fn skips_rewrite_for_exact_ok() {
        assert!(should_skip_rewrite("OK"));
    }

    #[test]
    fn sanitize_strips_fence_markers() {
        let text = "```json\n{\"ok\":true}\n```";
        let out = sanitize_reply_text(text);
        assert_eq!(out, "json\n{\"ok\":true}");
    }

    #[test]
    fn strip_markdown_removes_bold_and_headers() {
        let text = "# Title\n**bold** and `code`";
        let out = strip_markdown(text);
        assert_eq!(out, "Title\nbold and code");
    }
}
