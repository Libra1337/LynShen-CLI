//! The `web_search` and `web_fetch` tools' engines.
//!
//! `web_search` searches from this machine by default (`local`): it reads a
//! search engine's own HTML result page, needs no key and no login. The
//! LynShen gateway (POST /tools/v1/search) is used only when
//! `web_search_engine` in config.json names it; those calls authenticate
//! with the LynShen login session and are billed to the account.
//!
//! `web_fetch` fetches from this machine unless `web_fetch_engine` names a
//! gateway engine (POST /tools/v1/fetch); the local engine lives in
//! `web_fetch`.

use serde_json::{json, Value};
use std::time::Duration;

/// `local` searches from this machine; `gateway` (and the vendor names kept
/// for configs written before the local engine existed) goes through the
/// LynShen gateway.
pub const SEARCH_ENGINES: &[&str] = &["local", "gateway", "auto", "parallel", "brave"];
pub const FETCH_ENGINES: &[&str] = &["local", "jina", "firecrawl", "parallel"];
pub const DEFAULT_SEARCH_ENGINE: &str = "local";
pub const DEFAULT_FETCH_ENGINE: &str = "local";
const LOCAL_ENGINE: &str = "local";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The gateway gives a fetch vendor 30 s per page; this leaves room for it.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_SEARCH_RESULTS: u64 = 10;
const QUERY_MAX_CHARS: usize = 400;
/// A result page is a few hundred KB of markup at most.
const MAX_SEARCH_PAGE_BYTES: u64 = 1024 * 1024;
/// The HTML endpoints serve a stripped page to an unknown client.
const SEARCH_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36";

/// What the web tools need from the engine's config and login state, set on
/// the tool state at the start of every turn.
#[derive(Debug, Clone)]
pub struct WebTools {
    pub api_url: String,
    pub encrypt_secrets: bool,
    pub search_engine: String,
    pub fetch_engine: String,
    /// A LynShen session exists; only gateway search needs one.
    pub signed_in: bool,
}

impl WebTools {
    /// Whether `web_search` goes through the gateway instead of searching
    /// from this machine.
    pub fn gateway_search(&self) -> bool {
        self.search_engine != LOCAL_ENGINE
    }

    /// Whether `web_search` can run at all: the local engine always can, the
    /// gateway needs a LynShen login.
    pub fn search_available(&self) -> bool {
        !self.gateway_search() || self.signed_in
    }
}

pub fn search_definition() -> Value {
    json!({
        "type": "function",
        "name": "web_search",
        "description": "Search the web. Returns ranked results with title, url and snippet; read the pages you need with web_fetch.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Up to 400 characters." },
                "max_results": { "type": "number", "description": "1-10, default 10." },
                "freshness": { "type": "string", "enum": ["day", "week", "month", "year"], "description": "Only pages published within this period." }
            },
            "required": ["query"]
        }
    })
}

pub fn run_search(args: &Value, web: Option<&WebTools>) -> Value {
    let Some(web) = web else {
        return json!({ "error": "web_search is not available in this session" });
    };
    if !web.gateway_search() {
        return local_search(args, &LiveFetcher);
    }
    let body = match search_body(args, &web.search_engine) {
        Ok(body) => body,
        Err(error) => return json!({ "error": error }),
    };
    match gateway_post(web, "/tools/v1/search", body) {
        Ok(value) => {
            let results = value
                .get("results")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            crate::log_info!(
                "web_search",
                "searched",
                engine = web.search_engine.clone(),
                results = results
            );
            value
        }
        Err(error) => {
            crate::log_warn!(
                "web_search",
                "search failed",
                engine = web.search_engine.clone(),
                error = error.clone()
            );
            json!({ "error": error })
        }
    }
}

