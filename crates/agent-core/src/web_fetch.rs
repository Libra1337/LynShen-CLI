//! `web_fetch` tool: GET a single http(s) URL and return readable text.
//! HTML is reduced with a small hand-written extractor; the response body is
//! capped at 2 MB and the generic model-output projection in tools.rs handles
//! truncation plus saving the full result under .lynshen/truncated-results.
//!
//! Every HTTP response is a result, not an error: a 404 from an existence
//! check is the answer. Only a transport failure (DNS, connect, TLS, timeout)
//! is an error, and it is retried once.

use serde_json::{json, Value};
use std::io::Read;
use std::time::Duration;

/// The smallest read cap a call may set (see `run`).
const MIN_FETCH_BYTES: u64 = 32 * 1024;
const MAX_FETCH_BYTES: u64 = 2 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: u32 = 5;
/// Pause before the single retry of a transport failure.
const RETRY_BACKOFF: Duration = Duration::from_millis(600);

pub fn definition() -> Value {
    json!({
        "type": "function",
        "name": "web_fetch",
        "description": "GET an http(s) URL and return its `status` and readable text (HTML converted, links as `text (url)`). Every response is a result: a 404 or 403 is the server's answer, not a failed call. A long body is truncated and saved in full to a file whose path is returned. Not a search engine.",
        "parameters": {
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "max_bytes": { "type": "number", "description": "Read cap, 32 KB up to the 2 MB default." },
                "raw": { "type": "boolean", "description": "Skip the HTML-to-text conversion." }
            },
            "required": ["url"]
        }
    })
}

pub fn run(args: &Value) -> Value {
    let Some(url) = args.get("url").and_then(Value::as_str) else {
        return json!({ "error": "missing url" });
    };
    if let Err(error) = validate_url(url) {
        return json!({ "url": url, "error": error });
    }
    let max_bytes = read_cap(args);
    let raw = args.get("raw").and_then(Value::as_bool).unwrap_or_default();
    fetch(url, max_bytes, raw)
}

/// The bytes to read: the call's `max_bytes` within 32 KB and 2 MB. A cap of
/// a few hundred bytes (models asked for 300 in 111 of 162 fetches) reads
/// only a page's <head> or a JSON prefix, and the model fetches the same page
/// again and again.
fn read_cap(args: &Value) -> u64 {
    args.get("max_bytes")
        .and_then(Value::as_u64)
        .map(|value| value.clamp(MIN_FETCH_BYTES, MAX_FETCH_BYTES))
        .unwrap_or(MAX_FETCH_BYTES)
}

/// GETs `url` once. `Err` is a transport failure only; an HTTP status, any
/// status, comes back as a result.
fn fetch_once(url: &str, max_bytes: u64, raw: bool) -> Result<Value, String> {
    let agent = agent_for(url);
    let host = url_host(url);
    let response = match agent.get(url).call() {
        Ok(response) => response,
        // A status the server chose is an answer about the resource.
        Err(ureq::Error::Status(_, response)) => response,
        Err(ureq::Error::Transport(transport)) => {
            let message = transport_error_message(&transport);
            crate::log_warn!("web_fetch", "transport error", host = host, error = message);
            return Err(message);
        }
    };
    let status = response.status();
    let status_text = response.status_text().to_string();
    let final_url = response.get_url().to_string();
    let content_type = response
        .header("content-type")
        .unwrap_or_default()
        .to_string();
    let (body, network_truncated) = match read_capped(response.into_reader(), max_bytes) {
        Ok(read) => read,
        Err(error) => {
            crate::log_warn!("web_fetch", "read failed", host = host, error = error);
            return Err(format!("failed to read response body: {error}"));
        }
    };
    crate::log_info!(
        "web_fetch",
        "fetched",
        host = host,
        status = status,
        bytes = body.len(),
    );
    Ok(process_response(
        &final_url,
        status,
        &status_text,
        &content_type,
        &body,
        raw,
        network_truncated,
    ))
}

