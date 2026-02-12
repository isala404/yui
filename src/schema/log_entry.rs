use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[forge::forge_enum]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[forge::model]
pub struct LogEntry {
    pub id: Uuid,
    pub job_id: Uuid,
    pub stream: LogStream,
    pub line: String,
    pub created_at: DateTime<Utc>,
}
