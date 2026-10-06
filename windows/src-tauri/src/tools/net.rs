// `fetch_url`: one GET, bounded.
//
// The model asks for a page; this is the only way anything crosses the network
// from a tool, so it is deliberately dull: GET only, a short timeout, and a
// hard cap on the body. A redirect is followed by reqwest's default policy
// (up to 10 hops); the body is truncated and says so.

use std::time::Duration;

use serde_json::json;

use crate::claude::ToolResult;

/// Cap on the response body. Past this the tool cuts and says so.
const MAX_BODY: usize = 512 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

/// Fetches `url` and returns the status plus the body as text.
pub async fn fetch_url(id: &str, url: &str, method: Option<&str>) -> ToolResult {
    let verb = method.unwrap_or("GET");
    if !verb.eq_ignore_ascii_case("GET") {
        return ToolResult::err(
            id,
            format!("{verb} não é permitido: fetch_url só faz GET"),
        );
    }

    // Only http(s): a `file://` here would read the disk without the tool
    // rules ever seeing it.
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return ToolResult::err(id, format!("'{url}' não é http(s)"));
    }

    let client = match reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => return ToolResult::err(id, format!("cliente http: {e}")),
    };

    let response = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => return ToolResult::err(id, format!("{url}: {e}")),
    };

    let status = response.status();
    let final_url = response.url().to_string();
    let declared = response.content_length();

    let bytes = match response.bytes().await {
        Ok(b) => b,
        Err(e) => return ToolResult::err(id, format!("{url}: leitura falhou: {e}")),
    };

    let truncated = bytes.len() > MAX_BODY;
    let slice = if truncated {
        &bytes[..MAX_BODY]
    } else {
        &bytes[..]
    };
    // Lossy on purpose: a page in another encoding is still worth reading.
    let text = String::from_utf8_lossy(slice).into_owned();

    ToolResult::ok(
        id,
        json!({
            "url": final_url,
            "status": status.as_u16(),
            "ok": status.is_success(),
            "bytes": bytes.len(),
            "truncated": truncated,
            "declared_bytes": declared,
            "body": text,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_non_get_method_is_refused() {
        let result = fetch_url("t", "https://example.com", Some("POST")).await;
        assert!(result.is_error);
        assert!(result.content.as_str().unwrap().contains("POST"));
    }

    #[tokio::test]
    async fn a_non_http_scheme_is_refused() {
        for url in ["file:///C:/Windows/System32", "ftp://example.com", "not a url"] {
            let result = fetch_url("t", url, None).await;
            assert!(result.is_error, "{url} deveria ser recusado");
            assert!(result.content.as_str().unwrap().contains("http"));
        }
    }

    #[tokio::test]
    async fn an_unreachable_host_is_an_error_not_a_panic() {
        // Port 1 on loopback refuses immediately, so this exercises the error
        // path without waiting out the production timeout.
        let result = fetch_url("t", "http://127.0.0.1:1/", None).await;
        assert!(result.is_error);
    }
}
