use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[forge::forge_enum]
pub enum JobStatus {
    Draft,
    Pending,
    Running,
    Paused,
    Done,
    Failed,
    Cancelled,
}

#[forge::forge_enum]
pub enum JobKind {
    Action,
    Chat,
    Schedule,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[forge::model]
pub struct Job {
    pub id: Uuid,
    pub kind: JobKind,
    pub chat_id: String,
    pub status: JobStatus,
    pub prompt: Option<String>,
    pub enriched_prompt: Option<String>,
    pub source_ids: Vec<Uuid>,
    pub resume_input: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub cancel_reason: Option<String>,
    pub forge_job_id: Option<Uuid>,
    pub session_id: Option<String>,
    pub container_id: Option<String>,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub question_pending: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub trace_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
