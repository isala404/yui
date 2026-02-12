use crate::services::AiService;
use forge::prelude::*;
use sqlx::PgPool;
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use uuid::Uuid;
use wacore::proto_helpers::MessageExt;
use wacore::types::events::Event;
use whatsapp_rust::bot::Bot;
use whatsapp_rust::store::SqliteStore;
use whatsapp_rust::{ChatStateEvent, Jid};
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust_ureq_http_client::UreqHttpClient;

/// OnceCell because gateway initializes this async, delivery reads it later
pub static WA_CLIENT: tokio::sync::OnceCell<Arc<whatsapp_rust::Client>> =
    tokio::sync::OnceCell::const_new();

struct BufferedMessage {
    platform_id: String,
    platform_chat_id: String,
    platform_sender_id: String,
    content: Option<String>,
    attachments: serde_json::Value,
}

struct TypingBuffer {
    messages: Vec<BufferedMessage>,
    is_typing: bool,
    last_user_activity: tokio::time::Instant,
}

impl TypingBuffer {
    fn new(now: tokio::time::Instant) -> Self {
        Self {
            messages: Vec::new(),
            is_typing: false,
            last_user_activity: now,
        }
    }

    fn upsert_message(&mut self, message: BufferedMessage, now: tokio::time::Instant) {
        if let Some(existing) = self
            .messages
            .iter_mut()
            .find(|m| m.platform_id == message.platform_id)
        {
            *existing = message;
        } else {
            self.messages.push(message);
        }
        // a sent message is an implicit end-of-typing signal
        self.is_typing = false;
        self.last_user_activity = now;
    }

    fn mark_typing(&mut self, now: tokio::time::Instant) {
        self.is_typing = true;
        self.last_user_activity = now;
    }

    fn mark_idle(&mut self, now: tokio::time::Instant) {
        self.is_typing = false;
        self.last_user_activity = now;
    }

    fn ready_to_flush(&self, now: tokio::time::Instant, flush_after: Duration) -> bool {
        !self.messages.is_empty()
            && !self.is_typing
            && now.duration_since(self.last_user_activity) >= flush_after
    }
}

fn should_process_inbound_message(chat_id: &str, is_from_me: bool) -> bool {
    if is_from_me {
        return false;
    }
    // status updates arrive on a broadcast chat and should not enter bot routing
    !chat_id.eq_ignore_ascii_case("status@broadcast")
}

