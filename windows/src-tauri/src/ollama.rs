// Ollama — chat and model list against an Ollama daemon on the user's own
// machine (default http://127.0.0.1:11434).
//
// Nothing is stored in the Credential Manager here: there is no API key, and the
// daemon is localhost unless the user points the address somewhere else. Every
// request is built and read in Rust, exactly like the Claude path, so a file's
// bytes never cross the IPC boundary.

use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::claude::{base64_for, ChatContext, ChatReply};

/// Provider ids shared with the settings window.
pub const PROVIDER_ANTHROPIC: &str = "anthropic";
pub const PROVIDER_OLLAMA: &str = "ollama";

pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:11434";

/// Listing models is a UI convenience: fail fast rather than hang the settings
/// window when the daemon is not running.
const TAGS_TIMEOUT: Duration = Duration::from_secs(4);
/// A local model can take a while on the first call — it is being paged into RAM.
const CHAT_TIMEOUT: Duration = Duration::from_secs(180);
/// Same cap as the Claude path: text and code are inlined, bigger files are skipped.
const MAX_INLINE_TEXT: u64 = 200_000;
/// Enough room for the Mochi persona, a dropped file and a few turns.
const NUM_CTX: u32 = 16_384;

/// Same character as the Claude prompt, minus the web search claim — a local
/// model has no tools here, and it should not pretend otherwise.
const SYSTEM_PROMPT: &str = "You are Mochi, a personal AI assistant living at the top of the user's screen. \
You run entirely on the user's own computer. \
Respond in the user's language. Be thorough and complete — use as much detail as the task requires. \
No markdown formatting (no **, no ##, no bullet dashes). Use plain text with line breaks.";

/// One entry of `GET /api/tags`.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LocalModel {
    pub name: String,
    pub size: u64,
    pub family: Option<String>,
    pub parameter_size: Option<String>,
    /// `-cloud` variants are served by Ollama's servers, not on this machine.
    pub cloud: bool,
}

/// Which model the daemon should be asked for, when settings has none.
///
/// A local model is what we want: the `-cloud` entries belong to Ollama's
/// servers, and asking for one with no key behind it just fails differently.
/// Returns None when the daemon has nothing local, so the caller can say so
/// rather than picking a name that will not answer.
pub async fn preferred_model(base_url: &str) -> Option<String> {
    let list = models(base_url).await.ok()?;
    list.iter()
        .find(|m| !m.cloud)
        .map(|m| m.name.clone())
}

/// Why the chat cannot start, in words that say what to do about it.
///
/// "No model selected" on its own left people hunting through the settings
/// window; Ollama is the one provider with no default, because the model only
/// exists once it has been pulled.
fn no_model_message() -> &'static str {
    "Ollama has no model selected. Open settings, press Detect models next to \
     the Model field, and pick one. If the list is empty, run: ollama pull llama3.2"
}

/// `GET {base}/api/tags` - the models the daemon actually has.
pub async fn models(base_url: &str) -> Result<Vec<LocalModel>, String> {
    let url = endpoint(base_url, "/api/tags")?;
    let client = reqwest::Client::builder()
        .timeout(TAGS_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| unreachable_error(base_url, &e))?;
    let status = response.status();
    let body = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("Ollama answered {status}."));
    }
    parse_tags(&body)
}

