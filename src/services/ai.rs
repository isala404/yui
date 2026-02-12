use crate::services::embedding::EmbeddingService;
use crate::services::reply_client::ReplyClient;
use crate::services::triage_client::{TriageClient, TriageClientConfig};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveCronSummary {
    pub name: String,
    pub schedule: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageBatchInput {
    pub chat_id: String,
    pub messages: Vec<TriageMessage>,
    pub active_jobs: Vec<ActiveJobSummary>,
    pub active_crons: Vec<ActiveCronSummary>,
    #[serde(default)]
    pub history: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageMessage {
    pub id: Uuid,
    pub content: Option<String>,
    pub is_edit: bool,
    #[serde(default)]
    pub has_audio: bool,
    #[serde(default)]
    pub has_image: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveJobSummary {
    pub id: Uuid,
    pub status: String,
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TriageDecision {
    Reply {
        text: String,
    },
    CreateJob {
        prompt: String,
        kind: String,
    },
    CreateCron {
        name: String,
        schedule: String,
        prompt: String,
    },
    CancelJob {
        job_id: Uuid,
        reason: String,
    },
    CancelCron {
        name: String,
    },
    ResumeJob {
        job_id: Uuid,
        input: String,
    },
    SetSubscription {
        enabled: bool,
    },
    Noop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageBatchDecision {
    pub decisions: Vec<TriageDecision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichInput {
    pub job_id: Uuid,
    pub prompt: String,
    pub history: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichOutput {
    pub enriched_prompt: String,
}

#[async_trait::async_trait]
pub trait AiService: Send + Sync {
    async fn triage_batch(&self, input: TriageBatchInput) -> anyhow::Result<TriageBatchDecision>;
    async fn enrich_job(&self, input: EnrichInput) -> anyhow::Result<EnrichOutput>;
    async fn embed_text(&self, text: &str) -> anyhow::Result<Vec<f32>>;
    async fn rewrite_reply(&self, content: &str, history: &[String]) -> anyhow::Result<String>;
}

pub struct RealAiService {
    triage_client: TriageClient,
    embedding: Arc<EmbeddingService>,
    reply_client: ReplyClient,
}

impl RealAiService {
    pub fn new(embedding: Arc<EmbeddingService>) -> anyhow::Result<Self> {
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .map_err(|_| anyhow::anyhow!("OPENROUTER_API_KEY not set"))?;
        let model = std::env::var("OPENROUTER_MODEL")
            .unwrap_or_else(|_| "moonshotai/kimi-k2.5".to_string());
        let provider_only = std::env::var("OPENROUTER_PROVIDER_ONLY")
            .ok()
            .or_else(|| Some("fireworks".to_string()));
        let provider_order = std::env::var("OPENROUTER_PROVIDER_ORDER")
            .ok()
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let config = TriageClientConfig {
            api_key: api_key.clone(),
            model: model.clone(),
            provider_only: provider_only.clone(),
            provider_order,
        };

        let reply_model = std::env::var("OPENROUTER_REPLY_MODEL").unwrap_or(model);

        Ok(Self {
            triage_client: TriageClient::new(config),
            embedding,
            reply_client: ReplyClient::new(api_key, reply_model, provider_only),
        })
    }
}

#[async_trait::async_trait]
impl AiService for RealAiService {
    async fn triage_batch(&self, input: TriageBatchInput) -> anyhow::Result<TriageBatchDecision> {
        self.triage_client.triage(&input).await
    }

    async fn enrich_job(&self, input: EnrichInput) -> anyhow::Result<EnrichOutput> {
        let mut enriched_prompt = String::from(
            "You are Yui, a capable autonomous agent running in a Docker container. \
             You have full shell access including curl, git, python3, nodejs, npm, and yt-dlp. \
             You CAN and SHOULD use curl/wget to fetch data from APIs, download files, and interact with web services. \
             Always attempt to complete the task using available tools. \
             Save any output files to /workspace/ so they can be sent back to the user.\n\
             To send a GIF: use `curl -L -o /workspace/funny.gif \"https://cataas.com/cat/gif\"`. \
             Any files in /workspace/ are automatically sent back as attachments. \
             For GIF requests, always download an actual GIF file rather than sending a URL.\n\n",
        );
        enriched_prompt.push_str(&input.prompt);
        if !input.history.is_empty() {
            enriched_prompt.push_str("\n\nRelevant history:\n");
            for (i, entry) in input.history.iter().enumerate() {
                enriched_prompt.push_str(&format!("{}. {}\n", i + 1, entry));
            }
        }
        Ok(EnrichOutput { enriched_prompt })
    }

    async fn embed_text(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let text = text.to_string();
        let embedding = self.embedding.clone();
        tokio::task::spawn_blocking(move || embedding.embed(&text))
            .await
            .map_err(|e| anyhow::anyhow!("embedding task failed: {e}"))?
    }

    async fn rewrite_reply(&self, content: &str, history: &[String]) -> anyhow::Result<String> {
        self.reply_client.rewrite(content, history).await
    }
}

