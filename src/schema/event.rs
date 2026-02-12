use chrono::{DateTime, Utc};
use forge::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[forge::model]
pub struct Event {
    pub id: Uuid,
    pub trace_id: Option<Uuid>,
    pub source: String,
    pub action: String,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}
