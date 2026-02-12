use chrono::{DateTime, Utc};
use forge::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[forge::forge_enum]
pub enum Direction {
    In,
    Out,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    #[serde(rename = "type")]
    pub kind: String,
    pub path: String,
    pub mime: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[forge::model]
pub struct Message {
    pub id: Uuid,
    pub platform_id: Option<String>,
    pub platform_chat_id: String,
    pub platform_sender_id: Option<String>,
    pub direction: Direction,
    pub content: Option<String>,
    pub attachments: serde_json::Value,
    pub content_version: i32,
    pub audit_processed_version: i32,
    pub routed_at: Option<DateTime<Utc>>,
    pub audit_processed_at: Option<DateTime<Utc>>,
    pub is_deleted: bool,
    pub reply_to_id: Option<Uuid>,
    pub job_id: Option<Uuid>,
    pub trace_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
