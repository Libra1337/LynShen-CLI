use serde_json::Value;
use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    sync::{Mutex, OnceLock},
};
use tiktoken_rs::bpe_for_model;

/// Texts at least this long have their count remembered. The engine recounts
/// the whole conversation after every item it gains (the context gauge), and
/// all but the newest items are unchanged: without this a long session spent
/// about 0.1 s of tokenizing per 130k tokens on every tool call.
const MEMO_MIN_BYTES: usize = 512;
const MEMO_MAX_ENTRIES: usize = 16_384;

fn memo() -> &'static Mutex<HashMap<u64, usize>> {
    static MEMO: OnceLock<Mutex<HashMap<u64, usize>>> = OnceLock::new();
    MEMO.get_or_init(Mutex::default)
}

#[derive(Debug, Clone)]
pub(crate) struct TokenCount {
    pub tokens: usize,
    pub tokenizer: String,
}

pub(crate) fn count_text(model: &str, text: &str) -> TokenCount {
    let requested = tokenizer_model(model);
    let (bpe, tokenizer) = bpe_for_model(requested).map_or_else(
        |_| {
            // gpt-5/o-series models use o200k_base. If a future deployment suffix is
            // unknown to tiktoken-rs, falling back to gpt-5 keeps counting tokenizer
            // based instead of reverting to character estimates.
            (
                bpe_for_model("gpt-5").expect("gpt-5 tokenizer must be available"),
                "gpt-5",
            )
        },
        |bpe| (bpe, requested),
    );
    let tokens = if text.len() < MEMO_MIN_BYTES {
        bpe.count_with_special_tokens(text)
    } else {
        let mut hasher = DefaultHasher::new();
        (tokenizer, text).hash(&mut hasher);
        let key = hasher.finish();
        let remembered = memo().lock().ok().and_then(|memo| memo.get(&key).copied());
        remembered.unwrap_or_else(|| {
            let tokens = bpe.count_with_special_tokens(text);
            if let Ok(mut memo) = memo().lock() {
                if memo.len() >= MEMO_MAX_ENTRIES {
                    memo.clear();
                }
                memo.insert(key, tokens);
            }
            tokens
        })
    };
    TokenCount {
        tokens,
        tokenizer: tokenizer.to_string(),
    }
}

pub(crate) fn count_value(model: &str, value: &Value) -> TokenCount {
    count_text(model, &value.to_string())
}

pub(crate) fn count_values<'a>(model: &str, values: impl Iterator<Item = &'a Value>) -> TokenCount {
    let mut tokens = 0usize;
    let mut tokenizer = tokenizer_model(model).to_string();
    for value in values {
        let count = count_value(model, value);
        tokenizer = count.tokenizer;
        tokens += count.tokens;
    }
    TokenCount { tokens, tokenizer }
}

fn tokenizer_model(model: &str) -> &str {
    if model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
        || model.starts_with("codex")
    {
        model
    } else {
        // Anthropic and custom models do not expose a local tokenizer here.
        // Count with o200k_base and show the tokenizer name explicitly in
        // statistics so callers do not confuse it with provider-reported usage.
        "gpt-5"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn counts_text_with_model_tokenizer() {
        let count = count_text("gpt-5", "hello world");

        assert!(count.tokens > 0);
        assert_eq!(count.tokenizer, "gpt-5");
    }

    #[test]
    fn long_texts_count_the_same_from_memory() {
        let text = "fn main() { println!(\"hello\"); }\n".repeat(64);
        let first = count_text("gpt-5", &text).tokens;
        let tokenizer = bpe_for_model("gpt-5").unwrap();
        assert_eq!(first, tokenizer.count_with_special_tokens(&text));
        assert_eq!(count_text("gpt-5", &text).tokens, first);
        // A different text is not mistaken for a remembered one.
        let longer = format!("{text}// one more line\n");
        assert_eq!(
            count_text("gpt-5", &longer).tokens,
            tokenizer.count_with_special_tokens(&longer)
        );
    }

    #[test]
    fn counts_values_without_character_estimates() {
        let value =
            json!({ "role": "user", "content": [{ "type": "input_text", "text": "hello" }] });
        let count = count_value("gpt-5", &value);

        assert!(count.tokens > 0);
    }

    #[test]
    fn reports_fallback_tokenizer_for_custom_models() {
        let count = count_text("claude-sonnet", "hello");

        assert!(count.tokens > 0);
        assert_eq!(count.tokenizer, "gpt-5");
    }
}
