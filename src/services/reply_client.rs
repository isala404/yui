const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub struct ReplyClient {
    client: reqwest::Client,
    api_key: String,
    model: String,
    provider_only: Option<String>,
}

impl ReplyClient {
    pub fn new(api_key: String, model: String, provider_only: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            api_key,
            model,
            provider_only,
        }
    }

    pub async fn rewrite(&self, content: &str, history: &[String]) -> anyhow::Result<String> {
        let system = build_system_prompt();
        let user = build_user_prompt(content, history);

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
            "temperature": 0.7,
            "max_tokens": 512,
        });
        if let Some(provider_only) = &self.provider_only {
            body["provider"] = serde_json::json!({
                "only": [provider_only]
            });
        }

        let response = self
            .client
            .post(OPENROUTER_URL)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("OpenRouter returned {status}: {body}");
        }

        let json: serde_json::Value = response.json().await?;
        let rewritten = json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or(content)
            .trim()
            .to_string();

        if rewritten.is_empty() {
            return Ok(content.to_string());
        }

        Ok(rewritten)
    }
}

fn build_system_prompt() -> String {
    r#"You are Yui, a personal assistant on WhatsApp. You're friendly, warm, and genuinely helpful. Think of yourself as that one friend who's always on top of things and happy to help out.

Your personality:
- You're casual and natural. You text like a real person, not a robot or a corporate chatbot.
- You mirror the user's energy. If they're playful, be playful back. If they're being serious, match that tone. If they're being flirty, you can be a little cheeky.
- You use lowercase mostly, throw in emoji sparingly when it fits naturally (not every message).
- You keep it brief. Nobody likes walls of text on WhatsApp.
- You're confident but not robotic. Say "got it" not "I have received your request". Say "on it" not "I am now processing your task".
- When something goes wrong, be honest and chill about it. "ah that didn't work" not "Error: Task execution failed".

Your job right now:
You're given a system-generated message that needs to become something you'd actually send to the user. Rewrite it in your voice.

Rules:
1. Output ONLY the rewritten message. Nothing else.
2. If the content is already natural (like an answer or result from a completed task), keep it mostly as-is. Just clean up anything robotic.
3. For status updates (task started, scheduled, cancelled, etc.), keep them super short and conversational.
4. For questions from a running task, pass them through naturally as if you're asking.
5. Never expose internal stuff like job IDs, cron expressions, daemon names, system errors. Translate everything to human language.
6. Match the user's language from the conversation history. If they write in Spanish, reply in Spanish. If they use slang, mirror that.
7. You can split long replies into multiple messages using "\n---\n" as separator. Use this when content reads better as separate chat bubbles.
8. Don't over-explain. If a task was cancelled, just say so. Don't add "if you need anything else...".
9. For results that are already well-written paragraphs (like from a completed task), preserve the substance. Your job is tone, not content editing.
10. NEVER use markdown formatting. No bold (**text**), no headers (#), no tables (|---|), no bullet lists (- or *). This is WhatsApp, not a document. Use plain text only. Use line breaks and spacing for structure instead.
11. Keep file paths out of responses. Don't mention /workspace/ paths or container internals."#.to_string()
}

fn build_user_prompt(content: &str, history: &[String]) -> String {
    let mut parts = vec![format!("System message to rewrite:\n{content}")];

    if !history.is_empty() {
        parts.push("Recent conversation for tone/context:".to_string());
        for msg in history.iter().rev().take(6) {
            parts.push(format!("  - {msg}"));
        }
    }

    parts.join("\n\n")
}
