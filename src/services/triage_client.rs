use crate::services::ai::{TriageBatchDecision, TriageBatchInput, TriageDecision};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const MAX_RETRIES: u32 = 2;
const TRIAGE_TOOL_NAME: &str = "triage_decisions";

#[derive(Debug, Clone)]
pub struct TriageClientConfig {
    pub api_key: String,
    pub model: String,
    pub provider_only: Option<String>,
    pub provider_order: Vec<String>,
}

pub struct TriageClient {
    client: reqwest::Client,
    config: TriageClientConfig,
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<ProviderConfig>,
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct ToolDefinition {
    #[serde(rename = "type")]
    kind: String,
    function: ToolFunction,
}

#[derive(Serialize)]
struct ToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Serialize)]
struct ProviderConfig {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    only: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    order: Vec<String>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<UsageInfo>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
    #[serde(default)]
    function_call: Option<FunctionCall>,
}

#[derive(Debug, Clone, Deserialize)]
struct ToolCall {
    _id: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    function: FunctionCall,
}

#[derive(Debug, Clone, Deserialize)]
struct FunctionCall {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct UsageInfo {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    total_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct LlmTriageOutput {
    decisions: Vec<LlmDecision>,
}

#[derive(Debug, Deserialize)]
struct LlmDecision {
    action: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    job_id: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

impl TriageClient {
    pub fn new(config: TriageClientConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("failed to build reqwest client");
        Self { client, config }
    }

    pub async fn triage(&self, input: &TriageBatchInput) -> anyhow::Result<TriageBatchDecision> {
        let force_fallback = std::env::var("YUI_TRIAGE_FORCE_FALLBACK")
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
            .unwrap_or(false);
        if force_fallback {
            return Ok(fallback_decision(input));
        }

        let system_prompt = build_system_prompt();
        let user_prompt = build_user_prompt(input);

        let provider = build_provider_config(&self.config);

        let mut request = ChatRequest {
            model: self.config.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: system_prompt,
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: user_prompt,
                },
            ],
            temperature: 0.1,
            max_tokens: 2048,
            tools: vec![triage_tool_definition()],
            tool_choice: Some(serde_json::json!({
                "type": "function",
                "function": { "name": TRIAGE_TOOL_NAME }
            })),
            provider,
        };

        let mut last_error = None;

        for attempt in 0..=MAX_RETRIES {
            match self.send_request(&request).await {
                Ok(message) => {
                    if let Some(tool_result) = parse_tool_call_result(&message) {
                        if let Some(decisions) = handle_parse_attempt(
                            parse_triage_response(&tool_result),
                            attempt,
                            ParseSource::ToolCall,
                            &mut last_error,
                        ) {
                            return Ok(decisions);
                        }
                        continue;
                    }

                    let raw = match extract_message_payload(&message) {
                        Ok(v) => v,
                        Err(err) => {
                            last_error = Some(err);
                            continue;
                        }
                    };

                    tracing::info!(raw = %raw, "triage LLM raw response");
                    if let Some(decisions) = handle_parse_attempt(
                        parse_triage_response(&raw),
                        attempt,
                        ParseSource::RawPayload,
                        &mut last_error,
                    ) {
                        return Ok(decisions);
                    }
                }
                Err(req_err) if attempt < MAX_RETRIES && is_retryable(&req_err) => {
                    if is_fireworks_rate_limited(&req_err) && request.provider.is_some() {
                        tracing::warn!(
                            "fireworks provider rate-limited; retrying without provider pin"
                        );
                        request.provider = None;
                    }
                    tracing::warn!(attempt, error = %req_err, "triage request failed, retrying");
                    let backoff = std::time::Duration::from_millis(500 * 2u64.pow(attempt));
                    tokio::time::sleep(backoff).await;
                    last_error = Some(req_err);
                }
                Err(req_err) => {
                    last_error = Some(req_err);
                    break;
                }
            }
        }

        // deterministic fallback: create a single action job with raw user text
        tracing::error!(
            error = ?last_error,
            "triage LLM failed after retries, using fallback"
        );
        Ok(fallback_decision(input))
    }

    async fn send_request(&self, request: &ChatRequest) -> anyhow::Result<ChoiceMessage> {
        let response = self
            .client
            .post(OPENROUTER_URL)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("OpenRouter returned {status}: {body}");
        }

