use chrono::{DateTime, Utc};
use forge::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[forge::model]
pub struct Outbox {
    pub id: Uuid,
    pub chat_id: String,
    pub content: Option<String>,
    pub attachments: serde_json::Value,
    pub reply_to: Option<String>,
    pub processed_at: Option<DateTime<Utc>>,
    pub attempt_count: i32,
    pub last_error: Option<String>,
    pub job_id: Option<Uuid>,
    pub reply_to_message_id: Option<Uuid>,
    pub rewritten_at: Option<DateTime<Utc>>,
    pub trace_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