/// One chat turn against `POST {base}/api/chat`.
pub async fn send(
    history: &mut Vec<Value>,
    base_url: &str,
    model: &str,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let model = model.trim();
    if model.is_empty() {
        return Err(no_model_message().to_string());
    }
    let url = endpoint(base_url, "/api/chat")?;

    let mut text = String::new();
    let mut images: Vec<String> = Vec::new();

    // File / window context rides along with the first message only, like the
    // Claude path. Ollama takes images as base64 next to the message, so they go
    // in their own field rather than in the text.
    if history.is_empty() {
        match &context {
            Some(ChatContext::File { name, path }) => {
                if let Some(file) = attachment(path) {
                    text.push_str(&file.text);
                    images.extend(file.images);
                }
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&format!("File: {name}"));
            }
            Some(ChatContext::Window { app_name, title, url: page }) => {
                text.push_str(&format!("Context — App: {app_name}, Window: {title}"));
                if let Some(page) = page {
                    text.push_str(&format!(", URL: {page}"));
                }
            }
            None => {}
        }
    }
    if !text.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str(&query);

    let mut user = json!({ "role": "user", "content": text });
    if !images.is_empty() {
        user["images"] = json!(images);
    }
    history.push(user);

    let body = json!({
        "model": model,
        "system": SYSTEM_PROMPT,
        "messages": history.clone(),
        "stream": false,
        "options": { "num_ctx": NUM_CTX },
    });

    let client = reqwest::Client::builder()
        .timeout(CHAT_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;

    let response = match client.post(&url).json(&body).send().await {
        Ok(r) => r,
        Err(err) => {
            history.pop(); // keep the history consistent with what the model saw
            return Err(unreachable_error(base_url, &err));
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
        // Surface the daemon's own message — that is what makes a bad model name
        // ("model not found, try pulling it first") obvious.
        let detail = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| raw.chars().take(200).collect());
        return Err(format!("Ollama {status}: {detail}"));
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

/// The inlined file, split into the two shapes Ollama understands.
struct Attachment {
    text: String,
    images: Vec<String>,
}

/// Text and code are inlined; images ride along as base64; a PDF is not
/// supported by the local API, so only its name reaches the model.
fn attachment(path: &str) -> Option<Attachment> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "gif" | "webp") {
        let bytes = std::fs::read(path).ok()?;
        return Some(Attachment { text: String::new(), images: vec![base64_for(&bytes)] });
    }

    if ext == "pdf" {
        return Some(Attachment {
            text: "A PDF was attached; the local model cannot read it.".to_string(),
            images: Vec::new(),
        });
    }

    let len = std::fs::metadata(path).ok()?.len();
    if len > MAX_INLINE_TEXT {
        return None;
    }
    let body = std::fs::read_to_string(path).ok()?;
    Some(Attachment { text: format!("File contents:\n{body}"), images: Vec::new() })
}

/// Joins the configured address to an API path, defaulting to plain HTTP so
/// "127.0.0.1:11434" works without ceremony.
fn endpoint(base_url: &str, path: &str) -> Result<String, String> {
    let mut base = base_url.trim().trim_end_matches('/').to_string();
    if base.is_empty() {
        return Err("No Ollama address. Set it in settings.".to_string());
    }
    if !base.starts_with("http://") && !base.starts_with("https://") {
        base = format!("http://{base}");
    }
    Ok(format!("{base}{path}"))
}

/// "Ollama is not running" is by far the most common failure, and the raw
/// reqwest text ("error sending request for url ...") says nothing useful.
fn unreachable_error(base_url: &str, err: &reqwest::Error) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if err.is_connect() {
        return format!("Cannot reach Ollama at {base}. Is it running?");
    }
    if err.is_timeout() {
        return format!("Ollama at {base} took too long to answer.");
    }
    format!("Network error: {err}")
}