        let body = response.text().await?;
        let body_json: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            anyhow::anyhow!("failed to parse OpenRouter response: {e}\nraw: {body}")
        })?;

        if let Some(err) = body_json.get("error") {
            let code = err.get("code").and_then(serde_json::Value::as_i64);
            let msg = err
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown provider error");
            let provider_name = err
                .get("metadata")
                .and_then(|m| m.get("provider_name"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            anyhow::bail!("OpenRouter provider error {code:?} from {provider_name}: {msg}");
        }

        let chat_response: ChatResponse = serde_json::from_value(body_json).map_err(|e| {
            anyhow::anyhow!("failed to parse OpenRouter response payload: {e}\nraw: {body}")
        })?;
        if let Some(usage) = &chat_response.usage {
            tracing::debug!(
                prompt_tokens = usage.prompt_tokens,
                completion_tokens = usage.completion_tokens,
                total_tokens = usage.total_tokens,
                "triage token usage"
            );
        }

        let first = chat_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no choices in LLM response"))?;

        Ok(first.message)
    }
}

#[derive(Clone, Copy)]
enum ParseSource {
    ToolCall,
    RawPayload,
}

fn handle_parse_attempt(
    parse_result: anyhow::Result<TriageBatchDecision>,
    attempt: u32,
    source: ParseSource,
    last_error: &mut Option<anyhow::Error>,
) -> Option<TriageBatchDecision> {
    let source_label = match source {
        ParseSource::ToolCall => "tool_call",
        ParseSource::RawPayload => "raw_payload",
    };

    match parse_result {
        Ok(decisions) => {
            tracing::info!(
                attempt,
                decision_count = decisions.decisions.len(),
                source = source_label,
                "triage LLM responded"
            );
            Some(decisions)
        }
        Err(parse_err) if attempt < MAX_RETRIES => {
            tracing::warn!(
                attempt,
                error = %parse_err,
                source = source_label,
                "triage parse failed, retrying"
            );
            *last_error = Some(parse_err);
            None
        }
        Err(parse_err) => {
            *last_error = Some(parse_err);
            None
        }
    }
}

fn is_retryable(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    msg.contains("429")
        || msg.contains("500")
        || msg.contains("502")
        || msg.contains("503")
        || msg.contains("timeout")
        || msg.contains("connection")
        || msg.contains("missing field")
        || msg.contains("failed to parse")
}

fn is_fireworks_rate_limited(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("fireworks") && (msg.contains("429") || msg.contains("rate-limited"))
}

fn triage_tool_definition() -> ToolDefinition {
    ToolDefinition {
        kind: "function".to_string(),
        function: ToolFunction {
            name: TRIAGE_TOOL_NAME.to_string(),
            description: "Return triage routing decisions for the current WhatsApp message batch."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "decisions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "action": {
                                    "type": "string",
                                    "enum": [
                                        "reply",
                                        "create_job",
                                        "create_cron",
                                        "cancel_job",
                                        "cancel_cron",
                                        "resume_job",
                                        "set_subscription",
                                        "noop"
                                    ]
                                },
                                "text": { "type": "string" },
                                "prompt": { "type": "string" },
                                "kind": { "type": "string" },
                                "name": { "type": "string" },
                                "schedule": { "type": "string" },
                                "job_id": { "type": "string" },
                                "reason": { "type": "string" },
                                "input": { "type": "string" },
                                "enabled": { "type": "boolean" }
                            },
                            "required": ["action"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["decisions"],
                "additionalProperties": false
            }),
        },
    }
}

fn build_provider_config(config: &TriageClientConfig) -> Option<ProviderConfig> {
    if let Some(provider_only) = &config.provider_only {
        return Some(ProviderConfig {
            only: vec![provider_only.clone()],
            order: vec![],
        });
    }

    if config.provider_order.is_empty() {
        None
    } else {
        Some(ProviderConfig {
            only: vec![],
            order: config.provider_order.clone(),
        })
    }
}

fn extract_message_payload(message: &ChoiceMessage) -> anyhow::Result<String> {
    message
        .content
        .clone()
        .filter(|content| !content.trim().is_empty())
        .or_else(|| message.reasoning.clone())
        .ok_or_else(|| anyhow::anyhow!("no content or tool call in LLM response"))
}

fn parse_tool_call_result(message: &ChoiceMessage) -> Option<String> {
    message
        .tool_calls
        .iter()
        .find(|call| call.kind == "function" && call.function.name == TRIAGE_TOOL_NAME)
        .map(|call| call.function.arguments.clone())
        .or_else(|| {
            message
                .function_call
                .as_ref()
                .filter(|f| f.name == TRIAGE_TOOL_NAME)
                .map(|f| f.arguments.clone())
        })
}

