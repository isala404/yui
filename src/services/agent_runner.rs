use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use uuid::Uuid;

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const RUNNER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerStartInput {
    pub job_id: Uuid,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunnerHandle {
    pub run_id: Uuid,
    pub job_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunnerEvent {
    Stdout(String),
    Stderr(String),
    AskUser {
        question: String,
    },
    Completed {
        output: String,
        #[serde(default)]
        attachments: Vec<serde_json::Value>,
    },
    Failed {
        error: String,
    },
}

#[async_trait::async_trait]
pub trait AgentRunnerService: Send + Sync {
    async fn start(&self, input: RunnerStartInput) -> anyhow::Result<RunnerHandle>;
    async fn poll(&self, handle: &RunnerHandle) -> anyhow::Result<Vec<RunnerEvent>>;
    async fn cancel(&self, handle: &RunnerHandle) -> anyhow::Result<()>;
}

pub struct OpenRouterAgentRunner {
    api_key: String,
    model: String,
    provider_only: Option<String>,
    client: reqwest::Client,
}

impl OpenRouterAgentRunner {
    pub fn new(api_key: String, model: String, provider_only: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(RUNNER_TIMEOUT)
            .build()
            .expect("failed to build reqwest client");
        Self {
            api_key,
            model,
            provider_only,
            client,
        }
    }

    pub fn from_env() -> Self {
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .expect("OPENROUTER_API_KEY required for OpenRouter runner");
        let model = std::env::var("OPENROUTER_RUNTIME_MODEL")
            .or_else(|_| std::env::var("OPENROUTER_MODEL"))
            .unwrap_or_else(|_| "moonshotai/kimi-k2.5".to_string());
        let provider_only = std::env::var("OPENROUTER_PROVIDER_ONLY")
            .ok()
            .or_else(|| Some("fireworks".to_string()));
        Self::new(api_key, model, provider_only)
    }
}

#[derive(Clone)]
enum ORRun {
    Pending(String),
    Running,
    Done(ORResult),
}

#[derive(Clone)]
enum ORResult {
    Completed(String),
    AskUser(String),
    Failed(String),
}

static OR_RUNS: std::sync::LazyLock<Mutex<HashMap<Uuid, ORRun>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

const RUNNER_SYSTEM_PROMPT: &str = r#"You are Yui's task execution engine. You receive enriched prompts and produce results.

Respond with a JSON object in one of these formats:
1. Task completed: {"status":"completed","output":"your detailed response here"}
2. Need user input: {"status":"ask_user","question":"your specific question here"}

Rules:
- ONLY output valid JSON, nothing else
- If the task explicitly asks you to ask a clarification question, use "ask_user" status FIRST
- After receiving user input (shown as "User response: ..."), complete the task with "completed" status
- Include any tokens, identifiers, or exact strings mentioned in the task verbatim in your output
- Be thorough but concise
- For real-time data queries (weather, stock prices, ISS location, current time): provide the best answer you can. If your knowledge is outdated, say so clearly.
- The "Relevant history" section contains previous conversation messages. Use them for context, recall questions, and to understand what the user previously discussed.
- Do NOT use markdown formatting in your output. Plain text only, suitable for WhatsApp messages.
- Never mention file paths, container internals, or system details in your output."#;

async fn call_openrouter(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    provider_only: Option<&str>,
    prompt: &str,
) -> ORResult {
    let mut body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": RUNNER_SYSTEM_PROMPT},
            {"role": "user", "content": prompt}
        ],
        "temperature": 0.3,
        "max_tokens": 2048,
        "response_format": {"type": "json_object"}
    });
    if let Some(provider) = provider_only {
        body["provider"] = serde_json::json!({
            "only": [provider]
        });
    }

    let response = match client
        .post(OPENROUTER_URL)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return ORResult::Failed(format!("HTTP error: {e}")),
    };

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return ORResult::Failed(format!("OpenRouter {status}: {body}"));
    }

    let chat_resp: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(e) => return ORResult::Failed(format!("response parse error: {e}")),
    };

    let content = match chat_resp["choices"][0]["message"]["content"].as_str() {
        Some(c) => c,
        None => return ORResult::Failed("no content in LLM response".to_string()),
    };

    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content) {
        match parsed["status"].as_str() {
            Some("ask_user") => {
                let question = parsed["question"]
                    .as_str()
                    .unwrap_or("clarification needed");
                return ORResult::AskUser(question.to_string());
            }
            Some("completed") => {
                let output = parsed["output"].as_str().unwrap_or(content);
                return ORResult::Completed(output.to_string());
            }
            _ => {}
        }
    }

    // fallback: treat raw content as output
    ORResult::Completed(content.to_string())
}

#[async_trait::async_trait]
impl AgentRunnerService for OpenRouterAgentRunner {
    async fn start(&self, input: RunnerStartInput) -> anyhow::Result<RunnerHandle> {
        let handle = RunnerHandle {
            run_id: Uuid::new_v4(),
            job_id: input.job_id,
        };
        OR_RUNS
            .lock()
            .unwrap()
            .insert(handle.run_id, ORRun::Pending(input.prompt));
        Ok(handle)
    }

    async fn poll(&self, handle: &RunnerHandle) -> anyhow::Result<Vec<RunnerEvent>> {
        let state = {
            let runs = OR_RUNS.lock().unwrap();
            runs.get(&handle.run_id).cloned()
        };

        match state {
            Some(ORRun::Pending(prompt)) => {
                {
                    let mut runs = OR_RUNS.lock().unwrap();
                    runs.insert(handle.run_id, ORRun::Running);
                }

                let run_id = handle.run_id;
                let client = self.client.clone();
                let api_key = self.api_key.clone();
                let model = self.model.clone();
                let provider_only = self.provider_only.clone();

                tokio::spawn(async move {
                    let result = call_openrouter(
                        &client,
                        &api_key,
                        &model,
                        provider_only.as_deref(),
                        &prompt,
                    )
                    .await;
                    let mut runs = OR_RUNS.lock().unwrap();
                    runs.insert(run_id, ORRun::Done(result));
                });

                Ok(vec![RunnerEvent::Stdout(
                    "sending prompt to LLM...".to_string(),
                )])
            }
            Some(ORRun::Running) => Ok(vec![]),
            Some(ORRun::Done(result)) => {
                let mut runs = OR_RUNS.lock().unwrap();
                runs.remove(&handle.run_id);
                drop(runs);

                match result {
                    ORResult::Completed(output) => Ok(vec![RunnerEvent::Completed {
                        output,
                        attachments: vec![],
                    }]),
                    ORResult::AskUser(question) => Ok(vec![RunnerEvent::AskUser { question }]),
                    ORResult::Failed(error) => Ok(vec![RunnerEvent::Failed { error }]),
                }
            }
            None => Ok(vec![]),
        }
    }

    async fn cancel(&self, handle: &RunnerHandle) -> anyhow::Result<()> {
        let mut runs = OR_RUNS.lock().unwrap();
        runs.remove(&handle.run_id);
        Ok(())
    }
}