/// Runs `web_fetch` on the configured engine: `local` fetches from this
/// machine, anything else goes through the gateway.
pub fn run_fetch(args: &Value, web: Option<&WebTools>) -> Value {
    let Some(web) = web.filter(|web| web.fetch_engine != "local") else {
        return crate::web_fetch::run(args);
    };
    let Some(url) = args.get("url").and_then(Value::as_str) else {
        return json!({ "error": "missing url" });
    };
    if let Err(error) = crate::web_fetch::validate_url(url) {
        return json!({ "url": url, "error": error });
    }
    let max_bytes = args.get("max_bytes").and_then(Value::as_u64);
    let body = json!({ "url": url, "engine": web.fetch_engine });
    match gateway_post(web, "/tools/v1/fetch", body) {
        Ok(value) => {
            crate::log_info!(
                "web_fetch",
                "fetched via gateway",
                engine = web.fetch_engine.clone()
            );
            fetch_result(&value, url, max_bytes)
        }
        Err(error) => {
            crate::log_warn!(
                "web_fetch",
                "gateway fetch failed",
                engine = web.fetch_engine.clone(),
                error = error.clone()
            );
            json!({ "url": url, "error": error })
        }
    }
}

fn search_body(args: &Value, engine: &str) -> Result<Value, String> {
    let query = search_query(args)?;
    // `gateway` only says where to search; the gateway picks the vendor.
    let engine = if engine == "gateway" { "auto" } else { engine };
    let mut body = json!({ "query": query, "engine": engine });
    if let Some(value) = args.get("max_results") {
        let count = value
            .as_u64()
            .filter(|count| (1..=MAX_SEARCH_RESULTS).contains(count))
            .ok_or("max_results must be an integer from 1 to 10")?;
        body["max_results"] = json!(count);
    }
    if let Some(freshness) = args.get("freshness").and_then(Value::as_str) {
        body["freshness"] = json!(freshness);
    }
    Ok(body)
}

fn search_query(args: &Value) -> Result<String, String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .ok_or("missing query")?;
    Ok(query.chars().take(QUERY_MAX_CHARS).collect())
}

fn search_max_results(args: &Value) -> Result<usize, String> {
    match args.get("max_results") {
        None | Some(Value::Null) => Ok(MAX_SEARCH_RESULTS as usize),
        Some(value) => value
            .as_u64()
            .filter(|count| (1..=MAX_SEARCH_RESULTS).contains(count))
            .map(|count| count as usize)
            .ok_or_else(|| "max_results must be an integer from 1 to 10".to_string()),
    }
}

/// Shapes a gateway fetch like a local one (`url`, `status`, `text`), capped
/// at `max_bytes` when the caller asked for less.
fn fetch_result(value: &Value, requested_url: &str, max_bytes: Option<u64>) -> Value {
    let mut text = value
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut result = json!({
        "url": value.get("url").and_then(Value::as_str).unwrap_or(requested_url),
        "engine": value.get("engine").cloned().unwrap_or(Value::Null),
    });
    if let Some(status) = value.get("status").and_then(Value::as_u64) {
        result["status"] = json!(status);
    }
    if let Some(title) = value
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        result["title"] = json!(title);
    }
    if let Some(published) = value
        .get("published_at")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
    {
        result["published_at"] = json!(published);
    }
    if let Some(cap) = max_bytes
        .map(|cap| cap as usize)
        .filter(|cap| text.len() > *cap)
    {
        let mut end = cap;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        result["truncated"] = json!(true);
    }
    result["text"] = json!(text);
    result
}

// ---------------------------------------------------------------------------
// Local search: a search engine's own HTML result page, parsed here. No key,
// no login, and the same proxy as web_fetch.
// ---------------------------------------------------------------------------

/// How the local engine reaches a result page. A test injects its own.
pub(crate) trait PageFetcher {
    fn get(&self, url: &str) -> Result<String, String>;
}

struct LiveFetcher;

impl PageFetcher for LiveFetcher {
    fn get(&self, url: &str) -> Result<String, String> {
        crate::web_fetch::get_text(url, SEARCH_USER_AGENT, MAX_SEARCH_PAGE_BYTES)
    }
}

#[derive(Debug, PartialEq)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// A result page to try: Bing first (its markup is stable and its snippets
/// are the longest), its China host when the global one is unreachable, then
/// DuckDuckGo's HTML endpoint.
struct SearchSource {
    engine: &'static str,
    url: fn(&str) -> String,
    parse: fn(&str, usize) -> Vec<SearchResult>,
}