fn build_system_prompt() -> String {
    r#"You are Yui's triage engine. Given a batch of WhatsApp messages and active job context, call the `triage_decisions` function exactly once.

Each decision must be one of:
- {"action":"reply","text":"..."} - send a chat reply directly
- {"action":"create_job","prompt":"...","kind":"action"} - create a new background task
- {"action":"create_cron","name":"short_name","schedule":"cron_expr","prompt":"..."} - schedule recurring task
- {"action":"cancel_job","job_id":"uuid","reason":"..."} - cancel an active job
- {"action":"cancel_cron","name":"..."} - cancel a scheduled task
- {"action":"resume_job","job_id":"uuid","input":"..."} - resume a paused job with user input
- {"action":"set_subscription","enabled":true|false} - toggle subscription
- {"action":"noop"} - do nothing

Rules:
1. Use the function call arguments only. No markdown, no explanations.
2. CRITICAL: If there is a paused job and the user sends ANY message that is NOT explicitly asking to cancel, ALWAYS resume that paused job with the user's message as input.
3. If a message says "cancel" or "stop" and there's one active/running job, cancel it. If multiple, ask which one via reply.
4. REPLY DIRECTLY ONLY for: greetings, small talk, pure arithmetic (2+2, 5*7), yes/no questions, format-constrained replies (user says "reply with X"), and requests to remember/store something ("remember this token: ABC").
5. CREATE JOB for: ANY task that needs real-time data (weather, stock prices, current time, ISS location), web research, writing code, file operations, downloads, analysis, or multi-step work. When in doubt, CREATE JOB instead of replying. The job executor has internet access and tools, you do not.
6. CRITICAL SCHEDULING RULE: ANY request involving repeated/periodic/recurring execution MUST use create_cron, NEVER create_job. Keywords that REQUIRE create_cron: "every", "each", "per minute/hour/day", "daily", "weekly", "monthly", "repeat", "recurring", "schedule", "for N minutes/hours". The cron prompt MUST describe the actual task to perform each time. If the user specifies a duration or count (e.g. "for 5 minutes", "3 times"), append AUTO_STOP_AFTER=N to the cron prompt where N is the number of executions. Convert to cron expressions: "every minute" = "* * * * *", "every hour" = "0 * * * *", "every day at 9am" = "0 9 * * *", "every Monday" = "0 9 * * 1". Name should be short snake_case.
7. If the user wants to unsubscribe/subscribe, use set_subscription.
8. CANCEL CRON: When cancelling a cron, use the EXACT name from the "Active crons" list. Match user intent to the closest cron name.
9. CONTEXT RECALL: If the user asks "what did I say" or "what was the token" or similar recall questions, look at the conversation history provided and reply directly with the exact information. The history section contains previous messages for this chat.
10. ATTACHMENTS: If a message has [audio] marker, the user sent a voice note. Create an action job with prompt that mentions transcribing the audio and executing any tasks mentioned. If a message has [image] marker, create an action job for image analysis.

EXAMPLES of correct routing:
- "iss location every minute for 5 mins" -> create_cron name="iss_location" schedule="* * * * *" prompt="Get the current ISS location using the API at http://api.open-notify.org/iss-now.json and report latitude, longitude, and UTC timestamp AUTO_STOP_AFTER=5"
- "remind me to drink water every hour" -> create_cron schedule="0 * * * *" prompt="Send a reminder to drink water"
- "tell me weather in new york" -> create_job (needs real-time data, use web API)
- "what time is it" -> create_job (needs current time from system)
- "clone this repo and count lines" -> create_job
- "what's 2+2" -> reply "4"
- "remember this token: ALPHA-991" -> reply "got it, saved ALPHA-991"
- "what token did i ask you to remember?" -> reply with the token from conversation history"#.to_string()
}

fn build_user_prompt(input: &TriageBatchInput) -> String {
    let mut parts = vec![format!("Chat: {}", input.chat_id)];

    if !input.history.is_empty() {
        parts.push("Conversation history (most recent first):".to_string());
        for (i, msg) in input.history.iter().enumerate().take(15) {
            parts.push(format!("  {}. {}", i + 1, msg));
        }
    }

    if !input.active_jobs.is_empty() {
        parts.push("Active jobs:".to_string());
        for job in &input.active_jobs {
            let prompt_preview = job
                .prompt
                .as_deref()
                .map(|p| p.chars().take(80).collect::<String>())
                .unwrap_or_default();
            parts.push(format!(
                "  - {} [{}]: {}",
                job.id, job.status, prompt_preview
            ));
        }
    }

    if !input.active_crons.is_empty() {
        parts.push("Active crons:".to_string());
        for cron in &input.active_crons {
            parts.push(format!(
                "  - name=\"{}\" schedule=\"{}\" prompt=\"{}\"",
                cron.name, cron.schedule, cron.prompt
            ));
        }
    }

    parts.push("Messages:".to_string());
    for msg in &input.messages {
        let content = msg.content.as_deref().unwrap_or("[no text]");
        let edit_marker = if msg.is_edit { " (edited)" } else { "" };
        let audio_marker = if msg.has_audio { " [audio]" } else { "" };
        let image_marker = if msg.has_image { " [image]" } else { "" };
        parts.push(format!(
            "  - [{}{}{}{}]: {}",
            msg.id, edit_marker, audio_marker, image_marker, content
        ));
    }

    parts.join("\n")
}