/// A transport failure is usually transient (a dropped TLS handshake, a slow
/// DNS answer), so the request is sent once more before it is reported.
fn fetch(url: &str, max_bytes: u64, raw: bool) -> Value {
    match fetch_once(url, max_bytes, raw) {
        Ok(value) => value,
        Err(first) => {
            std::thread::sleep(RETRY_BACKOFF);
            match fetch_once(url, max_bytes, raw) {
                Ok(mut value) => {
                    value["note"] = json!(format!("the first attempt failed ({first}); retried"));
                    value
                }
                Err(second) => json!({
                    "url": url,
                    "error": second,
                    "note": "the request was sent twice and failed both times",
                }),
            }
        }
    }
}

/// GETs `url` as text for another tool in this crate (the local web_search
/// engine), through the same proxy and timeouts as web_fetch. `Err` carries
/// a transport failure or a non-2xx status.
pub(crate) fn get_text(url: &str, user_agent: &str, max_bytes: u64) -> Result<String, String> {
    let response = agent_for(url)
        .get(url)
        .set("User-Agent", user_agent)
        .set("Accept", "text/html,application/xhtml+xml")
        .call()
        .map_err(|error| match error {
            ureq::Error::Status(code, response) => {
                format!("HTTP {code} {}", response.status_text())
            }
            ureq::Error::Transport(transport) => transport_error_message(&transport),
        })?;
    let (body, _) = read_capped(response.into_reader(), max_bytes)?;
    Ok(String::from_utf8_lossy(&body).to_string())
}

/// A ureq agent for `url`, through the proxy the environment names.
fn agent_for(url: &str) -> ureq::Agent {
    let builder = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .redirects(MAX_REDIRECTS);
    match env_proxy(url).and_then(|proxy| ureq::Proxy::new(proxy).ok()) {
        Some(proxy) => builder.proxy(proxy).build(),
        None => builder.build(),
    }
}

fn transport_error_message(transport: &ureq::Transport) -> String {
    let kind = match transport.kind() {
        ureq::ErrorKind::Dns => "DNS lookup failed",
        ureq::ErrorKind::ConnectionFailed => "connection failed",
        ureq::ErrorKind::Io => "network I/O error (possibly a timeout)",
        ureq::ErrorKind::InvalidUrl => "invalid URL",
        ureq::ErrorKind::TooManyRedirects => "too many redirects",
        _ => "request failed",
    };
    format!("{kind}: {transport}")
}

fn read_capped(mut reader: impl Read, max_bytes: u64) -> Result<(Vec<u8>, bool), String> {
    let mut body = Vec::new();
    let read = (&mut reader)
        .take(max_bytes)
        .read_to_end(&mut body)
        .map_err(|error| error.to_string())?;
    let mut probe = [0u8; 1];
    let truncated =
        read as u64 == max_bytes && reader.read(&mut probe).map_err(|error| error.to_string())? > 0;
    Ok((body, truncated))
}

/// Turn a fetched body into the tool result. Pure so tests can exercise the
/// content-type branches without touching the network. A status outside 2xx
/// is reported as `status` plus a short `note`, never as an error: the model
/// asked a question about the URL and the server answered it.
fn process_response(
    url: &str,
    status: u16,
    status_text: &str,
    content_type: &str,
    body: &[u8],
    raw: bool,
    network_truncated: bool,
) -> Value {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let mut result = json!({
        "url": url,
        "status": status,
        "content_type": mime,
        "bytes": body.len(),
    });
    let mut notes = Vec::new();
    if !(200..300).contains(&status) {
        notes.push(
            format!("HTTP {status} {}", status_text.trim())
                .trim()
                .to_string(),
        );
    }
    if network_truncated {
        result["network_truncated"] = json!(true);
        notes.push("body exceeded the byte cap; only the first bytes were read".to_string());
    }
    let is_html = mime == "text/html" || mime == "application/xhtml+xml";
    let is_text = mime.starts_with("text/")
        || mime == "application/json"
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
        || mime == "application/xml"
        || mime == "application/javascript";
    if is_html && !raw {
        result["text"] = json!(extract_html_text(&String::from_utf8_lossy(body)));
    } else if is_html || is_text {
        result["text"] = json!(String::from_utf8_lossy(body).to_string());
    } else {
        notes.push("binary content omitted; only metadata is returned".to_string());
    }
    if !notes.is_empty() {
        result["note"] = json!(notes.join("; "));
    }
    result
}