const SEARCH_SOURCES: &[SearchSource] = &[
    SearchSource {
        engine: "bing",
        url: |query| format!("https://www.bing.com/search?q={query}&count=20&setlang=en"),
        parse: parse_bing,
    },
    SearchSource {
        engine: "bing",
        url: |query| format!("https://cn.bing.com/search?q={query}&count=20&ensearch=1"),
        parse: parse_bing,
    },
    SearchSource {
        engine: "duckduckgo",
        url: |query| format!("https://html.duckduckgo.com/html/?q={query}"),
        parse: parse_duckduckgo,
    },
];

fn local_search(args: &Value, fetcher: &dyn PageFetcher) -> Value {
    let query = match search_query(args) {
        Ok(query) => query,
        Err(error) => return json!({ "error": error }),
    };
    let max_results = match search_max_results(args) {
        Ok(max) => max,
        Err(error) => return json!({ "error": error }),
    };
    let encoded = url_encode(&query);
    let mut failures = Vec::new();
    for source in SEARCH_SOURCES {
        let url = (source.url)(&encoded);
        match fetcher.get(&url) {
            Ok(page) => {
                let results = (source.parse)(&page, max_results);
                if results.is_empty() {
                    failures.push(format!("{}: {}", source.engine, empty_page_reason(&page)));
                    continue;
                }
                crate::log_info!(
                    "web_search",
                    "searched",
                    engine = source.engine,
                    results = results.len()
                );
                let mut value = json!({
                    "query": query,
                    "engine": source.engine,
                    "results": results
                        .iter()
                        .map(|result| json!({
                            "title": result.title,
                            "url": result.url,
                            "snippet": result.snippet,
                        }))
                        .collect::<Vec<_>>(),
                });
                if args.get("freshness").and_then(Value::as_str).is_some() {
                    value["note"] = json!(
                        "freshness is not supported by the local search engine and was ignored"
                    );
                }
                return value;
            }
            Err(error) => failures.push(format!("{}: {error}", source.engine)),
        }
    }
    crate::log_warn!("web_search", "search failed", error = failures.join("; "));
    json!({
        "query": query,
        "error": format!("no search engine answered ({}). Check the network or the HTTP_PROXY settings, or fetch a known URL with web_fetch.", failures.join("; ")),
    })
}

/// Why a page carried no results, as far as it says itself.
fn empty_page_reason(page: &str) -> &'static str {
    let lower = page.to_ascii_lowercase();
    if [
        "anomaly-modal",
        "captcha",
        "unusual traffic",
        "are you a human",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        "the engine asked for a captcha"
    } else {
        "the result page held no results"
    }
}

/// Bing wraps each organic result in `li.b_algo`: a heading anchor with the
/// title and target, and a `<p>` with the snippet.
fn parse_bing(html: &str, max_results: usize) -> Vec<SearchResult> {
    let mut results: Vec<SearchResult> = Vec::new();
    for block in split_blocks(html, "b_algo") {
        // The result's own link is the heading's; a block also holds a
        // breadcrumb link to the same site above it.
        let heading = element_html(block, "h2").unwrap_or("");
        let Some((href, title)) = first_link(heading).or_else(|| first_link(block)) else {
            continue;
        };
        let Some(url) = result_url(&href) else {
            continue;
        };
        if results.iter().any(|result| result.url == url) {
            continue;
        }
        results.push(SearchResult {
            title: if title.is_empty() { url.clone() } else { title },
            url,
            snippet: element_text(block, "p").unwrap_or_default(),
        });
        if results.len() >= max_results {
            break;
        }
    }
    results
}

