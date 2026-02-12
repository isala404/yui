use crate::functions::gateway::WA_CLIENT;
use crate::schema::message::Attachment;
use forge::prelude::*;
use sqlx::PgPool;
use uuid::Uuid;
use wacore::download::MediaType;
use waproto::whatsapp as wa;
use whatsapp_rust::Jid;
use whatsapp_rust::upload::UploadResponse;

struct PendingOutbox {
    id: Uuid,
    chat_id: String,
    content: Option<String>,
    attachments: serde_json::Value,
    attempt_count: i32,
    trace_id: Option<Uuid>,
}

const MAX_DELIVERY_ATTEMPTS: i32 = 5;

fn parse_attachments(raw: &serde_json::Value) -> std::result::Result<Vec<Attachment>, String> {
    if raw.is_null() {
        return Ok(vec![]);
    }
    serde_json::from_value(raw.clone()).map_err(|e| format!("invalid attachments payload: {e}"))
}

fn media_type_from_attachment(kind: &str) -> Option<MediaType> {
    match kind {
        "image" => Some(MediaType::Image),
        "video" => Some(MediaType::Video),
        "audio" => Some(MediaType::Audio),
        "document" => Some(MediaType::Document),
        _ => None,
    }
}

fn take_caption_for_attachment(
    index: usize,
    attachment: &Attachment,
    pending_text: &mut Option<String>,
) -> Option<String> {
    if index == 0 && matches!(attachment.kind.as_str(), "image" | "video" | "document") {
        pending_text.take()
    } else {
        None
    }
}

fn build_media_message(
    upload: &UploadResponse,
    attachment: &Attachment,
    caption: Option<String>,
) -> std::result::Result<wa::Message, String> {
    let common_fields = || {
        (
            Some(upload.url.clone()),
            Some(upload.direct_path.clone()),
            Some(upload.media_key.clone()),
            Some(upload.file_sha256.clone()),
            Some(upload.file_enc_sha256.clone()),
            Some(upload.file_length),
        )
    };

    let message = match attachment.kind.as_str() {
        "image" => wa::Message {
            image_message: {
                let (url, direct_path, media_key, file_sha256, file_enc_sha256, file_length) =
                    common_fields();
                Some(Box::new(wa::message::ImageMessage {
                    url,
                    direct_path,
                    media_key,
                    file_sha256,
                    file_enc_sha256,
                    file_length,
                    mimetype: Some(attachment.mime.clone()),
                    caption,
                    ..Default::default()
                }))
            },
            ..Default::default()
        },
        "video" => wa::Message {
            video_message: {
                let (url, direct_path, media_key, file_sha256, file_enc_sha256, file_length) =
                    common_fields();
                Some(Box::new(wa::message::VideoMessage {
                    url,
                    direct_path,
                    media_key,
                    file_sha256,
                    file_enc_sha256,
                    file_length,
                    mimetype: Some(attachment.mime.clone()),
                    caption,
                    ..Default::default()
                }))
            },
            ..Default::default()
        },
        "audio" => wa::Message {
            audio_message: {
                let (url, direct_path, media_key, file_sha256, file_enc_sha256, file_length) =
                    common_fields();
                Some(Box::new(wa::message::AudioMessage {
                    url,
                    direct_path,
                    media_key,
                    file_sha256,
                    file_enc_sha256,
                    file_length,
                    mimetype: Some(attachment.mime.clone()),
                    ..Default::default()
                }))
            },
            ..Default::default()
        },
        "document" => wa::Message {
            document_message: {
                let (url, direct_path, media_key, file_sha256, file_enc_sha256, file_length) =
                    common_fields();
                Some(Box::new(wa::message::DocumentMessage {
                    url,
                    direct_path,
                    media_key,
                    file_sha256,
                    file_enc_sha256,
                    file_length,
                    mimetype: Some(attachment.mime.clone()),
                    file_name: attachment.name.clone(),
                    caption,
                    ..Default::default()
                }))
            },
            ..Default::default()
        },
        other => {
            return Err(format!("unsupported attachment type: {other}"));
        }
    };
    Ok(message)
}

async fn send_attachment(
    client: &std::sync::Arc<whatsapp_rust::Client>,
    jid: &Jid,
    attachment: &Attachment,
    caption: Option<String>,
) -> std::result::Result<String, String> {
    let media_type = media_type_from_attachment(&attachment.kind)
        .ok_or_else(|| format!("unsupported attachment type: {}", attachment.kind))?;

    let data = std::fs::read(&attachment.path)
        .map_err(|e| format!("failed to read attachment {}: {e}", attachment.path))?;
    let upload = client
        .upload(data, media_type)
        .await
        .map_err(|e| format!("failed to upload attachment {}: {e}", attachment.path))?;
    let msg = build_media_message(&upload, attachment, caption)?;
    client
        .send_message(jid.clone(), msg)
        .await
        .map_err(|e| e.to_string())
}

