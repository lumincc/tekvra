//! Defensive HTML web search (`web_search` tool) over a configurable HTML endpoint
//! (default: DuckDuckGo HTML Lite) with zero external HTML-parsing crates.
//!
//! Parsing is a deliberately hand-written scanner over well-known anchor classes
//! (`result__a` titles, `result__snippet` bodies) with `/l/?uddg=` redirect decoding,
//! the five named HTML entities, host collapsing, and a hard result cap (1..=10).

use tekvra_protocol::{ToolOutput, WebSearchResult};

use crate::{SafeWebFetcher, ToolsError};

/// Default search endpoint (DuckDuckGo HTML Lite — no JS required).
pub const DEFAULT_WEBSEARCH_BASE_URL: &str = "https://html.duckduckgo.com/html/";

/// Hard bounds on `max_results` (design: 1..=10, default 8 handled by the registry arm).
pub const MAX_SEARCH_RESULTS: usize = 10;

/// Performs one web search per call through the SSRF-safe fetcher.
#[derive(Debug, Clone)]
pub struct WebSearcher {
    fetcher: SafeWebFetcher,
    base_url: String,
}

impl WebSearcher {
    /// Constructs a searcher bound to the session's SSRF-safe fetcher and endpoint.
    #[must_use]
    pub fn new(fetcher: SafeWebFetcher, base_url: Option<String>) -> Self {
        let base = base_url
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_WEBSEARCH_BASE_URL.to_string());
        Self {
            fetcher,
            base_url: base,
        }
    }

    /// Executes `query` and returns up to `max_results` (clamped to `1..=10`) results.
    ///
    /// # Errors
    /// Returns a bounded [`ToolsError`] naming the endpoint when the fetch fails, the endpoint
    /// returns non-HTML, or zero results were parsed.
    pub async fn search(&self, query: &str, max_results: u32) -> Result<ToolOutput, ToolsError> {
        let capped = usize::try_from(max_results.max(1))
            .unwrap_or(1)
            .min(MAX_SEARCH_RESULTS);
        let url = format!(
            "{}?q={}",
            self.base_url.trim_end_matches('/'),
            encode_query(query)
        );
        let fetched = self.fetcher.fetch(&url).await?;
        let ToolOutput::Text { text } = fetched else {
            return Err(ToolsError::WebFetch {
                message: format!(
                    "`web_search` endpoint `{}` returned non-text",
                    self.base_url
                ),
            });
        };
        // `SafeWebFetcher::fetch` prepends `HTTP <status> <url>\n\n` — take the body.
        let body = text
            .split_once("\n\n")
            .map(|(_, body)| body)
            .unwrap_or(&text);
        if !body.contains("result__a") {
            return Err(ToolsError::WebFetch {
                message: format!(
                    "`web_search` endpoint `{}` returned no parseable HTML results for `{query}`",
                    self.base_url
                ),
            });
        }
        let results = parse_results(body, capped);
        if results.is_empty() {
            return Err(ToolsError::WebFetch {
                message: format!(
                    "`web_search` endpoint `{}` returned zero results for `{query}`",
                    self.base_url
                ),
            });
        }
        Ok(ToolOutput::SearchResults {
            query: query.to_string(),
            results,
        })
    }
}

