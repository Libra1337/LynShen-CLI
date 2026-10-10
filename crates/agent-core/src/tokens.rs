use base64::Engine;
use serde_json::Value;
use std::{
    borrow::Cow,
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tiktoken_rs::{
    tokenizer::{get_tokenizer, Tokenizer},
    CoreBPE,
};

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

/// The tokenizers loaded, with when each was last used. One holds about
/// 40 MB: a host that runs for days (the daemon) lets go of those it has not
/// used for a while (`release_idle_tokenizers`), and the next count loads
/// them again.
type Loaded = Vec<(Tokenizer, Arc<CoreBPE>, Instant)>;

fn loaded() -> &'static Mutex<Loaded> {
    static LOADED: OnceLock<Mutex<Loaded>> = OnceLock::new();
    LOADED.get_or_init(Mutex::default)
}

fn bpe_for_model(model: &str) -> Option<Arc<CoreBPE>> {
    let tokenizer = get_tokenizer(model)?;
    let mut loaded = loaded().lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, bpe, used)) = loaded.iter_mut().find(|(t, _, _)| *t == tokenizer) {
        *used = Instant::now();
        return Some(Arc::clone(bpe));
    }
    let bpe = Arc::new(
        match tokenizer {
            Tokenizer::O200kHarmony => tiktoken_rs::o200k_harmony(),
            Tokenizer::O200kBase => tiktoken_rs::o200k_base(),
            Tokenizer::Cl100kBase => tiktoken_rs::cl100k_base(),
            Tokenizer::P50kBase => tiktoken_rs::p50k_base(),
            Tokenizer::P50kEdit => tiktoken_rs::p50k_edit(),
            Tokenizer::R50kBase | Tokenizer::Gpt2 => tiktoken_rs::r50k_base(),
        }
        .ok()?,
    );
    loaded.push((tokenizer, Arc::clone(&bpe), Instant::now()));
    Some(bpe)
}

/// Lets go of the tokenizers not used for `idle` (a count in progress
/// keeps its own until it is done).
pub fn release_idle_tokenizers(idle: Duration) {
    loaded()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(_, _, used)| used.elapsed() < idle);
}

#[derive(Debug, Clone)]
pub(crate) struct TokenCount {
    pub tokens: usize,
    pub tokenizer: String,
}

/// Tokens a provider charges for an image whose size cannot be read: about
/// what Anthropic charges for a 1.15-megapixel image (OpenAI's high-detail
/// tiles land close to it).
const IMAGE_TOKENS_DEFAULT: usize = 1_600;
/// Pixels per token, and the image area providers scale down to.
const IMAGE_PIXELS_PER_TOKEN: usize = 750;
const IMAGE_MAX_PIXELS: usize = 1_150_000;
/// Enough of an image's base64 to reach its size in a JPEG behind an EXIF
/// block.
const IMAGE_HEADER_BASE64: usize = 87_384;

/// `text` without the base64 of its inline images (`data:image/…;base64,…`),
/// and what those images cost as tokens. A provider counts an image by its
/// size, not by its base64: a screenshot is about 1,600 tokens, while its
/// base64 tokenized as text is hundreds of thousands, which once filled the
/// context gauge past 100% and set off compaction for nothing.
fn without_inline_images(text: &str) -> (Cow<'_, str>, usize) {
    const MARK: &str = "data:image/";
    if !text.contains(MARK) {
        return (Cow::Borrowed(text), 0);
    }
    let mut out = String::with_capacity(text.len().min(1 << 16));
    let mut tokens = 0;
    let mut rest = text;
    while let Some(at) = rest.find(MARK) {
        let after_mark = &rest[at..];
        let Some(comma) = after_mark
            .find(";base64,")
            .filter(|comma| *comma < 64)
            .map(|comma| comma + ";base64,".len())
        else {
            out.push_str(&rest[..at + MARK.len()]);
            rest = &rest[at + MARK.len()..];
            continue;
        };
        out.push_str(&rest[..at + comma]);
        let data = &after_mark[comma..];
        let end = data
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')))
            .unwrap_or(data.len());
        if end > 0 {
            tokens += image_tokens(&data[..end]);
        }
        rest = &data[end..];
    }
    out.push_str(rest);
    (Cow::Owned(out), tokens)
}