fn parse_tags(body: &str) -> Result<Vec<LocalModel>, String> {
    let json: Value = serde_json::from_str(body).map_err(|e| format!("Bad model list: {e}"))?;
    let Some(list) = json.get("models").and_then(Value::as_array) else {
        return Err("Bad model list: no models.".to_string());
    };

    Ok(list
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name").and_then(Value::as_str)?.to_string();
            let details = entry.get("details");
            let cloud = name.ends_with("-cloud") || name.ends_with(":cloud");
            Some(LocalModel {
                cloud,
                family: details
                    .and_then(|d| d.get("family"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                parameter_size: details
                    .and_then(|d| d.get("parameter_size"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                size: entry.get("size").and_then(Value::as_u64).unwrap_or(0),
                name,
            })
        })
        .collect())
}

fn parse_reply(body: &str) -> Result<String, String> {
    let json: Value = serde_json::from_str(body).map_err(|e| format!("Bad API response: {e}"))?;
    let text = json
        .get("message")
        .and_then(|m| m.get("content"))
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
    use super::{endpoint, no_model_message, parse_reply, parse_tags, preferred_model};

    /// A daemon that answers `/api/tags` with the given JSON, on a port the OS
    /// picked. Returns the address to hand to the functions under test.
    fn fake_daemon(tags_body: &str) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("porta livre");
        let port = listener.local_addr().expect("endereco").port();
        let body = tags_body.to_string();

        std::thread::spawn(move || {
            // One connection is enough: each test asks a single question.
            let Ok((mut stream, _)) = listener.accept() else { return };
            let mut head = [0u8; 2048];
            let _ = stream.read(&mut head);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.flush();
        });

        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn preferred_model_chooses_a_local_model_over_a_cloud_one() {
        // The order is the trap: the cloud model comes first in the list, and
        // asking Ollama's servers for one without a key fails in a way that
        // looks like the chat is broken.
        let base = fake_daemon(
            r#"{"models":[
                {"name":"kimi-k3:cloud"},
                {"name":"qwen2.5:0.5b"},
                {"name":"gemma4:31b-cloud"}
            ]}"#,
        );
        assert_eq!(
            preferred_model(&base).await.as_deref(),
            Some("qwen2.5:0.5b"),
            "escolheu a cloud quando havia local"
        );
    }

    #[tokio::test]
    async fn preferred_model_is_none_when_only_cloud_models_exist() {
        let base = fake_daemon(r#"{"models":[{"name":"gemma4:31b-cloud"}]}"#);
        assert_eq!(
            preferred_model(&base).await,
            None,
            "inventar um nome de cloud seria pior do que dizer que nao ha"
        );
    }

    #[tokio::test]
    async fn preferred_model_is_none_when_the_daemon_is_absent() {
        // Port 1 is reserved and nothing listens there, so this is the
        // "Ollama is not running" path without needing to stop anything.
        assert_eq!(preferred_model("http://127.0.0.1:1").await, None);
    }

    #[test]
    fn the_no_model_error_says_where_to_go() {
        let message = no_model_message();
        assert!(message.contains("Detect models"), "{message}");
        assert!(
            message.contains("ollama pull"),
            "a mensagem nao diz o que fazer quando a lista esta vazia: {message}"
        );
    }

    #[test]
    fn endpoint_defaults_to_plain_http_and_drops_trailing_slash() {
        assert_eq!(
            endpoint("127.0.0.1:11434", "/api/chat").unwrap(),
            "http://127.0.0.1:11434/api/chat"
        );
        assert_eq!(
            endpoint("http://localhost:11434/", "/api/tags").unwrap(),
            "http://localhost:11434/api/tags"
        );
        assert_eq!(
            endpoint(" https://ollama.example.com ", "/api/chat").unwrap(),
            "https://ollama.example.com/api/chat"
        );
    }

    #[test]
    fn endpoint_refuses_an_empty_address() {
        assert!(endpoint("   ", "/api/chat").is_err());
    }

    #[test]
    fn tags_are_read_with_their_details() {
        // Shapes taken from a real `GET /api/tags`: a local gguf model and a
        // `-cloud` one that Ollama serves from its own hosts.
        let models = parse_tags(
            r#"{"models":[
                {"name":"qwen2.5:0.5b","model":"qwen2.5:0.5b",
                 "modified_at":"2026-10-04T17:48:46.9896785-03:00","size":397821319,
                 "digest":"a8b0c51577010a279d933d14c2a8ab4b268079d44c5c8830c0a93900f1827c67",
                 "details":{"parent_model":"","format":"gguf","family":"qwen2",
                            "families":["qwen2"],"parameter_size":"494.03M",
                            "quantization_level":"Q4_K_M","context_length":32768},
                 "capabilities":["completion","tools"]},
                {"name":"kimi-k3:cloud","model":"kimi-k3:cloud","remote_model":"kimi-k3",
                 "remote_host":"https://ollama.com","size":308,
                 "details":{"family":"kimi"}}
            ]}"#,
        )
        .unwrap();

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "qwen2.5:0.5b");
        assert_eq!(models[0].size, 397821319);
        assert_eq!(models[0].family.as_deref(), Some("qwen2"));
        assert_eq!(models[0].parameter_size.as_deref(), Some("494.03M"));
        assert!(!models[0].cloud);
        assert!(models[1].cloud);
        assert_eq!(models[1].family.as_deref(), Some("kimi"));
    }

    #[test]
    fn a_cloud_model_without_details_is_still_listed() {
        let models = parse_tags(r#"{"models":[{"name":"gemma4:31b-cloud","size":312}]}"#).unwrap();
        assert_eq!(models.len(), 1);
        assert!(models[0].cloud);
        assert_eq!(models[0].size, 312);
        assert_eq!(models[0].family, None);
    }

    #[test]
    fn an_empty_or_broken_model_list_is_not_a_panic() {
        assert_eq!(parse_tags(r#"{"models":[]}"#).unwrap().len(), 0);
        assert!(parse_tags("not json").is_err());
        assert!(parse_tags(r#"{"other":1}"#).is_err());
        // No `name` — skipped rather than half-built.
        assert_eq!(parse_tags(r#"{"models":[{"size":1}]}"#).unwrap().len(), 0);
    }

    #[test]
    fn reply_text_comes_from_the_message_object() {
        assert_eq!(
            parse_reply(r#"{"model":"llama3.2","message":{"role":"assistant","content":" hi "},"done":true}"#)
                .unwrap(),
            "hi"
        );
        assert!(parse_reply(r#"{"message":{"content":"   "}}"#).is_err());
        assert!(parse_reply(r#"{"done":true}"#).is_err());
        assert!(parse_reply("not json").is_err());
    }
}