pub fn validate_url(url: &str) -> Result<(), String> {
    let rest = if let Some(rest) = url.strip_prefix("https://") {
        rest
    } else if let Some(rest) = url.strip_prefix("http://") {
        rest
    } else {
        return Err("only http:// and https:// URLs are supported".to_string());
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err("URL has no host".to_string());
    }
    if authority.contains('@') {
        return Err("URLs with embedded credentials are not supported".to_string());
    }
    Ok(())
}

fn url_host(url: &str) -> String {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_string()
}

/// The proxy the environment names for `url`, or None to connect directly.
/// ureq's own `try_proxy_from_env` reads ALL_PROXY first and ignores
/// NO_PROXY, so the variables are read here (see AGENTS.md).
pub(crate) fn env_proxy(url: &str) -> Option<String> {
    let host = url_host(url)
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if host.is_empty() || no_proxy(&host) {
        return None;
    }
    let scheme_vars: &[&str] = if url.starts_with("https://") {
        &["https_proxy", "HTTPS_PROXY"]
    } else {
        &["http_proxy", "HTTP_PROXY"]
    };
    scheme_vars
        .iter()
        .chain(["all_proxy", "ALL_PROXY"].iter())
        .filter_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

/// Whether `host` is exempt from the proxy: NO_PROXY names it (`*`, a bare
/// host, or a `.suffix`), or it is this machine.
fn no_proxy(host: &str) -> bool {
    if matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") {
        return true;
    }
    let list = std::env::var("no_proxy")
        .or_else(|_| std::env::var("NO_PROXY"))
        .unwrap_or_default();
    list.split(',').any(|entry| {
        let entry = entry.trim().trim_start_matches('.').to_ascii_lowercase();
        let entry = entry.split(':').next().unwrap_or_default();
        if entry.is_empty() {
            return false;
        }
        entry == "*" || host == entry || host.ends_with(&format!(".{entry}"))
    })
}

// `title` is skipped because the extractor already emits it as the first line.
const SKIP_TAGS: [&str; 8] = [
    "script", "style", "head", "nav", "footer", "noscript", "template", "title",
];
const BLOCK_TAGS: [&str; 25] = [
    "p",
    "div",
    "br",
    "li",
    "ul",
    "ol",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "tr",
    "table",
    "section",
    "article",
    "header",
    "main",
    "aside",
    "blockquote",
    "pre",
    "hr",
    "form",
    "dt",
    "dd",
];

/// Reduce HTML to readable text: page title first, skip non-content elements
/// and comments, render links as `text (url)`, break on block elements,
/// decode common entities, and collapse whitespace runs.
pub fn extract_html_text(html: &str) -> String {
    let mut out = String::new();
    if let Some(title) = extract_title(html) {
        out.push_str(&title);
        out.push_str("\n\n");
    }
    let bytes = html.as_bytes();
    let mut i = 0;
    let mut link: Option<(String, String)> = None; // (href, buffered text)
    while i < bytes.len() {
        if bytes[i] != b'<' {
            let end = html[i..].find('<').map(|at| i + at).unwrap_or(html.len());
            let text = decode_entities(&html[i..end]);
            match &mut link {
                Some((_, buffered)) => buffered.push_str(&text),
                None => out.push_str(&text),
            }
            i = end;
            continue;
        }
        if html[i..].starts_with("<!--") {
            i = html[i..]
                .find("-->")
                .map(|at| i + at + 3)
                .unwrap_or(html.len());
            continue;
        }
        let tag_end = html[i..].find('>').map(|at| i + at).unwrap_or(html.len());
        let tag_body = &html[i + 1..tag_end.min(html.len())];
        i = (tag_end + 1).min(html.len());
        let closing = tag_body.starts_with('/');
        let name = tag_name(tag_body);
        if name.is_empty() {
            continue;
        }
        if !closing && SKIP_TAGS.contains(&name.as_str()) {
            i = skip_element(html, i, &name);
            continue;
        }
        if name == "a" {
            if closing {
                if let Some((href, text)) = link.take() {
                    out.push_str(&render_link(&text, &href));
                }
            } else {
                // Flush any unterminated previous link as plain text.
                if let Some((_, text)) = link.take() {
                    out.push_str(&text);
                }
                link = Some((
                    attr_value(tag_body, "href").unwrap_or_default(),
                    String::new(),
                ));
            }
            continue;
        }
        if BLOCK_TAGS.contains(&name.as_str()) {
            let target = match &mut link {
                Some((_, buffered)) => buffered,
                None => &mut out,
            };
            // Closing list items add no break so consecutive items stay adjacent.
            if name == "li" {
                if !closing {
                    target.push_str("\n- ");
                }
            } else {
                target.push('\n');
            }
        }
    }
    if let Some((_, text)) = link {
        out.push_str(&text);
    }
    collapse_whitespace(&out)
}

fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open_end = html[start..].find('>').map(|at| start + at + 1)?;
    let close = lower[open_end..].find("</title").map(|at| open_end + at)?;
    let title = collapse_whitespace(&decode_entities(&html[open_end..close]));
    (!title.is_empty()).then_some(title)
}

fn tag_name(tag_body: &str) -> String {
    tag_body
        .trim_start_matches('/')
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn skip_element(html: &str, from: usize, name: &str) -> usize {
    let lower = html[from..].to_ascii_lowercase();
    let closer = format!("</{name}");
    match lower.find(&closer) {
        Some(at) => {
            let after = from + at + closer.len();
            html[after..]
                .find('>')
                .map(|gt| after + gt + 1)
                .unwrap_or(html.len())
        }
        None => html.len(),
    }
}

pub(crate) fn attr_value(tag_body: &str, attr: &str) -> Option<String> {
    let lower = tag_body.to_ascii_lowercase();
    let needle = format!("{attr}=");
    let mut search = 0;
    loop {
        let at = lower[search..].find(&needle)? + search;
        let before = lower[..at].chars().next_back();
        if before.is_some_and(|ch| !ch.is_whitespace()) {
            search = at + needle.len();
            continue;
        }
        let rest = &tag_body[at + needle.len()..];
        let value = match rest.chars().next() {
            Some(quote @ ('"' | '\'')) => rest[1..].split(quote).next().unwrap_or_default(),
            _ => rest.split_whitespace().next().unwrap_or_default(),
        };
        return Some(decode_entities(value));
    }
}

fn render_link(text: &str, href: &str) -> String {
    let text = collapse_whitespace(text);
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') || href == text {
        return text;
    }
    if text.is_empty() {
        return href.to_string();
    }
    format!("{text} ({href})")
}