fn parse_triage_response(content: &str) -> anyhow::Result<TriageBatchDecision> {
    let output: LlmTriageOutput = serde_json::from_str(content)
        .map_err(|e| anyhow::anyhow!("failed to parse triage JSON: {e}\nraw: {content}"))?;

    let decisions = output
        .decisions
        .into_iter()
        .filter_map(|d| convert_decision(d).ok())
        .collect();

    Ok(TriageBatchDecision { decisions })
}

fn convert_decision(d: LlmDecision) -> anyhow::Result<TriageDecision> {
    match d.action.as_str() {
        "reply" => Ok(TriageDecision::Reply {
            text: d.text.unwrap_or_default(),
        }),
        "create_job" => Ok(TriageDecision::CreateJob {
            prompt: d.prompt.unwrap_or_default(),
            kind: d.kind.unwrap_or_else(|| "action".to_string()),
        }),
        "create_cron" => Ok(TriageDecision::CreateCron {
            name: d
                .name
                .unwrap_or_else(|| format!("cron_{}", Uuid::new_v4().as_simple())),
            schedule: d.schedule.unwrap_or_default(),
            prompt: d.prompt.unwrap_or_default(),
        }),
        "cancel_job" => Ok(TriageDecision::CancelJob {
            job_id: parse_job_id_or_new(d.job_id.as_deref()),
            reason: d.reason.unwrap_or_else(|| "user requested".to_string()),
        }),
        "cancel_cron" => Ok(TriageDecision::CancelCron {
            name: d.name.unwrap_or_default(),
        }),
        "resume_job" => Ok(TriageDecision::ResumeJob {
            job_id: parse_job_id_or_new(d.job_id.as_deref()),
            input: d.input.unwrap_or_default(),
        }),
        "set_subscription" => Ok(TriageDecision::SetSubscription {
            enabled: d.enabled.unwrap_or(true),
        }),
        "noop" => Ok(TriageDecision::Noop),
        other => anyhow::bail!("unknown action: {other}"),
    }
}

fn parse_job_id_or_new(job_id: Option<&str>) -> Uuid {
    job_id
        .and_then(|value| value.parse::<Uuid>().ok())
        .unwrap_or_else(Uuid::new_v4)
}