async fn send_text_message(
    client: &std::sync::Arc<whatsapp_rust::Client>,
    jid: &Jid,
    text: String,
) -> std::result::Result<String, String> {
    client
        .send_message(
            jid.clone(),
            wa::Message {
                conversation: Some(text),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| e.to_string())
}

async fn send_via_whatsapp(
    client: &std::sync::Arc<whatsapp_rust::Client>,
    item: &PendingOutbox,
) -> std::result::Result<Option<String>, String> {
    let jid: Jid = item
        .chat_id
        .parse()
        .map_err(|e| format!("invalid jid: {}", e))?;

    let _ = client.chatstate().send_composing(&jid).await;

    let attachments = parse_attachments(&item.attachments)?;
    let mut pending_text = item.content.clone();
    let mut sent_id = None;

    for (idx, attachment) in attachments.iter().enumerate() {
        let caption = take_caption_for_attachment(idx, attachment, &mut pending_text);
        let id = send_attachment(client, &jid, attachment, caption).await?;
        sent_id = Some(id);
    }

    if let Some(text) = pending_text {
        let id = send_text_message(client, &jid, text).await?;
        sent_id = Some(id);
    }

    let _ = client.chatstate().send_paused(&jid).await;
    Ok(sent_id)
}

pub async fn delivery_tick(db: &PgPool) -> Result<u32> {
    let fake_send = std::env::var("YUI_DELIVERY_FAKE_SEND")
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false);

    let wa_client = if fake_send { None } else { WA_CLIENT.get() };
    if wa_client.is_none() && !fake_send {
        return Ok(0);
    }

    let pending = sqlx::query_as!(
        PendingOutbox,
        r#"
        SELECT id, chat_id, content, attachments, attempt_count, trace_id
        FROM outbox
        WHERE processed_at IS NULL AND rewritten_at IS NOT NULL AND attempt_count < $1
        ORDER BY created_at
        LIMIT 20
        FOR UPDATE SKIP LOCKED
        "#,
        MAX_DELIVERY_ATTEMPTS
    )
    .fetch_all(db)
    .await?;

    if pending.is_empty() {
        return Ok(0);
    }

    let mut processed = 0u32;

    tracing::debug!(
        count = pending.len(),
        fake_send,
        "delivery: processing outbox"
    );

    for item in &pending {
        let trace_id = item.trace_id.unwrap_or_else(Uuid::new_v4);
        let msg_id = Uuid::new_v4();
        let platform_id = format!("out_{}", msg_id.as_simple());

        let has_attachments = item.attachments.as_array().is_some_and(|a| !a.is_empty());
        tracing::info!(
            outbox_id = %item.id,
            chat_id = %item.chat_id,
            has_text = item.content.is_some(),
            has_attachments,
            attempt = item.attempt_count + 1,
            "delivery: sending message"
        );

        let mut tx = db.begin().await?;

        // record outbound message before send so audit trail exists even if delivery fails
        sqlx::query!(
            r#"
            INSERT INTO messages (id, platform_id, platform_chat_id, direction, content, attachments, trace_id)
            VALUES ($1, $2, $3, 'out', $4, $5, $6)
            "#,
            msg_id,
            platform_id,
            item.chat_id,
            item.content,
            item.attachments,
            trace_id
        )
        .execute(&mut *tx)
        .await?;

        let send_result = match wa_client {
            Some(client) => {
                let result = send_via_whatsapp(client, item).await;
                if let Ok(Some(real_id)) = &result {
                    sqlx::query!(
                        "UPDATE messages SET platform_id = $1 WHERE id = $2",
                        real_id,
                        msg_id
                    )
                    .execute(&mut *tx)
                    .await?;
                }
                result.map(|_| ())
            }
            None => Ok(()),
        };

        match send_result {
            Ok(()) => {
                sqlx::query!(
                    r#"
                    UPDATE outbox SET processed_at = now(), attempt_count = attempt_count + 1
                    WHERE id = $1
                    "#,
                    item.id
                )
                .execute(&mut *tx)
                .await?;

                sqlx::query!(
                    r#"
                    INSERT INTO events (trace_id, source, action, payload)
                    VALUES ($1, 'delivery', 'message_sent', $2)
                    "#,
                    trace_id,
                    serde_json::json!({ "message_id": msg_id, "chat_id": item.chat_id, "outbox_id": item.id })
                )
                .execute(&mut *tx)
                .await?;

                processed += 1;
            }
            Err(err) => {
                sqlx::query!(
                    r#"
                    UPDATE outbox SET attempt_count = attempt_count + 1, last_error = $2
                    WHERE id = $1
                    "#,
                    item.id,
                    err
                )
                .execute(&mut *tx)
                .await?;

                sqlx::query!(
                    r#"
                    INSERT INTO events (trace_id, source, action, payload)
                    VALUES ($1, 'delivery', 'send_failed', $2)
                    "#,
                    trace_id,
                    serde_json::json!({ "outbox_id": item.id, "error": err, "attempt": item.attempt_count + 1 })
                )
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
    }

    Ok(processed)
}

#[forge::daemon]
pub async fn delivery(ctx: &DaemonContext) -> Result<()> {
    let poll_ms: u64 = ctx.env_parse("YUI_LOOP_POLL_MS_DELIVERY").unwrap_or(500);

    loop {
        tokio::select! {
            _ = ctx.shutdown_signal() => break,
            _ = tokio::time::sleep(std::time::Duration::from_millis(poll_ms)) => {
                match delivery_tick(ctx.db()).await {
                    Ok(n) if n > 0 => tracing::info!(processed = n, "delivery tick"),
                    Err(e) => tracing::error!(error = %e, "delivery tick failed"),
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
        let db = base.isolated("delivery").await.unwrap();
        db.run_sql(&forge::get_internal_sql()).await.unwrap();
        db.run_sql(
            r#"
            CREATE TABLE messages (
                id uuid PRIMARY KEY,
                platform_id text UNIQUE,
                platform_chat_id text NOT NULL,
                platform_sender_id text,
                direction text NOT NULL,
                content text,
                attachments jsonb NOT NULL DEFAULT '[]'::jsonb,
                trace_id uuid,
                created_at timestamptz NOT NULL DEFAULT now()
            );

            CREATE TABLE outbox (
                id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
                chat_id text NOT NULL,
                content text,
                attachments jsonb NOT NULL DEFAULT '[]'::jsonb,
                reply_to text,
                processed_at timestamptz,
                rewritten_at timestamptz,
                attempt_count int NOT NULL DEFAULT 0,
                last_error text,
                trace_id uuid,
                created_at timestamptz NOT NULL DEFAULT now()
            );

            CREATE TABLE events (
                id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
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
    fn parses_attachment_array() {
        let raw = serde_json::json!([
            {
                "type": "image",
                "path": "storage/media/1.jpg",
                "mime": "image/jpeg",
                "name": "1.jpg"
            }
        ]);
        let attachments = parse_attachments(&raw).unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].kind, "image");
    }

    #[test]
    fn rejects_invalid_attachment_payload() {
        let raw = serde_json::json!({"bad":"shape"});
        assert!(parse_attachments(&raw).is_err());
    }

    #[test]
    fn maps_media_types() {
        assert!(matches!(
            media_type_from_attachment("image"),
            Some(MediaType::Image)
        ));
        assert!(matches!(
            media_type_from_attachment("video"),
            Some(MediaType::Video)
        ));
        assert!(matches!(
            media_type_from_attachment("audio"),
            Some(MediaType::Audio)
        ));
        assert!(matches!(
            media_type_from_attachment("document"),
            Some(MediaType::Document)
        ));
        assert!(media_type_from_attachment("other").is_none());
    }

    #[test]
    fn first_supported_attachment_consumes_caption() {
        let attachment = Attachment {
            kind: "image".to_string(),
            path: "storage/media/1.jpg".to_string(),
            mime: "image/jpeg".to_string(),
            name: Some("1.jpg".to_string()),
        };

        let mut pending = Some("caption".to_string());
        let caption = take_caption_for_attachment(0, &attachment, &mut pending);
        assert_eq!(caption.as_deref(), Some("caption"));
        assert!(pending.is_none());
    }

    #[test]
    fn unsupported_or_non_first_attachment_keeps_text_pending() {
        let attachment = Attachment {
            kind: "audio".to_string(),
            path: "storage/media/1.ogg".to_string(),
            mime: "audio/ogg".to_string(),
            name: Some("1.ogg".to_string()),
        };

        let mut pending = Some("caption".to_string());
        assert!(take_caption_for_attachment(0, &attachment, &mut pending).is_none());
        assert_eq!(pending.as_deref(), Some("caption"));

        let second = Attachment {
            kind: "image".to_string(),
            path: "storage/media/2.jpg".to_string(),
            mime: "image/jpeg".to_string(),
            name: Some("2.jpg".to_string()),
        };
        assert!(take_caption_for_attachment(1, &second, &mut pending).is_none());
        assert_eq!(pending.as_deref(), Some("caption"));
    }

    #[tokio::test]
    async fn keeps_outbox_pending_without_whatsapp_client() {
        let (_db, pool) = setup().await;

        sqlx::query(
            r#"
            INSERT INTO outbox (chat_id, content, attachments)
            VALUES ($1, $2, $3)
            "#,
        )
        .bind("25491067@s.whatsapp.net")
        .bind("help")
        .bind(serde_json::json!([]))
        .execute(&pool)
        .await
        .unwrap();

        let processed = delivery_tick(&pool).await.unwrap();
        assert_eq!(processed, 0);

        let pending_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM outbox WHERE processed_at IS NULL AND attempt_count = 0",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(pending_count, 1);

        let sent_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE direction = 'out'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(sent_count, 0);
    }
}