/// DuckDuckGo's HTML endpoint: `a.result__a` carries the title behind a
/// `/l/?uddg=…` redirect, `a.result__snippet` the snippet below it.
fn parse_duckduckgo(html: &str, max_results: usize) -> Vec<SearchResult> {
    let mut results: Vec<SearchResult> = Vec::new();
    for link in links(html) {
        if link.class.contains("result__a") {
            if results.len() >= max_results {
                break;
            }
            let Some(url) = result_url(&link.href) else {
                continue;
            };
            if results.iter().any(|result| result.url == url) {
                continue;
            }
            results.push(SearchResult {
                title: if link.text.is_empty() {
                    url.clone()
                } else {
                    link.text
                },
                url,
                snippet: String::new(),
            });
        } else if link.class.contains("result__snippet") {
            if let Some(last) = results.last_mut() {
                if last.snippet.is_empty() {
                    last.snippet = link.text;
                }
            }
        }
    }
    results
}

/// The slices of `html` that start at each occurrence of `marker` (a class
/// name) and run to the next one: one result block each.
fn split_blocks<'a>(html: &'a str, marker: &str) -> Vec<&'a str> {
    let starts: Vec<usize> = html.match_indices(marker).map(|(at, _)| at).collect();
    starts
        .iter()
        .enumerate()
        .map(|(index, &start)| {
            let end = starts.get(index + 1).copied().unwrap_or(html.len());
            &html[start..end]
        })
        .collect()
}

struct Link {
    class: String,
    href: String,
    text: String,
}

/// Every `<a>` in `html` with its class, href and visible text.
fn links(html: &str) -> Vec<Link> {
    let mut links = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find("<a") {
        let after = &rest[at + 2..];
        // `<abbr>` and friends: an anchor's name ends at a space or `>`.
        if !after.starts_with([' ', '\t', '\n', '\r', '>']) {
            rest = after;
            continue;
        }
        let Some(tag_end) = after.find('>') else {
            break;
        };
        let tag_body = &after[..tag_end];
        let body = &after[tag_end + 1..];
        let text_end = body.find("</a").unwrap_or(body.len());
        links.push(Link {
            class: crate::web_fetch::attr_value(tag_body, "class").unwrap_or_default(),
            href: crate::web_fetch::attr_value(tag_body, "href").unwrap_or_default(),
            text: strip_tags(&body[..text_end]),
        });
        rest = &body[text_end..];
    }
    links
}

/// The first link in `block` that points at a page: (href, text).
fn first_link(block: &str) -> Option<(String, String)> {
    links(block)
        .into_iter()
        .find(|link| result_url(&link.href).is_some())
        .map(|link| (link.href, link.text))
}

/// The markup inside the first `<name>` element in `block`.
fn element_html<'a>(block: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}");
    let at = block.find(&open)?;
    let after = &block[at + open.len()..];
    if !after.starts_with([' ', '\t', '\n', '\r', '>']) {
        return None;
    }
    let tag_end = after.find('>')?;
    let body = &after[tag_end + 1..];
    let end = body.find(&format!("</{name}")).unwrap_or(body.len());
    Some(&body[..end])
}

/// The text of the first `<name>` element in `block`.
fn element_text(block: &str, name: &str) -> Option<String> {
    element_html(block, name)
        .map(strip_tags)
        .filter(|text| !text.is_empty())
}

/// An http(s) result URL, or None when the href is a fragment, a script or
/// an engine's own page. A redirect through the engine (DuckDuckGo's
/// `uddg=`, Bing's `ck/a?…u=a1<base64url>`) is decoded to its target.
fn result_url(href: &str) -> Option<String> {
    let href = href.trim();
    let absolute = if let Some(rest) = href.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        href.to_string()
    };
    if let Some(target) = query_param(&absolute, "uddg").map(|value| percent_decode(&value)) {
        return result_url(&target);
    }
    if absolute.contains("/ck/a") {
        if let Some(target) = query_param(&absolute, "u")
            .and_then(|value| value.strip_prefix("a1").map(str::to_string))
            .and_then(|value| decode_base64_url(&value))
        {
            return result_url(&target);
        }
    }
    if !absolute.starts_with("http://") && !absolute.starts_with("https://") {
        return None;
    }
    let host = absolute
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let own = ["bing.com", "duckduckgo.com", "microsofttranslator.com"];
    if own
        .iter()
        .any(|engine| host == *engine || host.ends_with(&format!(".{engine}")))
    {
        return None;
    }
    Some(absolute)
}

