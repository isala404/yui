use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const MEDIA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub struct MediaPreprocessor {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl MediaPreprocessor {
    pub fn from_env() -> Self {
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .expect("OPENROUTER_API_KEY required for media preprocessor");
        let model = std::env::var("OPENROUTER_MEDIA_MODEL")
            .unwrap_or_else(|_| "google/gemini-2.5-flash".to_string());
        let client = reqwest::Client::builder()
            .timeout(MEDIA_TIMEOUT)
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            api_key,
            model,
        }
    }

    async fn chat_completion(&self, body: serde_json::Value) -> anyhow::Result<String> {
        let resp = self
            .client
            .post(OPENROUTER_URL)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let json: serde_json::Value = resp.json().await?;

        if !status.is_success() {
            let err_msg = json["error"]["message"].as_str().unwrap_or("unknown error");
            anyhow::bail!("OpenRouter media API error ({}): {}", status, err_msg);
        }

        let text = json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();

        if text.is_empty() {
            anyhow::bail!("empty response from media model");
        }

        Ok(text)
    }

    pub async fn transcribe_audio(&self, path: &str) -> anyhow::Result<String> {
        let bytes = tokio::fs::read(path).await?;
        let encoded = BASE64.encode(&bytes);

        // WhatsApp voice notes are always ogg/opus
        let format = match path.rsplit('.').next() {
            Some("mp3") => "mp3",
            Some("wav") => "wav",
            Some("m4a" | "aac") => "m4a",
            _ => "ogg",
        };

        self.chat_completion(serde_json::json!({
            "model": self.model,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Transcribe this audio exactly. Output only the transcription, nothing else."},
                    {"type": "input_audio", "input_audio": {"data": encoded, "format": format}}
                ]
            }],
            "temperature": 0.0,
            "max_tokens": 2048
        }))
        .await
    }

    pub async fn describe_image(&self, path: &str, user_prompt: &str) -> anyhow::Result<String> {
        let bytes = tokio::fs::read(path).await?;
        let encoded = BASE64.encode(&bytes);

        let mime = match path.rsplit('.').next() {
            Some("png") => "image/png",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            _ => "image/jpeg",
        };

        let instruction = if user_prompt.is_empty() {
            "Describe this image in detail.".to_string()
        } else {
            format!(
                "The user sent this image with the message: \"{user_prompt}\". Answer their question about the image. If the message doesn't ask a specific question, describe the image in detail."
            )
        };

        let data_uri = format!("data:{mime};base64,{encoded}");

        self.chat_completion(serde_json::json!({
            "model": self.model,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": instruction},
                    {"type": "image_url", "image_url": {"url": data_uri}}
                ]
            }],
            "temperature": 0.3,
            "max_tokens": 2048
        }))
        .await
    }
}
