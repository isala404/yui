use std::sync::Mutex;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

pub struct EmbeddingService {
    model: Mutex<TextEmbedding>,
}

impl EmbeddingService {
    pub fn new() -> anyhow::Result<Self> {
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::EmbeddingGemma300M).with_show_download_progress(true),
        )?;
        Ok(Self {
            model: Mutex::new(model),
        })
    }

    pub fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let truncated = truncate_input(text, 2048);
        let mut model = self
            .model
            .lock()
            .map_err(|e| anyhow::anyhow!("embedding model lock poisoned: {e}"))?;
        let results = model.embed(vec![truncated], None)?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embedding returned empty results"))
    }

}

fn truncate_input(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        text.to_string()
    } else {
        text.chars().take(max_chars).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_long_input() {
        let long = "a".repeat(3000);
        let result = truncate_input(&long, 2048);
        assert_eq!(result.len(), 2048);
    }

    #[test]
    fn keeps_short_input() {
        let short = "hello world";
        let result = truncate_input(short, 2048);
        assert_eq!(result, short);
    }
}