fn query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split(['?', '#']).nth(1)?;
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| crate::web_fetch::decode_entities(key).trim() == name)
        .map(|(_, value)| value.to_string())
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    // One line: a title or snippet spread over several source lines reads as
    // one sentence.
    crate::web_fetch::decode_entities(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Percent-encodes a query for a URL. Unreserved characters stay as they
/// are, everything else becomes %XX of its UTF-8 bytes.
fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&text[index + 1..index + 3], 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Bing's `u=a1…` parameter is base64url without padding.
fn decode_base64_url(text: &str) -> Option<String> {
    use base64::Engine;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text.trim_end_matches('='))
        .ok()?;
    String::from_utf8(decoded).ok()
}

fn gateway_post(web: &WebTools, path: &str, body: Value) -> Result<Value, String> {
    let auth = crate::oauth::ensure_session(&web.api_url, web.encrypt_secrets)?;
    let token = auth
        .lynshen_access_token()
        .ok_or("not logged in to LynShen. Run /login.")?;
    let url = format!("{}{}", web.api_url.trim_end_matches('/'), path);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .build();
    match agent
        .post(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(body)
    {
        Ok(response) => response
            .into_json::<Value>()
            .map_err(|error| format!("invalid gateway response: {error}")),
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_json::<Value>().unwrap_or(Value::Null);
            let message = body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            Err(format!("LynShen gateway HTTP {code}: {message}"))
        }
        Err(ureq::Error::Transport(transport)) => {
            Err(format!("could not reach the LynShen gateway: {transport}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_body_carries_engine_and_validates() {
        let body = search_body(
            &json!({ "query": " rust ", "max_results": 3, "freshness": "week" }),
            "brave",
        )
        .unwrap();
        assert_eq!(
            body,
            json!({ "query": "rust", "engine": "brave", "max_results": 3, "freshness": "week" })
        );
        assert_eq!(
            search_body(&json!({ "query": "  " }), "auto").unwrap_err(),
            "missing query"
        );
        assert!(search_body(&json!({ "query": "x", "max_results": 11 }), "auto").is_err());
        assert!(search_body(&json!({ "query": "x", "max_results": 2.5 }), "auto").is_err());
    }

    #[test]
    fn fetch_result_matches_local_shape_and_caps_text() {
        let gateway = json!({
            "object": "web.fetch", "engine": "jina", "url": "https://example.com/",
            "title": "Example", "content": "héllo world", "status": 200
        });
        let result = fetch_result(&gateway, "https://example.com", Some(2));
        assert_eq!(result["url"], "https://example.com/");
        assert_eq!(result["status"], 200);
        assert_eq!(result["title"], "Example");
        // The cap backs off to a char boundary instead of splitting "é".
        assert_eq!(result["text"], "h");
        assert_eq!(result["truncated"], true);

        let full = fetch_result(&gateway, "https://example.com", None);
        assert_eq!(full["text"], "héllo world");
        assert!(full.get("truncated").is_none());
    }

    #[test]
    fn local_engine_and_missing_config_fetch_locally() {
        let local = WebTools {
            api_url: "https://api.lynshen.org".to_string(),
            encrypt_secrets: false,
            search_engine: "auto".to_string(),
            fetch_engine: "local".to_string(),
            signed_in: true,
        };
        for web in [None, Some(&local)] {
            let result = run_fetch(&json!({ "url": "ftp://example.com" }), web);
            assert!(
                result["error"].as_str().unwrap().contains("http"),
                "{result}"
            );
        }
    }

    #[test]
    fn search_without_config_reports_unavailable() {
        let result = run_search(&json!({ "query": "x" }), None);
        assert!(result["error"].as_str().unwrap().contains("not available"));
    }

    #[test]
    fn local_search_needs_no_login_and_the_gateway_does() {
        let mut web = WebTools {
            api_url: "https://api.lynshen.org".to_string(),
            encrypt_secrets: false,
            search_engine: "local".to_string(),
            fetch_engine: "local".to_string(),
            signed_in: false,
        };
        assert!(!web.gateway_search());
        assert!(web.search_available());
        web.search_engine = "gateway".to_string();
        assert!(web.gateway_search());
        assert!(!web.search_available());
        web.signed_in = true;
        assert!(web.search_available());
    }

    /// A fetcher over fixed pages that records what was asked for.
    struct FakeFetcher {
        pages: Vec<(&'static str, Result<String, String>)>,
        asked: std::cell::RefCell<Vec<String>>,
    }

    impl PageFetcher for FakeFetcher {
        fn get(&self, url: &str) -> Result<String, String> {
            self.asked.borrow_mut().push(url.to_string());
            self.pages
                .iter()
                .find(|(host, _)| url.contains(*host))
                .map(|(_, page)| page.clone())
                .unwrap_or_else(|| Err("not in the fixture".to_string()))
        }
    }

    /// Bing's organic block, trimmed to the parts the parser reads.
    const BING_PAGE: &str = r#"<html><body><ol id="b_results">
        <li class="b_ad"><a href="https://ads.example/pay">Ad</a><p>buy now</p></li>
        <li class="b_algo"><h2><a href="https://doc.rust-lang.org/book/" h="ID=1">The Rust
            Programming Language</a></h2><div class="b_caption"><p>Learn Rust &amp; its
            ownership model.</p></div></li>
        <li class="b_algo"><h2><a href="https://www.bing.com/ck/a?!&amp;&amp;p=1&amp;u=a1aHR0cHM6Ly9jcmF0ZXMuaW8vY3JhdGVzL3VyZXE">crates.io:
            ureq</a></h2><p>A simple HTTP client.</p></li>
        </ol></body></html>"#;

    const DDG_PAGE: &str = r#"<html><body>
        <div class="result results_links"><h2 class="result__title">
        <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fa&amp;rut=9f">Example
        A</a></h2><a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fa">First
        snippet.</a></div>
        <div class="result results_links"><h2 class="result__title">
        <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fb">Example B</a>
        </h2><a class="result__snippet" href="/l/?x=1">Second snippet.</a></div>
        </body></html>"#;

    #[test]
    fn bing_results_carry_title_url_and_snippet() {
        let results = parse_bing(BING_PAGE, 10);
        assert_eq!(results.len(), 2, "{results:?}");
        assert_eq!(results[0].title, "The Rust Programming Language");
        assert_eq!(results[0].url, "https://doc.rust-lang.org/book/");
        assert_eq!(results[0].snippet, "Learn Rust & its ownership model.");
        // The engine's own redirect is decoded to the target page.
        assert_eq!(results[1].url, "https://crates.io/crates/ureq");
        assert_eq!(results[1].snippet, "A simple HTTP client.");
        // max_results caps the list.
        assert_eq!(parse_bing(BING_PAGE, 1).len(), 1);
        assert!(parse_bing("<html><body>nothing</body></html>", 10).is_empty());
    }

    #[test]
    fn duckduckgo_results_decode_the_redirect_url() {
        let results = parse_duckduckgo(DDG_PAGE, 10);
        assert_eq!(results.len(), 2, "{results:?}");
        assert_eq!(results[0].title, "Example A");
        assert_eq!(results[0].url, "https://example.org/a");
        assert_eq!(results[0].snippet, "First snippet.");
        assert_eq!(results[1].url, "https://example.org/b");
        assert_eq!(results[1].snippet, "Second snippet.");
        assert_eq!(parse_duckduckgo(DDG_PAGE, 1).len(), 1);
    }

    #[test]
    fn result_urls_decode_redirects_and_drop_the_engines_own_pages() {
        assert_eq!(
            result_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Fa.test%2Fp%3Fq%3D1&rut=x").unwrap(),
            "https://a.test/p?q=1"
        );
        assert_eq!(
            result_url("https://www.bing.com/ck/a?u=a1aHR0cHM6Ly9iLnRlc3Qv").unwrap(),
            "https://b.test/"
        );
        assert_eq!(result_url("https://c.test/x").unwrap(), "https://c.test/x");
        assert_eq!(result_url("#top"), None);
        assert_eq!(result_url("javascript:void(0)"), None);
        assert_eq!(result_url("/search?q=next+page"), None);
        assert_eq!(result_url("https://www.bing.com/images/search?q=x"), None);
        assert_eq!(
            url_encode("rust ureq proxy?&=/"),
            "rust+ureq+proxy%3F%26%3D%2F"
        );
        assert_eq!(percent_decode("a%2Fb%20c"), "a/b c");
    }

    #[test]
    fn local_search_tries_bing_then_duckduckgo() {
        let fetcher = FakeFetcher {
            pages: vec![
                ("www.bing.com", Err("connection failed".to_string())),
                // Reachable but empty: a consent or captcha page.
                (
                    "cn.bing.com",
                    Ok("<html><body>no results</body></html>".to_string()),
                ),
                ("html.duckduckgo.com", Ok(DDG_PAGE.to_string())),
            ],
            asked: Default::default(),
        };
        let result = local_search(
            &json!({ "query": " example ", "max_results": 1, "freshness": "week" }),
            &fetcher,
        );
        assert!(result.get("error").is_none(), "{result}");
        assert_eq!(result["engine"], "duckduckgo");
        assert_eq!(result["query"], "example");
        assert_eq!(result["results"].as_array().unwrap().len(), 1);
        assert_eq!(result["results"][0]["url"], "https://example.org/a");
        // freshness has no equivalent here, so it is reported as ignored.
        assert!(result["note"].as_str().unwrap().contains("freshness"));
        let asked = fetcher.asked.borrow().clone();
        assert_eq!(asked.len(), 3, "{asked:?}");
        assert!(asked[0].starts_with("https://www.bing.com/search?q=example"));
        assert!(asked[1].starts_with("https://cn.bing.com/search?q=example"));
        assert!(asked[2].starts_with("https://html.duckduckgo.com/html/?q=example"));

        // The first engine that answers wins and the rest are not asked.
        let fetcher = FakeFetcher {
            pages: vec![("www.bing.com", Ok(BING_PAGE.to_string()))],
            asked: Default::default(),
        };
        let result = local_search(&json!({ "query": "rust" }), &fetcher);
        assert_eq!(result["engine"], "bing");
        assert_eq!(fetcher.asked.borrow().len(), 1);
        assert!(result.get("note").is_none(), "{result}");
    }

    #[test]
    fn local_search_reports_bad_arguments_and_a_dead_network() {
        assert!(local_search(
            &json!({ "query": "  " }),
            &FakeFetcher {
                pages: vec![],
                asked: Default::default()
            }
        )["error"]
            .as_str()
            .unwrap()
            .contains("missing query"));
        assert!(local_search(
            &json!({ "query": "x", "max_results": 50 }),
            &FakeFetcher {
                pages: vec![],
                asked: Default::default()
            }
        )["error"]
            .as_str()
            .unwrap()
            .contains("max_results"));
        let dead = FakeFetcher {
            pages: vec![],
            asked: Default::default(),
        };
        let result = local_search(&json!({ "query": "x" }), &dead);
        let error = result["error"].as_str().unwrap();
        assert!(error.contains("no search engine answered"), "{error}");
        assert_eq!(dead.asked.borrow().len(), SEARCH_SOURCES.len());
    }

    #[test]
    #[ignore = "hits the real search engines"]
    fn live_local_search_returns_results() {
        let result = local_search(
            &json!({ "query": "rust ureq crate", "max_results": 3 }),
            &LiveFetcher,
        );
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
        assert!(result.get("error").is_none(), "{result}");
        let results = result["results"].as_array().unwrap();
        assert!(!results.is_empty(), "{result}");
        for item in results {
            assert!(item["url"].as_str().unwrap().starts_with("http"), "{item}");
            assert!(!item["title"].as_str().unwrap().is_empty(), "{item}");
        }
    }
}
