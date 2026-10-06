// OpenAI-compatible chat client.
//
// One client, several agents: Codex and every OpenAI-compatible gateway answer on
// `/chat/completions`, Kimi answers on the same shape at api.moonshot.ai, and a
// local gateway (LM Studio, llama.cpp, vLLM…) is the same call with another
// address. So the agent only decides the key we read and the defaults the
// settings window offers.
//
// Everything happens here rather than in the island: the API key never leaves the
// Credential Manager, and file bytes never cross the IPC boundary.

use std::time::Duration;

use serde_json::{json, Value};

use crate::claude::{base64_for, ChatContext, ChatReply};

const MAX_TOKENS: u32 = 4096;
/// Same cap as the Claude path: text and code are inlined, bigger files are skipped.
const MAX_INLINE_TEXT: u64 = 200_000;
const TIMEOUT: Duration = Duration::from_secs(90);

/// One chat turn. `api_key` is the provider's key, already read from the
/// Credential Manager by the caller.
pub async fn send(
    history: &mut Vec<Value>,
    model: &str,
    base_url: &str,
    api_key: &str,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let model = model.trim();
    if model.is_empty() {
        return Err("No model selected. Pick one in settings.".to_string());
    }
    let url = endpoint(base_url)?;

    // The persona is the system message: every OpenAI-compatible endpoint takes
    // one, and no markdown keeps the island readable.
    let mut messages: Vec<Value> = vec![json!({
        "role": "system",
        "content": crate::claude::LOCAL_SYSTEM_PROMPT,
    })];
    // Everything said so far, so the second turn knows the first.
    messages.extend(history.iter().cloned());

    // File / window context rides along with the first message only, exactly
    // like the Claude path.
    if history.is_empty() {
        match &context {
            Some(ChatContext::File { name, path }) => {
                if let Some(parts) = file_parts(path) {
                    messages.push(json!({ "role": "user", "content": parts }));
                }
                messages.push(json!({
                    "role": "user",
                    "content": format!("File: {name}"),
                }));
            }
            Some(ChatContext::Window { app_name, title, url: page }) => {
                let mut text = format!("Context — App: {app_name}, Window: {title}");
                if let Some(page) = page {
                    text.push_str(&format!(", URL: {page}"));
                }
                messages.push(json!({ "role": "user", "content": text }));
            }
            None => {}
        }
    }

    // One content value, used both in the request and in the history we keep.
    let user = json!({ "role": "user", "content": query });
    messages.push(user.clone());
    history.push(user);

    let body = json!({
        "model": model,
        "messages": messages,
        "max_tokens": MAX_TOKENS,
        "stream": false,
    });

    let client = reqwest::Client::builder().timeout(TIMEOUT).build().map_err(|e| e.to_string())?;
    let response = match client
        .post(&url)
        .header("authorization", format!("Bearer {api_key}"))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => {
            // The failed turn leaves the history exactly as it was.
        history.pop();
            return Err(network_error(base_url, &err));
        }
    };

    let status = response.status();
    let raw = match response.text().await {
        Ok(t) => t,
        Err(err) => {
            history.pop();
            return Err(err.to_string());
        }
    };
    if !status.is_success() {
        history.pop();
        // Surface the API's own message: that is what makes a bad key or a model
        // name the account cannot reach obvious.
        let detail = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| raw.chars().take(200).collect());
        return Err(format!("API {status}: {detail}"));
    }

    let text = match parse_reply(&raw) {
        Ok(t) => t,
        Err(err) => {
            history.pop();
            return Err(err);
        }
    };

    history.push(json!({ "role": "assistant", "content": text.clone() }));
    Ok(ChatReply { text })
}

/// `{base}/chat/completions`, with the scheme filled in when it was left out.
fn endpoint(base_url: &str) -> Result<String, String> {
    let mut base = base_url.trim().trim_end_matches('/').to_string();
    if base.is_empty() {
        return Err("No API address. Set it in settings.".to_string());
    }
    if !base.starts_with("http://") && !base.starts_with("https://") {
        base = format!("https://{base}");
    }
    Ok(format!("{base}/chat/completions"))
}

/// A bad key says "incorrect api key"; a wrong address says something else
/// entirely. Saying which one happened saves the round trip.
fn network_error(base_url: &str, err: &reqwest::Error) -> String {
    if err.is_timeout() {
        return "The API took too long to answer.".to_string();
    }
    let base = if base_url.trim().is_empty() { "the address in settings" } else { base_url.trim() };
    format!("Cannot reach {base}: {err}")
}

/// The file as content parts: text inlined, images as data URIs.
fn file_parts(path: &str) -> Option<Vec<Value>> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let media_type = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "",
    };
    if !media_type.is_empty() {
        let bytes = std::fs::read(path).ok()?;
        return Some(vec![json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{media_type};base64,{}", base64_for(&bytes)) },
        })]);
    }

    // A PDF is not something this shape can carry; the file name still is, which
    // is what the island appends next.
    if ext == "pdf" {
        return None;
    }

    let len = std::fs::metadata(path).ok()?.len();
    if len > MAX_INLINE_TEXT {
        return None;
    }
    let body = std::fs::read_to_string(path).ok()?;
    Some(vec![json!({
        "type": "text",
        "text": format!("File contents:\n{body}"),
    })])
}

fn parse_reply(body: &str) -> Result<String, String> {
    let json: Value = serde_json::from_str(body).map_err(|e| format!("Bad API response: {e}"))?;
    let text = json
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return Err("No response text.".to_string());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::{endpoint, parse_reply};

    #[test]
    fn endpoint_fills_in_the_scheme_and_the_path() {
        assert_eq!(
            endpoint("https://api.openai.com/v1").unwrap(),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            endpoint("api.moonshot.ai/v1/").unwrap(),
            "https://api.moonshot.ai/v1/chat/completions"
        );
        assert_eq!(
            endpoint("http://127.0.0.1:1234/v1").unwrap(),
            "http://127.0.0.1:1234/v1/chat/completions"
        );
        assert!(endpoint("  ").is_err());
    }

    #[test]
    fn reply_text_is_read_from_the_first_choice() {
        let body = r#"{"id":"chatcmpl-1","choices":[{"index":0,"message":{"role":"assistant","content":" hi "},"finish_reason":"stop"}]}"#;
        assert_eq!(parse_reply(body).unwrap(), "hi");
    }

    /// The shape a reasoning model returns when it puts text in `reasoning`
    /// first, and an empty one — neither may crash the island.
    #[test]
    fn a_missing_or_empty_message_is_an_error_not_a_panic() {
        assert!(parse_reply(r#"{"choices":[]}"#).is_err());
        assert!(parse_reply(r#"{"choices":[{"message":{"content":"   "}}]}"#).is_err());
        assert!(parse_reply(r#"{"choices":[{"message":{}}]}"#).is_err());
        assert!(parse_reply("not json").is_err());
    }
}