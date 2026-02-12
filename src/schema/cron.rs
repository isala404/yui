use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[forge::model]
pub struct Cron {
    pub id: Uuid,
    pub name: String,
    pub schedule: String,
    pub timezone: String,
    pub chat_id: String,
    pub prompt: String,
    pub enabled: bool,
    pub last_run_at: Option<DateTime<Utc>>,
    pub next_run_at: Option<DateTime<Utc>>,
    pub last_job_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