/// What a provider charges for the image in `base64`: its area over 750
/// pixels, after scaling it down to 1.15 megapixels; the default when its
/// size cannot be read.
fn image_tokens(base64: &str) -> usize {
    let head = &base64[..base64.len().min(IMAGE_HEADER_BASE64) / 4 * 4];
    let Some((width, height)) = base64::engine::general_purpose::STANDARD
        .decode(head)
        .ok()
        .and_then(|bytes| image_size(&bytes))
    else {
        return IMAGE_TOKENS_DEFAULT;
    };
    let pixels = (width * height).clamp(1, IMAGE_MAX_PIXELS);
    pixels.div_ceil(IMAGE_PIXELS_PER_TOKEN)
}

/// Width and height from a PNG, GIF or JPEG header.
fn image_size(bytes: &[u8]) -> Option<(usize, usize)> {
    let be = |at: usize| Some(u16::from_be_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]) as usize);
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        let word = |at: usize| -> Option<usize> {
            Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize)
        };
        return Some((word(16)?, word(20)?));
    }
    if bytes.starts_with(b"GIF8") {
        let le =
            |at: usize| Some(u16::from_le_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]) as usize);
        return Some((le(6)?, le(8)?));
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        // Walk the JPEG segments to a start-of-frame marker.
        let mut at = 2;
        while at + 9 < bytes.len() {
            if bytes[at] != 0xFF {
                return None;
            }
            let marker = bytes[at + 1];
            let length = be(at + 2)?;
            if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                return Some((be(at + 7)?, be(at + 5)?));
            }
            at += 2 + length;
        }
    }
    None
}

pub(crate) fn count_text(model: &str, text: &str) -> TokenCount {
    let (text, image_tokens) = without_inline_images(text);
    let text = text.as_ref();
    let requested = tokenizer_model(model);
    let (bpe, tokenizer) = bpe_for_model(requested).map_or_else(
        || {
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
        tokens: tokens + image_tokens,
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
    fn a_tokenizer_let_go_of_is_loaded_again_by_the_next_count() {
        let before = count_text("gpt-5", "hello tokenizer world").tokens;
        release_idle_tokenizers(Duration::ZERO);
        assert_eq!(count_text("gpt-5", "hello tokenizer world").tokens, before);
        assert!(loaded()
            .lock()
            .unwrap()
            .iter()
            .any(|(tokenizer, _, _)| *tokenizer == Tokenizer::O200kBase));
    }

    fn png(width: u32, height: u32, body: usize) -> String {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend(width.to_be_bytes());
        bytes.extend(height.to_be_bytes());
        bytes.extend(std::iter::repeat_n(7u8, body));
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn an_inline_image_counts_by_its_size_not_its_base64() {
        // A 2 MB screenshot in a user message, as the request carries it.
        let item = json!({
            "type": "message",
            "role": "user",
            "content": [
                { "type": "input_text", "text": "what is wrong here?" },
                { "type": "input_image", "image_url": format!("data:image/png;base64,{}", png(1920, 1080, 2_000_000)) }
            ]
        });
        let count = count_value("gpt-5", &item).tokens;
        // 1920x1080 scales to 1.15 megapixels: 1,534 tokens, plus the text.
        assert!((1_534..1_600).contains(&count), "{count}");
        // A small image counts less; one whose size cannot be read, the default.
        let small = format!("data:image/png;base64,{}", png(400, 300, 10));
        assert_eq!(
            count_text("gpt-5", &small).tokens,
            160 + count_text("gpt-5", "data:image/png;base64,").tokens
        );
        let unknown = "data:image/webp;base64,AAAA";
        assert_eq!(
            count_text("gpt-5", unknown).tokens,
            IMAGE_TOKENS_DEFAULT + count_text("gpt-5", "data:image/webp;base64,").tokens
        );
        // Text around the image still counts; a mention of data:image/ alone is text.
        assert_eq!(
            count_text("gpt-5", "see data:image/ docs").tokens,
            bpe_for_model("gpt-5")
                .unwrap()
                .count_with_special_tokens("see data:image/ docs")
        );
    }

    #[test]
    fn image_sizes_come_from_png_gif_and_jpeg_headers() {
        use base64::Engine;
        let bytes = |b64: String| {
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap()
        };
        assert_eq!(image_size(&bytes(png(640, 480, 0))), Some((640, 480)));
        assert_eq!(image_size(b"GIF89a\x40\x01\xf0\x00rest"), Some((320, 240)));
        // SOI, an APP0 segment, then SOF0 with height 600 and width 800.
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        jpeg.extend([0u8; 14]);
        jpeg.extend([0xFF, 0xC0, 0x00, 0x11, 0x08, 0x02, 0x58, 0x03, 0x20, 0x03]);
        jpeg.extend([0u8; 12]);
        assert_eq!(image_size(&jpeg), Some((800, 600)));
        assert_eq!(image_size(b"not an image"), None);
    }

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