async fn flush_buffer(
    db: &PgPool,
    ai: &dyn AiService,
    chat_id: &str,
    buffer: &mut TypingBuffer,
) -> Result<()> {
    if buffer.messages.is_empty() {
        return Ok(());
    }

    let messages = std::mem::take(&mut buffer.messages);
    let trace_id = Uuid::new_v4();
    let mut tx = db.begin().await?;

    for msg in &messages {
        let embedding = if let Some(ref text) = msg.content {
            ai.embed_text(text).await.ok()
        } else {
            None
        };

        sqlx::query!(
            r#"
            INSERT INTO messages (platform_id, platform_chat_id, platform_sender_id, direction, content, attachments, embedding, trace_id)
            VALUES ($1, $2, $3, 'in', $4, $5, $6::vector, $7)
            ON CONFLICT (platform_id) DO UPDATE SET
                content = EXCLUDED.content,
                attachments = EXCLUDED.attachments,
                is_deleted = false,
                content_version = CASE
                    WHEN messages.content IS DISTINCT FROM EXCLUDED.content
                      OR messages.attachments IS DISTINCT FROM EXCLUDED.attachments
                      OR messages.is_deleted = true
                    THEN messages.content_version + 1
                    ELSE messages.content_version
                END,
                updated_at = CASE
                    WHEN messages.content IS DISTINCT FROM EXCLUDED.content
                      OR messages.attachments IS DISTINCT FROM EXCLUDED.attachments
                      OR messages.is_deleted = true
                    THEN now()
                    ELSE messages.updated_at
                END
            "#,
            msg.platform_id,
            msg.platform_chat_id,
            msg.platform_sender_id,
            msg.content,
            msg.attachments,
            embedding.as_deref() as Option<&[f32]>,
            trace_id
        )
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query!(
        r#"
        INSERT INTO events (trace_id, source, action, payload)
        VALUES ($1, 'gateway', 'batch_received', $2)
        "#,
        trace_id,
        serde_json::json!({ "chat_id": chat_id, "count": messages.len() })
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    tracing::info!(chat_id, count = messages.len(), "flushed inbound buffer");
    Ok(())
}

async fn try_save_media(
    client: &Arc<whatsapp_rust::Client>,
    media: &dyn wacore::download::Downloadable,
    path: &str,
    kind: &str,
    mime: &str,
    name: &str,
    attachments: &mut Vec<serde_json::Value>,
) {
    if download_media(client, media, path).await {
        attachments.push(serde_json::json!({
            "type": kind,
            "path": path,
            "mime": mime,
            "name": name,
        }));
    }
}

async fn save_media(
    client: &Arc<whatsapp_rust::Client>,
    msg: &waproto::whatsapp::Message,
    msg_id: &str,
    media_dir: &str,
) -> Vec<serde_json::Value> {
    let base = msg.get_base_message();
    let mut attachments = Vec::new();

    if let Some(img) = &base.image_message {
        let path = format!("{media_dir}/{msg_id}.jpg");
        let mime = img.mimetype.as_deref().unwrap_or("image/jpeg");
        let name = format!("{msg_id}.jpg");
        try_save_media(client, img.as_ref(), &path, "image", mime, &name, &mut attachments).await;
    }
    if let Some(vid) = &base.video_message {
        let path = format!("{media_dir}/{msg_id}.mp4");
        let mime = vid.mimetype.as_deref().unwrap_or("video/mp4");
        let name = format!("{msg_id}.mp4");
        try_save_media(client, vid.as_ref(), &path, "video", mime, &name, &mut attachments).await;
    }
    if let Some(aud) = &base.audio_message {
        let path = format!("{media_dir}/{msg_id}.ogg");
        let mime = aud.mimetype.as_deref().unwrap_or("audio/ogg");
        let name = format!("{msg_id}.ogg");
        try_save_media(client, aud.as_ref(), &path, "audio", mime, &name, &mut attachments).await;
    }
    if let Some(doc) = &base.document_message {
        let ext = doc
            .mimetype
            .as_deref()
            .and_then(|m| m.split('/').next_back())
            .unwrap_or("bin");
        let path = format!("{media_dir}/{msg_id}.{ext}");
        let mime = doc.mimetype.as_deref().unwrap_or("application/octet-stream");
        let name = doc.file_name.clone().unwrap_or_else(|| format!("{msg_id}.{ext}"));
        try_save_media(client, doc.as_ref(), &path, "document", mime, &name, &mut attachments).await;
    }

    attachments
}

async fn download_media(
    client: &Arc<whatsapp_rust::Client>,
    media: &dyn wacore::download::Downloadable,
    path: &str,
) -> bool {
    let mut buf = Cursor::new(Vec::new());
    if let Err(e) = client.download_to_file(media, &mut buf).await {
        tracing::error!(path, error = %e, "failed to download media");
        return false;
    }

    let data = buf.into_inner();
    if let Err(e) = tokio::fs::write(path, &data).await {
        tracing::error!(path, error = %e, "failed to write media");
        return false;
    }

    tracing::info!(path, bytes = data.len(), "saved media");
    true
}

#[forge::daemon]
pub async fn gateway(ctx: &DaemonContext) -> Result<()> {
    let ai: Arc<dyn AiService> = crate::get_ai_service();
    let flush_idle_ms: u64 = ctx.env_parse("YUI_TYPING_IDLE_FLUSH_MS").unwrap_or(5000);
    let poll_ms: u64 = ctx.env_parse("YUI_LOOP_POLL_MS_GATEWAY").unwrap_or(500);
    let wa_db_path: String = ctx
        .env_parse("YUI_WHATSAPP_DB_PATH")
        .unwrap_or_else(|_| "whatsapp.db".to_string());
    let media_dir: String = ctx
        .env_parse("YUI_MEDIA_DIR")
        .unwrap_or_else(|_| "storage/media".to_string());

    std::fs::create_dir_all(&media_dir).ok();

    let buffers: Arc<Mutex<HashMap<String, TypingBuffer>>> = Arc::new(Mutex::new(HashMap::new()));
    let db = ctx.db().clone();

    let backend = Arc::new(
        SqliteStore::new(&wa_db_path)
            .await
            .map_err(|e| ForgeError::Internal(format!("failed to create sqlite store: {e}")))?,
    );

    let buf_handle = buffers.clone();
    let media_handle = media_dir.clone();

    let mut bot = Bot::builder()
        .with_backend(backend)
        .with_transport_factory(TokioWebSocketTransportFactory::new())
        .with_http_client(UreqHttpClient::new())
        .on_event(move |event, client| {
            let buffers = buf_handle.clone();
            let media_dir = media_handle.clone();
            async move {
                match event {
                    Event::PairingQrCode { code, timeout } => {
                        tracing::info!(timeout_secs = timeout.as_secs(), "scan QR code:");
                        qr2term::print_qr(&code).unwrap_or_else(|e| {
                            tracing::error!(error = %e, "failed to render QR");
                            println!("QR data: {}", code);
                        });
                    }
                    Event::Connected(_) => {
                        tracing::info!("WhatsApp connected");
                    }
                    Event::LoggedOut(_) => {
                        tracing::error!("WhatsApp logged out");
                    }
                    Event::Message(msg, msg_info) => {
                        let chat_id = msg_info.source.chat.to_string();
                        if !should_process_inbound_message(&chat_id, msg_info.source.is_from_me) {
                            tracing::debug!(
                                chat_id,
                                is_from_me = msg_info.source.is_from_me,
                                "skipping inbound message"
                            );
                            return;
                        }

                        let sender_id = msg_info.source.sender.to_string();
                        let text = msg.text_content().map(|s| s.to_string());
                        let attachments = save_media(&client, &msg, &msg_info.id, &media_dir).await;

                        let buffered = BufferedMessage {
                            platform_id: msg_info.id.clone(),
                            platform_chat_id: chat_id.clone(),
                            platform_sender_id: sender_id,
                            content: text,
                            attachments: serde_json::json!(attachments),
                        };

                        let now = tokio::time::Instant::now();
                        let mut bufs = buffers.lock().await;
                        let entry = bufs
                            .entry(chat_id)
                            .or_insert_with(|| TypingBuffer::new(now));
                        entry.upsert_message(buffered, now);

                        let receipt_sender = msg_info
                            .source
                            .is_group
                            .then(|| msg_info.source.sender.clone());
                        if let Err(e) = client
                            .mark_as_read(
                                &msg_info.source.chat,
                                receipt_sender.as_ref(),
                                vec![msg_info.id],
                            )
                            .await
                        {
                            tracing::warn!(error = %e, "failed to mark as read");
                        }
                    }
                    _ => {}
                }
            }
        })
        .build()
        .await
        .map_err(|e| ForgeError::Internal(format!("failed to build WhatsApp bot: {e}")))?;

    let (cs_tx, mut cs_rx) = tokio::sync::mpsc::unbounded_channel::<ChatStateEvent>();
    bot.client()
        .register_chatstate_handler(Arc::new(move |event: ChatStateEvent| {
            if let Err(e) = cs_tx.send(event) {
                tracing::warn!(error = %e, "chatstate channel closed");
            }
        }))
        .await;

    let _handle = bot
        .run()
        .await
        .map_err(|e| ForgeError::Internal(format!("bot failed to start: {e}")))?;
    let client = bot.client().clone();

    WA_CLIENT.set(client.clone()).ok();
    tracing::info!("gateway daemon started with WhatsApp client");

    // Track user typing state. Actual flush happens in the periodic loop after idle delay.
    let buffers_cs = buffers.clone();
    tokio::spawn(async move {
        while let Some(event) = cs_rx.recv().await {
            let chat_key = event.chat.to_string();
            let now = tokio::time::Instant::now();
            let mut bufs = buffers_cs.lock().await;
            let entry = bufs
                .entry(chat_key.clone())
                .or_insert_with(|| TypingBuffer::new(now));
            match event.state {
                wacore::iq::chatstate::ReceivedChatState::Typing
                | wacore::iq::chatstate::ReceivedChatState::RecordingAudio => {
                    entry.mark_typing(now);
                    tracing::debug!(chat = %chat_key, "user typing");
                }
                wacore::iq::chatstate::ReceivedChatState::Idle => {
                    entry.mark_idle(now);
                    tracing::debug!(chat = %chat_key, "user idle");
                }
            }
        }
    });

    loop {
        tokio::select! {
            _ = ctx.shutdown_signal() => break,
            _ = tokio::time::sleep(std::time::Duration::from_millis(poll_ms)) => {
                let now = tokio::time::Instant::now();
                let flush_threshold = Duration::from_millis(flush_idle_ms);

                let mut bufs = buffers.lock().await;
                let stale_chats: Vec<String> = bufs
                    .iter()
                    .filter(|(_, buf)| buf.ready_to_flush(now, flush_threshold))
                    .map(|(chat_id, _)| chat_id.clone())
                    .collect();

                for chat_id in stale_chats {
                    if let Some(buf) = bufs.get_mut(&chat_id)
                        && let Err(e) = flush_buffer(&db, ai.as_ref(), &chat_id, buf).await
                    {
                        tracing::error!(chat_id, error = %e, "buffer flush failed");
                    }
                }

                bufs.retain(|_, buf| !buf.messages.is_empty() || buf.is_typing);
                drop(bufs);

                // typing indicator only for actively working chats
                // excludes paused jobs (they have outbox questions but aren't progressing)
                let active_chats: Vec<String> = sqlx::query_scalar!(
                    r#"
                    SELECT DISTINCT chat_id as "chat_id!"
                    FROM (
                        SELECT chat_id
                        FROM jobs
                        WHERE status IN ('draft', 'pending', 'running')
                        UNION
                        SELECT o.chat_id
                        FROM outbox o
                        WHERE o.processed_at IS NULL
                          AND NOT EXISTS (
                            SELECT 1 FROM jobs j
                            WHERE j.id = o.job_id AND j.status = 'paused'
                          )
                    ) active
                    "#
                )
                .fetch_all(&db)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "failed to query active chats");
                    Vec::new()
                });

                for chat_id in active_chats {
                    if let Ok(jid) = chat_id.parse::<Jid>() {
                        let _ = client.chatstate().send_composing(&jid).await;
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_message(id: &str, content: &str) -> BufferedMessage {
        BufferedMessage {
            platform_id: id.to_string(),
            platform_chat_id: "chat".to_string(),
            platform_sender_id: "sender".to_string(),
            content: Some(content.to_string()),
            attachments: serde_json::json!([]),
        }
    }

    #[test]
    fn does_not_flush_while_user_is_typing() {
        let t0 = tokio::time::Instant::now();
        let mut buffer = TypingBuffer::new(t0);
        buffer.upsert_message(make_message("m1", "hello"), t0);

        let t1 = t0 + Duration::from_secs(1);
        buffer.mark_typing(t1);

        let t2 = t0 + Duration::from_secs(20);
        assert!(!buffer.ready_to_flush(t2, Duration::from_secs(5)));
    }

    #[test]
    fn flushes_only_after_idle_window() {
        let t0 = tokio::time::Instant::now();
        let mut buffer = TypingBuffer::new(t0);
        buffer.upsert_message(make_message("m1", "hello"), t0);

        let t1 = t0 + Duration::from_secs(1);
        buffer.mark_typing(t1);
        let t2 = t0 + Duration::from_secs(3);
        buffer.mark_idle(t2);

        assert!(!buffer.ready_to_flush(t0 + Duration::from_secs(7), Duration::from_secs(5)));
        assert!(buffer.ready_to_flush(t0 + Duration::from_secs(8), Duration::from_secs(5)));
    }

    #[test]
    fn upsert_replaces_same_platform_message_in_buffer() {
        let t0 = tokio::time::Instant::now();
        let mut buffer = TypingBuffer::new(t0);
        buffer.upsert_message(make_message("same", "hello"), t0);
        buffer.upsert_message(
            make_message("same", "hello edited"),
            t0 + Duration::from_secs(1),
        );

        assert_eq!(buffer.messages.len(), 1);
        assert_eq!(buffer.messages[0].content.as_deref(), Some("hello edited"));
    }

    #[test]
    fn message_clears_typing_flag_so_buffer_can_flush() {
        let t0 = tokio::time::Instant::now();
        let mut buffer = TypingBuffer::new(t0);

        let t1 = t0 + Duration::from_secs(1);
        buffer.mark_typing(t1);

        let t2 = t0 + Duration::from_secs(2);
        buffer.upsert_message(make_message("m1", "hello"), t2);

        let t3 = t0 + Duration::from_secs(8);
        assert!(buffer.ready_to_flush(t3, Duration::from_secs(5)));
    }

    #[test]
    fn skips_self_sent_messages() {
        assert!(!should_process_inbound_message(
            "25491067@s.whatsapp.net",
            true
        ));
    }

    #[test]
    fn skips_status_broadcast_messages() {
        assert!(!should_process_inbound_message("status@broadcast", false));
        assert!(!should_process_inbound_message("STATUS@BROADCAST", false));
    }

    #[test]
    fn processes_normal_inbound_messages() {
        assert!(should_process_inbound_message(
            "25491067@s.whatsapp.net",
            false
        ));
    }
}