/// Deterministic fallback when LLM triage fails after retries.
/// Creates a single action job with raw user text so nothing gets dropped.
fn fallback_decision(input: &TriageBatchInput) -> TriageBatchDecision {
    let combined_text: String = input
        .messages
        .iter()
        .filter_map(|m| m.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n");

    if combined_text.trim().is_empty() {
        return TriageBatchDecision {
            decisions: vec![TriageDecision::Noop],
        };
    }

    tracing::warn!("triage fallback: creating action job from raw user text");
    TriageBatchDecision {
        decisions: vec![TriageDecision::CreateJob {
            prompt: combined_text,
            kind: "action".to_string(),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::ai::{ActiveJobSummary, TriageMessage};

    #[test]
    fn parses_valid_triage_response() {
        let json = r#"{"decisions":[{"action":"reply","text":"hello"},{"action":"create_job","prompt":"do something","kind":"action"}]}"#;
        let result = parse_triage_response(json).unwrap();
        assert_eq!(result.decisions.len(), 2);
        assert!(matches!(&result.decisions[0], TriageDecision::Reply { text } if text == "hello"));
        assert!(
            matches!(&result.decisions[1], TriageDecision::CreateJob { prompt, .. } if prompt == "do something")
        );
    }

    #[test]
    fn handles_malformed_json() {
        assert!(parse_triage_response("not json").is_err());
    }

    #[test]
    fn skips_unknown_actions() {
        let json = r#"{"decisions":[{"action":"unknown_thing"},{"action":"reply","text":"ok"}]}"#;
        let result = parse_triage_response(json).unwrap();
        assert_eq!(result.decisions.len(), 1);
    }

    #[test]
    fn fallback_creates_job_from_messages() {
        let input = TriageBatchInput {
            chat_id: "chat".to_string(),
            messages: vec![TriageMessage {
                id: Uuid::new_v4(),
                content: Some("do this thing".to_string()),
                is_edit: false,
                has_audio: false,
                has_image: false,
            }],
            active_jobs: vec![],
            active_crons: vec![],
            history: vec![],
        };
        let result = fallback_decision(&input);
        assert_eq!(result.decisions.len(), 1);
        assert!(
            matches!(&result.decisions[0], TriageDecision::CreateJob { prompt, .. } if prompt == "do this thing")
        );
    }

    #[test]
    fn fallback_returns_noop_for_empty_messages() {
        let input = TriageBatchInput {
            chat_id: "chat".to_string(),
            messages: vec![TriageMessage {
                id: Uuid::new_v4(),
                content: None,
                is_edit: false,
                has_audio: false,
                has_image: false,
            }],
            active_jobs: vec![],
            active_crons: vec![],
            history: vec![],
        };
        let result = fallback_decision(&input);
        assert!(matches!(&result.decisions[0], TriageDecision::Noop));
    }

    #[test]
    fn builds_user_prompt_with_jobs_and_messages() {
        let input = TriageBatchInput {
            chat_id: "test_chat".to_string(),
            messages: vec![TriageMessage {
                id: Uuid::new_v4(),
                content: Some("hello".to_string()),
                is_edit: false,
                has_audio: false,
                has_image: false,
            }],
            active_jobs: vec![ActiveJobSummary {
                id: Uuid::new_v4(),
                status: "running".to_string(),
                prompt: Some("existing task".to_string()),
            }],
            active_crons: vec![],
            history: vec![],
        };
        let prompt = build_user_prompt(&input);
        assert!(prompt.contains("test_chat"));
        assert!(prompt.contains("running"));
        assert!(prompt.contains("hello"));
    }

    #[test]
    fn parses_tool_call_arguments() {
        let message = ChoiceMessage {
            content: None,
            reasoning: None,
            tool_calls: vec![ToolCall {
                _id: Some("call_1".to_string()),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: TRIAGE_TOOL_NAME.to_string(),
                    arguments: r#"{"decisions":[{"action":"reply","text":"ok"}]}"#.to_string(),
                },
            }],
            function_call: None,
        };

        let args = parse_tool_call_result(&message).expect("expected tool call args");
        let parsed = parse_triage_response(&args).expect("expected parsed tool args");
        assert!(matches!(
            &parsed.decisions[0],
            TriageDecision::Reply { text } if text == "ok"
        ));
    }

    #[test]
    fn extract_message_payload_prefers_content_then_reasoning() {
        let with_content = ChoiceMessage {
            content: Some("hello".to_string()),
            reasoning: Some("fallback".to_string()),
            tool_calls: vec![],
            function_call: None,
        };
        assert_eq!(extract_message_payload(&with_content).unwrap(), "hello");

        let with_reasoning = ChoiceMessage {
            content: Some("   ".to_string()),
            reasoning: Some("fallback".to_string()),
            tool_calls: vec![],
            function_call: None,
        };
        assert_eq!(
            extract_message_payload(&with_reasoning).unwrap(),
            "fallback"
        );
    }

    #[test]
    fn build_provider_config_prefers_only_over_order() {
        let config = TriageClientConfig {
            api_key: "key".to_string(),
            model: "model".to_string(),
            provider_only: Some("fireworks".to_string()),
            provider_order: vec!["openai".to_string(), "anthropic".to_string()],
        };

        let provider = build_provider_config(&config).expect("expected provider");
        assert_eq!(provider.only, vec!["fireworks".to_string()]);
        assert!(provider.order.is_empty());
    }

    #[test]
    fn handle_parse_attempt_records_retryable_error() {
        let mut last_error = None;
        let result = handle_parse_attempt(
            parse_triage_response("not json"),
            0,
            ParseSource::RawPayload,
            &mut last_error,
        );

        assert!(result.is_none());
        assert!(last_error.is_some());
    }

    #[test]
    fn handle_parse_attempt_returns_decisions_on_success() {
        let mut last_error = None;
        let result = handle_parse_attempt(
            parse_triage_response(r#"{"decisions":[{"action":"noop"}]}"#),
            0,
            ParseSource::ToolCall,
            &mut last_error,
        );

        assert!(result.is_some());
        assert!(last_error.is_none());
    }
}