/// Percent-encodes a query string for the `q=` parameter (RFC 3986 unreserved kept).
fn encode_query(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for byte in query.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Decodes percent-escapes in a `/l/?uddg=` target URL.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"),
                16,
            )
        {
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Unescapes the five named entities the scanner can encounter (`& < > " '`).
fn html_unescape(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
}

/// Strips any nested tags (`<b>`, `<span>` …) from a raw element body.
fn strip_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Extracts the value of `href="…"` inside one anchor tag body.
fn href_of(anchor_tag: &str) -> Option<String> {
    let lower = anchor_tag.to_ascii_lowercase();
    let idx = lower.find("href=")?;
    let rest = &anchor_tag[idx + 5..];
    let rest = rest.trim_start();
    let quoted = rest.starts_with('"') || rest.starts_with('\'');
    let quote = rest.chars().next()?;
    if quoted {
        let end = rest[1..].find(quote)?;
        Some(rest[1..1 + end].to_string())
    } else {
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

/// Resolves a DuckDuckGo result href (either a direct URL or `/l/?uddg=<encoded>`).
fn resolve_result_href(href: &str) -> Option<String> {
    let trimmed = html_unescape(href.trim());
    if let Some(pos) = trimmed.find("uddg=") {
        let encoded = &trimmed[pos + 5..];
        let end = encoded.find('&').unwrap_or(encoded.len());
        let decoded = percent_decode(&encoded[..end]);
        if decoded.starts_with("http://") || decoded.starts_with("https://") {
            return Some(decoded);
        }
        return None;
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Some(trimmed);
    }
    None
}

/// Returns the text between the end of `open_tag_pos`'s tag and its closing `</a>`.
/// Tags are stripped before entity decoding so `&lt;v2&gt;` survives as literal text.
fn anchor_text_after(body: &str, tag_end: usize) -> String {
    let rest = &body[tag_end..];
    let end = rest.to_ascii_lowercase().find("</a>").unwrap_or(rest.len());
    html_unescape(strip_tags(&rest[..end]).trim())
}

/// Scans `body` for result anchors, collapsing duplicate hosts and capping at `max`.
fn parse_results(body: &str, max: usize) -> Vec<WebSearchResult> {
    let mut out: Vec<WebSearchResult> = Vec::new();
    let mut seen_hosts: Vec<String> = Vec::new();
    let lower = body.to_ascii_lowercase();
    let mut cursor = 0usize;

    while out.len() < max {
        let rel_pos = lower[cursor..]
            .find("class=\"result__a\"")
            .map(|p| cursor + p);
        let Some(mark) = rel_pos else { break };
        // The anchor tag opening `<a` precedes the class marker.
        let tag_start = body[..mark].rfind("<a").unwrap_or(mark);
        let Some(tag_end_rel) = body[mark..].find('>') else {
            break;
        };
        let tag_end = mark + tag_end_rel + 1;
        let anchor_tag = &body[tag_start..tag_end];
        let Some(href) = href_of(anchor_tag).and_then(|h| resolve_result_href(&h)) else {
            cursor = tag_end;
            continue;
        };
        let title = anchor_text_after(body, tag_end);
        let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
        if title.is_empty() {
            cursor = tag_end;
            continue;
        }

        // Snippet: the nearest `result__snippet` anchor AFTER this title (optional).
        let snippet_pos = lower[tag_end..]
            .find("class=\"result__snippet\"")
            .map(|p| tag_end + p);
        let next_title_pos = lower[tag_end..]
            .find("class=\"result__a\"")
            .map(|p| tag_end + p);
        let snippet = match (snippet_pos, next_title_pos) {
            (Some(sp), Some(np)) if sp < np => {
                let sp_end = body[sp..].find('>').map(|p| sp + p + 1).unwrap_or(sp);
                anchor_text_after(body, sp_end)
            }
            (Some(sp), None) => {
                let sp_end = body[sp..].find('>').map(|p| sp + p + 1).unwrap_or(sp);
                anchor_text_after(body, sp_end)
            }
            _ => String::new(),
        };
        let snippet = snippet.split_whitespace().collect::<Vec<_>>().join(" ");

        // Host collapse: keep only the first hit per host.
        let host = host_of(&href);
        if let Some(host) = &host {
            if seen_hosts.iter().any(|h| h == host) {
                cursor = tag_end;
                continue;
            }
            seen_hosts.push(host.clone());
        }

        out.push(WebSearchResult {
            title,
            url: href,
            snippet,
        });
        cursor = tag_end;
    }
    out
}

/// Best-effort host extraction for collapse decisions.
fn host_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if host.is_empty() { None } else { Some(host) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_PAGE: &str = r##"<html><body>
    <div class="result results_links">
      <h2 class="result__title">
        <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdocs&amp;rut=abc"><b>Example</b> Docs</a>
      </h2>
      <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdocs">The &quot;official&quot; &amp; friendly guide &lt;v2&gt;</a>
    </div>
    <div class="result">
      <h2 class="result__title"><a class="result__a" href="https://direct.example.org/page">Direct Hit</a></h2>
      <a class="result__snippet" href="#">A plain snippet</a>
    </div>
    <div class="result">
      <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdirect.example.org%2Fother">Duplicate Host</a></h2>
    </div>
    </body></html>"##;

    #[test]
    fn parses_entities_uddg_and_collapses_hosts() {
        let results = parse_results(GOLDEN_PAGE, 10);
        assert_eq!(
            results.len(),
            2,
            "duplicate host must collapse: {results:?}"
        );
        assert_eq!(results[0].title, "Example Docs");
        assert_eq!(results[0].url, "https://example.com/docs");
        assert_eq!(results[0].snippet, "The \"official\" & friendly guide <v2>");
        assert_eq!(results[1].title, "Direct Hit");
        assert_eq!(results[1].url, "https://direct.example.org/page");
        assert_eq!(results[1].snippet, "A plain snippet");
    }

    #[test]
    fn respects_result_cap() {
        let results = parse_results(GOLDEN_PAGE, 1);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn percent_decode_and_query_encoding() {
        assert_eq!(
            percent_decode("https%3A%2F%2Fexample.com%2Fa%20b"),
            "https://example.com/a b"
        );
        assert_eq!(encode_query("rust async run"), "rust%20async%20run");
    }
}