pub fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let end = rest[..rest.len().min(12)].find(';');
        let Some(end) = end else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" | "ensp" | "emsp" | "thinsp" => Some(' '),
            "middot" | "bull" => Some('\u{b7}'),
            "hellip" => Some('\u{2026}'),
            "ndash" => Some('\u{2013}'),
            "mdash" => Some('\u{2014}'),
            "lsquo" | "rsquo" => Some('\''),
            "ldquo" | "rdquo" => Some('"'),
            "laquo" => Some('\u{ab}'),
            "raquo" => Some('\u{bb}'),
            "times" => Some('\u{d7}'),
            "deg" => Some('\u{b0}'),
            "copy" => Some('\u{a9}'),
            "reg" => Some('\u{ae}'),
            "trade" => Some('\u{2122}'),
            _ => decode_numeric_entity(entity),
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_numeric_entity(entity: &str) -> Option<char> {
    let digits = entity.strip_prefix('#')?;
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse::<u32>().ok()?,
    };
    char::from_u32(code)
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank_run += 1;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blank_run > 0 {
                out.push('\n');
            }
        }
        blank_run = 0;
        out.push_str(&line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn validate_url_accepts_only_plain_http_and_https() {
        assert!(validate_url("https://example.com/docs?q=1").is_ok());
        assert!(validate_url("http://example.com").is_ok());
        assert!(validate_url("ftp://example.com").is_err());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("https://").is_err());
        assert!(validate_url("example.com").is_err());
    }

    #[test]
    fn validate_url_rejects_embedded_credentials() {
        assert!(validate_url("https://user:pass@example.com/").is_err());
        assert!(validate_url("http://admin@example.com").is_err());
        // '@' after the authority is fine.
        assert!(validate_url("https://example.com/path?email=a@b.c").is_ok());
    }

    #[test]
    fn run_reports_missing_or_invalid_url() {
        assert!(run(&json!({})).get("error").is_some());
        let result = run(&json!({ "url": "ftp://example.com" }));
        assert!(result["error"]
            .as_str()
            .unwrap()
            .contains("http:// and https://"));
    }

    #[test]
    fn extractor_strips_non_content_elements_and_comments() {
        let html = "<html><head><title>Doc</title><style>p{color:red}</style></head>\
            <body><nav>menu</nav><script>alert(1)</script><!-- hidden -->\
            <p>Hello world</p><footer>legal</footer></body></html>";
        let text = extract_html_text(html);
        assert_eq!(text, "Doc\n\nHello world");
    }

    #[test]
    fn extractor_renders_links_with_href() {
        let html = r#"<p>See <a href="https://example.com/a">the docs</a> now.</p>"#;
        let text = extract_html_text(html);
        assert_eq!(text, "See the docs (https://example.com/a) now.");
    }

    #[test]
    fn extractor_skips_fragment_and_self_referencing_links() {
        let html = r##"<a href="#top">Top</a> <a href="https://x.io">https://x.io</a>"##;
        let text = extract_html_text(html);
        assert_eq!(text, "Top https://x.io");
    }

    #[test]
    fn extractor_decodes_common_entities() {
        let html = "<p>a &amp; b &lt;c&gt; &quot;d&quot; &#39;e&#39; &#x41;&nbsp;f</p>";
        assert_eq!(extract_html_text(html), "a & b <c> \"d\" 'e' A f");
        assert_eq!(decode_entities("1 &unknown; 2 & 3"), "1 &unknown; 2 & 3");
    }

    #[test]
    fn extractor_breaks_on_block_elements_and_collapses_blanks() {
        let html = "<h1>Title</h1><div><div><p>one</p></div></div><ul><li>a</li><li>b</li></ul>";
        let text = extract_html_text(html);
        assert_eq!(text, "Title\n\none\n\n- a\n- b");
    }

    #[test]
    fn extractor_puts_page_title_first() {
        let html = "<title>My &amp; Page</title><p>body text</p>";
        assert_eq!(extract_html_text(html), "My & Page\n\nbody text");
    }

    #[test]
    fn process_response_extracts_html_text() {
        let body = b"<html><title>T</title><body><p>hi</p></body></html>";
        let result = process_response(
            "https://example.com",
            200,
            "OK",
            "text/html; charset=utf-8",
            body,
            false,
            false,
        );
        assert_eq!(result["status"], 200);
        assert_eq!(result["content_type"], "text/html");
        assert_eq!(result["text"], "T\n\nhi");
        assert_eq!(result["bytes"], body.len());
    }

    #[test]
    fn a_tiny_read_cap_is_raised_to_32_kb() {
        assert_eq!(read_cap(&json!({ "max_bytes": 300 })), 32 * 1024);
        assert_eq!(read_cap(&json!({ "max_bytes": 100_000 })), 100_000);
        assert_eq!(
            read_cap(&json!({ "max_bytes": 5_000_000 })),
            MAX_FETCH_BYTES
        );
        assert_eq!(read_cap(&json!({})), MAX_FETCH_BYTES);
    }

    #[test]
    fn process_response_passes_text_and_json_through() {
        let result = process_response(
            "u",
            200,
            "OK",
            "application/json",
            b"{\"a\":1}",
            false,
            false,
        );
        assert_eq!(result["text"], "{\"a\":1}");
        let result = process_response("u", 200, "OK", "text/plain", b"plain", false, false);
        assert_eq!(result["text"], "plain");
    }

    #[test]
    fn process_response_raw_skips_html_extraction() {
        let result = process_response("u", 200, "OK", "text/html", b"<p>hi</p>", true, false);
        assert_eq!(result["text"], "<p>hi</p>");
    }

    #[test]
    fn process_response_returns_metadata_only_for_binary() {
        let result = process_response("u", 200, "OK", "image/png", &[0x89, 0x50], false, false);
        assert!(result.get("text").is_none());
        assert_eq!(result["bytes"], 2);
        assert!(result["note"].as_str().unwrap().contains("binary"));
    }

    #[test]
    fn process_response_marks_network_truncation() {
        let result = process_response("u", 200, "OK", "text/plain", b"abc", false, true);
        assert_eq!(result["network_truncated"], true);
    }

    #[test]
    fn an_http_status_is_a_result_not_an_error() {
        // A 404 is the answer to "does this name exist?": the model needs the
        // status and the body, not a failed call.
        let body = b"<html><title>Not Found</title><body><p>no such domain</p></body></html>";
        let result = process_response(
            "https://rdap.org/domain/x.dev",
            404,
            "Not Found",
            "text/html",
            body,
            false,
            false,
        );
        assert!(result.get("error").is_none(), "{result}");
        assert_eq!(result["status"], 404);
        assert_eq!(result["note"], "HTTP 404 Not Found");
        assert_eq!(result["text"], "Not Found\n\nno such domain");

        let forbidden =
            process_response("u", 403, "Forbidden", "text/plain", b"nope", false, false);
        assert!(forbidden.get("error").is_none(), "{forbidden}");
        assert_eq!(forbidden["note"], "HTTP 403 Forbidden");
        assert_eq!(forbidden["text"], "nope");
        // A success keeps its clean shape.
        let ok = process_response("u", 200, "OK", "text/plain", b"hi", false, false);
        assert!(ok.get("note").is_none(), "{ok}");
    }

    #[test]
    fn env_proxy_follows_the_scheme_and_no_proxy() {
        // These tests share the process environment, so they run in one test.
        let restore: Vec<(&str, Option<String>)> = [
            "http_proxy",
            "HTTP_PROXY",
            "https_proxy",
            "HTTPS_PROXY",
            "all_proxy",
            "ALL_PROXY",
            "no_proxy",
            "NO_PROXY",
        ]
        .iter()
        .map(|name| (*name, env::var(name).ok()))
        .collect();
        for (name, _) in &restore {
            env::remove_var(name);
        }

        assert_eq!(env_proxy("https://example.com/x"), None);
        env::set_var("HTTP_PROXY", "http://127.0.0.1:7890");
        env::set_var("HTTPS_PROXY", "http://127.0.0.1:7891");
        assert_eq!(
            env_proxy("http://example.com/x").as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            env_proxy("https://example.com/x").as_deref(),
            Some("http://127.0.0.1:7891")
        );
        // ALL_PROXY only fills in for a scheme with no variable of its own.
        env::remove_var("HTTPS_PROXY");
        env::set_var("ALL_PROXY", "http://127.0.0.1:1080");
        assert_eq!(
            env_proxy("https://example.com/x").as_deref(),
            Some("http://127.0.0.1:1080")
        );
        // NO_PROXY exempts a host, a suffix and (with *) everything.
        env::set_var("NO_PROXY", "example.com, .internal.test");
        assert_eq!(env_proxy("https://example.com/x"), None);
        assert_eq!(env_proxy("https://www.example.com/x"), None);
        assert_eq!(env_proxy("https://a.internal.test/x"), None);
        assert!(env_proxy("https://other.test/x").is_some());
        env::set_var("NO_PROXY", "*");
        assert_eq!(env_proxy("https://other.test/x"), None);
        // This machine is never proxied.
        env::remove_var("NO_PROXY");
        assert_eq!(env_proxy("http://localhost:3000/x"), None);
        assert_eq!(env_proxy("http://127.0.0.1:3000/x"), None);

        for (name, value) in restore {
            match value {
                Some(value) => env::set_var(name, value),
                None => env::remove_var(name),
            }
        }
    }

    #[test]
    fn read_capped_stops_at_limit_and_flags_truncation() {
        let (body, truncated) = read_capped(&b"0123456789"[..], 4).unwrap();
        assert_eq!(body, b"0123");
        assert!(truncated);
        let (body, truncated) = read_capped(&b"0123"[..], 4).unwrap();
        assert_eq!(body, b"0123");
        assert!(!truncated);
        let (body, truncated) = read_capped(&b"01"[..], 4).unwrap();
        assert_eq!(body, b"01");
        assert!(!truncated);
    }
}
