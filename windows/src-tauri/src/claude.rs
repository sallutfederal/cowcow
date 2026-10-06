// Claude API client — the same integration as ClaudeService.swift: multi-turn
// chat with web search, and files sent as document/image/text blocks.
//
// Everything happens here rather than in the island: the API key never leaves
// the Credential Manager, and file bytes never cross the IPC boundary.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;

use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::secrets;
use crate::tools::{self, ToolDef};

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Server-side fallback: on a policy decline the API retries the same request on
/// a fallback model inside the same call, so the island never shows a dead end.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MAX_TOKENS: u32 = 4096;
/// Text and code files are inlined; anything larger is skipped, as on macOS.
const MAX_INLINE_TEXT: u64 = 200_000;

pub const DEFAULT_MODEL: &str = "claude-opus-5";

/// How many messages at the end of the history are never truncated.
const KEEP_RECENT: usize = 8;
/// Marker opening the block that stands in for a truncated history.
const SUMMARY_PREFIX: &str = "[resumo do histórico anterior:";

const SYSTEM_PROMPT: &str = "You are Mochi, a personal AI assistant living at the top of the user's screen. \
You have web search access and can help with absolutely anything — research, coding, finding places, recommendations, tasks, questions. \
Respond in the user's language. Be thorough and complete — use as much detail as the task requires. \
No markdown formatting (no **, no ##, no bullet dashes). Use plain text with line breaks.";

/// The same character for every other provider. No web search claim: those APIs
/// give us no tool here, and it should not pretend otherwise.
pub(crate) const LOCAL_SYSTEM_PROMPT: &str = "You are Mochi, a personal AI assistant living at the top of the user's screen. \
Respond in the user's language. Be thorough and complete — use as much detail as the task requires. \
No markdown formatting (no **, no ##, no bullet dashes). Use plain text with line breaks.";

#[derive(Default)]
pub struct Chat {
    /// Full multi-turn history, including tool_use / tool_result blocks.
    messages: Mutex<Vec<Value>>,
}

impl Chat {
    pub fn reset(&self) {
        self.messages.lock().unwrap().clear();
    }

    fn is_empty(&self) -> bool {
        self.messages.lock().unwrap().is_empty()
    }

    fn snapshot(&self) -> Vec<Value> {
        self.messages.lock().unwrap().clone()
    }

    /// Commits a finished turn. Only called once the model has answered: a turn
    /// that failed leaves the history exactly as it was.
    fn replace(&self, messages: Vec<Value>) {
        *self.messages.lock().unwrap() = messages;
    }
}

// ── the agent loop ──────────────────────────────────────────────────────────

/// Limits for one ReAct loop.
#[derive(Debug, Clone)]
pub struct AgentTurn {
    /// Hard cap on API calls. Past it the loop gives up rather than spending.
    pub max_iters: usize,
    /// Seconds a single tool may run before it is killed.
    #[allow(dead_code)] // read by shell.rs, which enforces it per command
    pub tool_timeout_s: u64,
    /// Above this many tokens the history is summarised before the next call.
    pub budget_tokens: usize,
}

impl Default for AgentTurn {
    fn default() -> Self {
        Self {
            max_iters: 24,
            tool_timeout_s: 60,
            budget_tokens: 180_000,
        }
    }
}

/// What the agent is allowed to touch, for one turn.
///
/// The three fields are read by the tools themselves — `cwd` and `dry_run` by
/// the filesystem tools, `allowed_shell` by `run_shell` — so this struct is the
/// single thing standing between the model and the machine.
#[derive(Debug, Clone)]
#[allow(dead_code)] // read by fs.rs / shell.rs, which land with the tools
pub struct ToolCtx {
    pub cwd: PathBuf,
    /// First token of a command that `run_shell` will accept. Empty blocks
    /// everything: a tool that cannot run anything is a tool the user did not
    /// authorise.
    pub allowed_shell: Vec<String>,
    /// When true, writing tools report what they would do and touch nothing.
    pub dry_run: bool,
    /// How long `run_shell` may run before the tree is killed.
    ///
    /// This lives here and not only on `AgentTurn` because `execute_tool` sees
    /// nothing but the context, and the timeout is the one limit a tool needs to
    /// enforce by itself. `AgentTurn::tool_timeout_s` is the default it is
    /// built from.
    pub tool_timeout_s: u64,
}

impl ToolCtx {
    /// The starting point: the process's own directory, nothing runnable, and
    /// every writing tool in report-only mode.
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            allowed_shell: Vec::new(),
            dry_run: true,
            tool_timeout_s: AgentTurn::default().tool_timeout_s,
        }
    }
}

impl Default for ToolCtx {
    fn default() -> Self {
        Self::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}

/// The outcome of one tool call, already shaped for the API.
#[derive(Debug, Clone, Serialize)]
pub struct ToolResult {
    pub id: String,
    pub content: Value,
    pub is_error: bool,
}

impl ToolResult {
    #[allow(dead_code)] // every tool returns a result; this is the success arm
    pub fn ok(id: &str, content: Value) -> Self {
        Self {
            id: id.to_string(),
            content,
            is_error: false,
        }
    }

    pub fn err(id: &str, message: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            content: Value::String(message.into()),
            is_error: true,
        }
    }

    /// The `tool_result` block the next user turn carries.
    ///
    /// `content` goes on the wire as a string: the API accepts a string or an
    /// array of content blocks, so a tool returning structured JSON has to be
    /// serialised rather than handed over as an object.
    pub fn to_block(&self) -> Value {
        let content = match &self.content {
            Value::String(text) => Value::String(text.clone()),
            other => Value::String(other.to_string()),
        };
        json!({
            "type": "tool_result",
            "tool_use_id": self.id,
            "content": content,
            "is_error": self.is_error,
        })
    }
}

/// What a `stop_reason` means for the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopAction {
    /// The model is done talking.
    Finish,
    /// The model wants tools run before it continues.
    Tools,
}

fn stop_action(stop: &str) -> Result<StopAction, String> {
    match stop {
        "end_turn" | "stop_sequence" | "max_tokens" => Ok(StopAction::Finish),
        "tool_use" => Ok(StopAction::Tools),
        other => Err(format!("stop_reason inesperado: {other}")),
    }
}

/// One line per iteration, for the app log: which call, and why it stopped.
///
/// Split in two so the wording is unit-testable without a test run appending to
/// the user's `%LOCALAPPDATA%\Coucou\coucou.log`.
fn trace_line(iter: usize, stop: &str) -> String {
    format!("agent iter={iter} stop={stop}")
}

#[cfg(not(test))]
fn trace(iter: usize, stop: &str) {
    crate::log::line(trace_line(iter, stop));
}

/// A test run must not touch the real log file.
#[cfg(test)]
fn trace(_iter: usize, _stop: &str) {}

/// Rough token count: four characters per token. Cheap, and only ever used to
/// decide when the history needs trimming — never billed.
fn approx_tokens(chat: &[Value]) -> usize {
    let chars: usize = chat
        .iter()
        .map(|message| message.to_string().chars().count())
        .sum();
    chars / 4 + 1
}

/// True for a user turn that is nothing but `tool_result` blocks.
///
/// Cutting the history straight through one of these orphans the `tool_use`
/// that asked for it, which the API rejects — so the cut walks forward until
/// it lands on a turn that can start a conversation.
fn is_tool_result_only(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    match message.get("content") {
        Some(Value::Array(blocks)) => {
            !blocks.is_empty()
                && blocks.iter().all(|b| {
                    b.get("type").and_then(Value::as_str) == Some("tool_result")
                })
        }
        _ => false,
    }
}

/// Trims the history to fit the budget, oldest first.
///
/// The last [`KEEP_RECENT`] messages stay verbatim. Everything older becomes
/// one summary block. When the cut point lands inside a tool exchange it slides
/// forward to the next turn that can legally open the conversation; if there is
/// none, the history is left alone rather than corrupted.
fn fit_budget(chat: &mut Vec<Value>, budget_tokens: usize) {
    if chat.len() <= KEEP_RECENT || approx_tokens(chat) <= budget_tokens {
        return;
    }

    let mut head = chat.len() - KEEP_RECENT;
    while head < chat.len() && !can_open(chat, head) {
        head += 1;
    }
    if head >= chat.len() {
        return;
    }

    let dropped: Vec<Value> = chat.drain(..head).collect();
    prepend_summary(chat, &summarise(&dropped));
}

/// Whether the message at `index` may become the first one on the wire.
fn can_open(chat: &[Value], index: usize) -> bool {
    match chat.get(index).and_then(|m| m.get("role")).and_then(Value::as_str) {
        Some("user") => !is_tool_result_only(&chat[index]),
        _ => false,
    }
}

/// An extractive digest of the messages about to be dropped.
///
/// Phase 1 keeps this local and cheap: one bullet per turn, capped, so the
/// budget holds without an extra API call. Phase 4 replaces the body with a
/// model-written summary.
fn summarise(dropped: &[Value]) -> Vec<String> {
    const MAX_BULLETS: usize = 40;
    let mut bullets: Vec<String> = Vec::new();

    for message in dropped {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("?");
        match message.get("content") {
            Some(Value::String(text)) => push_bullet(&mut bullets, role, "text", text),
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                push_bullet(&mut bullets, role, "text", text);
                            }
                        }
                        Some("tool_use") => {
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("ferramenta");
                            bullets.push(format!("{role} usou {name}"));
                        }
                        Some("tool_result") => {
                            let failed = block
                                .get("is_error")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            bullets.push(format!("{role} recebeu o resultado{failed}"));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    if bullets.len() > MAX_BULLETS {
        bullets.truncate(MAX_BULLETS);
        bullets.push(format!("(mais {} turnos omitidos)", dropped.len()));
    }
    bullets
}

/// One bullet, on a single line and capped, so a long turn cannot blow the
/// budget it was added to protect.
fn push_bullet(bullets: &mut Vec<String>, role: &str, _kind: &str, text: &str) {
    const MAX_CHARS: usize = 160;
    let line: String = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .take(MAX_CHARS)
        .collect();
    if line.is_empty() {
        return;
    }
    bullets.push(format!("{role}: {line}"));
}

/// Puts the summary at the front without ever producing two user turns in a
/// row: an existing user turn absorbs it as a leading text block.
fn prepend_summary(chat: &mut Vec<Value>, bullets: &[String]) {
    let summary = format!("{SUMMARY_PREFIX} {}]", bullets.join(" · "));

    if let Some(first) = chat.first_mut() {
        if first.get("role").and_then(Value::as_str) == Some("user") {
            match first.get_mut("content") {
                Some(Value::Array(blocks)) => {
                    blocks.insert(0, json!({ "type": "text", "text": summary }));
                }
                Some(Value::String(text)) => {
                    let merged = format!("{summary}\n{text}");
                    first["content"] = Value::String(merged);
                }
                _ => {}
            }
            return;
        }
    }
    chat.insert(0, json!({ "role": "user", "content": summary }));
}

/// The request body. Server-side web search comes first so the client-side
/// tools stay the last thing the API reads.
fn build_body(model: &str, system: &str, chat: &[Value], tools: &[ToolDef]) -> Value {
    let mut list = vec![json!({
        "type": "web_search_20260209",
        "name": "web_search",
        "max_uses": 5,
    })];
    list.extend(tools.iter().map(ToolDef::to_anthropic));

    json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "system": system,
        "tools": list,
        "fallbacks": "default",
        "messages": chat,
    })
}

/// Runs the ReAct loop over `chat`, calling `transport` once per iteration.
///
/// The transport is a parameter so the loop can be driven by a scripted
/// sequence in tests without touching the network — and so the key never has to
/// leave this function in a test.
async fn run_loop<F, Fut>(
    chat: &mut Vec<Value>,
    system: &str,
    tools: &[ToolDef],
    ctx: &ToolCtx,
    cfg: &AgentTurn,
    model: &str,
    mut transport: F,
) -> Result<ChatReply, String>
where
    F: FnMut(Value) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    let mut final_text = String::new();
    let mut calls = 0usize;

    loop {
        if calls >= cfg.max_iters {
            return Err("agent loop hit max_iters".to_string());
        }
        fit_budget(chat, cfg.budget_tokens);

        let response = transport(build_body(model, system, chat, tools)).await?;
        calls += 1;

        // A policy decline arrives as HTTP 200 with stop_reason "refusal".
        let stop = response
            .get("stop_reason")
            .and_then(Value::as_str)
            .unwrap_or_default();
        trace(calls, stop);

        if stop == "refusal" {
            let why = response
                .get("stop_details")
                .and_then(|d| d.get("explanation"))
                .and_then(Value::as_str)
                .unwrap_or("Claude recusou esta.");
            return Err(why.to_string());
        }

        let Some(blocks) = response.get("content").and_then(Value::as_array).cloned() else {
            return Err("Resposta inesperada da API.".into());
        };

        // Keep the whole content, tool_use included: the next iteration needs it.
        chat.push(json!({ "role": "assistant", "content": blocks.clone() }));

        let text = blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        let text = text.trim();
        if !text.is_empty() {
            if !final_text.is_empty() {
                final_text.push('\n');
            }
            final_text.push_str(text);
        }

        match stop_action(stop)? {
            StopAction::Finish => break,
            StopAction::Tools => {
                // Server-side tools (web search) resolve inside the same
                // response; only client-side `tool_use` blocks need us.
                let uses: Vec<&Value> = blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                    .collect();
                if uses.is_empty() {
                    break;
                }

                let results = join_all(uses.iter().map(|use_| {
                    let id = use_
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let name = use_
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let input = use_.get("input").cloned().unwrap_or_else(|| json!({}));
                    let ctx = ctx.clone();
                    // Owned by the future: nothing here may borrow a local.
                    async move { tools::execute_tool(&id, &name, &input, &ctx).await }
                }))
                .await;

                let blocks_out: Vec<Value> = results.iter().map(ToolResult::to_block).collect();
                chat.push(json!({ "role": "user", "content": blocks_out }));
            }
        }
    }

    if final_text.is_empty() {
        return Err("No response text.".into());
    }
    Ok(ChatReply {
        text: final_text,
    })
}

/// One agent turn: ask, run whatever tools the model asked for, ask again.
///
/// `model` is passed in because the body needs it; the API key is read here so
/// it never leaves this module.
pub async fn send_agent(
    chat: &mut Vec<Value>,
    model: &str,
    system: &str,
    tools: &[ToolDef],
    ctx: &ToolCtx,
    cfg: &AgentTurn,
) -> Result<ChatReply, String> {
    let key = secrets::get("anthropic-api-key")
        .ok_or_else(|| "API key missing. Open settings.".to_string())?;
    // The key is cloned into every request rather than borrowed: the future has
    // to own everything it touches, since it outlives this stack frame.
    let shared = key.clone();
    run_loop(chat, system, tools, ctx, cfg, model, move |body: Value| {
        let key = shared.clone();
        async move { call(&key, &body).await }
    })
    .await
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ChatContext {
    File { name: String, path: String },
    Window { app_name: String, title: String, url: Option<String> },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatReply {
    pub text: String,
}

/// One chat turn from the island: the model, the server-side web search, and
/// the client-side tools the user's settings allow.
///
/// A thin wrapper over [`send_agent`] so `chat_send` keeps its shape. The
/// history is committed only when the turn succeeds, which is why a failed
/// turn leaves the chat exactly as it was.
pub async fn send(
    chat: &Chat,
    model: &str,
    query: String,
    context: Option<ChatContext>,
    ctx: &ToolCtx,
) -> Result<ChatReply, String> {
    let mut messages = chat.snapshot();

    let mut content: Vec<Value> = Vec::new();

    // File / window context rides along with the first message only, exactly
    // like ClaudeService.chat().
    if chat.is_empty() {
        match &context {
            Some(ChatContext::File { name, path }) => {
                if let Some(block) = file_block(path) {
                    content.push(block);
                }
                content.push(json!({ "type": "text", "text": format!("File: {name}") }));
            }
            Some(ChatContext::Window { app_name, title, url }) => {
                let mut text = format!("Context — App: {app_name}, Window: {title}");
                if let Some(url) = url {
                    text.push_str(&format!(", URL: {url}"));
                }
                content.push(json!({ "type": "text", "text": text }));
            }
            None => {}
        }
    }
    content.push(json!({ "type": "text", "text": query }));
    messages.push(json!({ "role": "user", "content": content }));

    let reply = send_agent(
        &mut messages,
        model,
        SYSTEM_PROMPT,
        &tools::registry(),
        ctx,
        &AgentTurn::default(),
    )
    .await?;


    chat.replace(messages);
    Ok(reply)
}

async fn call(key: &str, body: &Value) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .build()
        .map_err(|e| e.to_string())?;

    let response = client
        .post(ENDPOINT)
        .header("x-api-key", key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("anthropic-beta", FALLBACK_BETA)
        .header("content-type", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;

    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        // Surface the API's own message, which is what makes a bad key obvious.
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!("Claude API {status}: {detail}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("Bad API response: {e}"))
}

/// PDF → document block, image → image block, text/code → inline text.
/// Mirrors readFileAsBlock() in ClaudeService.swift.
fn file_block(path: &str) -> Option<Value> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let media_type = match ext.as_str() {
        "pdf" => Some(("document", "application/pdf")),
        "jpg" | "jpeg" => Some(("image", "image/jpeg")),
        "png" => Some(("image", "image/png")),
        "gif" => Some(("image", "image/gif")),
        "webp" => Some(("image", "image/webp")),
        _ => None,
    };

    if let Some((block_type, media)) = media_type {
        let bytes = std::fs::read(path).ok()?;
        return Some(json!({
            "type": block_type,
            "source": { "type": "base64", "media_type": media, "data": base64(&bytes) },
        }));
    }

    let len = std::fs::metadata(path).ok()?.len();
    if len > MAX_INLINE_TEXT {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    Some(json!({ "type": "text", "text": format!("File contents:\n{text}") }))
}

/// Small standalone base64 encoder — not worth another dependency.
/// Also used for Stripe's basic auth.
pub(crate) fn base64_for(bytes: &[u8]) -> String {
    base64(bytes)
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    #[test]
    fn the_trace_line_names_the_iteration_and_the_stop_reason() {
        assert_eq!(trace_line(1, "end_turn"), "agent iter=1 stop=end_turn");
        assert_eq!(trace_line(12, "tool_use"), "agent iter=12 stop=tool_use");
    }

    #[test]
    fn the_body_carries_the_tool_schemas_in_the_shape_the_api_expects() {
        // The one thing a wrong tool array would break silently: without this,
        // the loop's mechanics are fine and every real call 400s.
        let (sent, transport) = scripted(vec![turn("end_turn", "ok")]);
        let mut chat = vec![];
        let tools = tools::registry();

        block_on(run_loop(
            &mut chat,
            "sys",
            &tools,
            &ctx(),
            &AgentTurn::default(),
            "claude-opus-5",
            transport,
        ))
        .expect("ok");

        let tools_sent = body(&sent, 0)["tools"].clone();
        let list = tools_sent.as_array().expect("tools is a list");
        // Server-side web search first, then the seven client-side ones.
        assert_eq!(list.len(), 8, "web_search + as sete ferramentas");
        assert_eq!(list[0]["type"], "web_search_20260209");

        for entry in &list[1..] {
            assert!(entry["name"].is_string(), "{entry}");
            assert!(entry["description"].is_string(), "{entry} sem descrição");
            // Anthropic's key is `input_schema`, not `schema` or `parameters`.
            let schema = entry
                .get("input_schema")
                .unwrap_or_else(|| panic!("{} sem input_schema: {entry}", entry["name"]));
            assert_eq!(schema["type"], "object", "{}", entry["name"]);
            assert!(
                schema["required"].is_array(),
                "{} sem required: {schema}",
                entry["name"]
            );
            // The client-side shape must not leak the server-only keys.
            assert!(entry.get("type").is_none(), "{}", entry["name"]);
        }

        let read = list
            .iter()
            .find(|t| t["name"] == "read_file")
            .expect("read_file registrado");
        let required = read["input_schema"]["required"].as_array().unwrap();
        assert!(required.contains(&json!("path")));
    }

    #[tokio::test]
    async fn the_tool_name_that_arrives_is_the_one_that_runs() {
        // A `tool_use` naming something real has to reach that tool, with its
        // `input` intact: a dispatcher that always answered "desconhecida" would
        // pass every other test here.
        let (sent, transport) = scripted(vec![
            json!({
                "stop_reason": "tool_use",
                "content": [tool_call(
                    "tu_1",
                    "write_file",
                    json!({ "path": "do-loop.txt", "content": "escrito pelo loop" }),
                )],
            }),
            turn("end_turn", "pronto"),
        ]);
        let mut chat = vec![];
        let dir = std::env::temp_dir().join(format!("coucou-dispatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let mut ctx = ToolCtx::new(dir.clone());
        ctx.dry_run = true;

        run_loop(
            &mut chat,
            "sys",
            &tools::registry(),
            &ctx,
            &AgentTurn::default(),
            "claude-opus-5",
            transport,
        )
        .await
        .expect("o loop fecha");

        assert_eq!(calls(&sent), 2);
        let result = &tool_result_turn(&chat)["content"][0];
        assert_eq!(result["tool_use_id"], "tu_1");
        // The tool it named ran, and its `input` arrived intact: dry-run write_file
        // reports the byte count of what it was handed, and "escrito pelo loop"
        // is exactly 17 bytes — a dispatcher that dropped or faked the input
        // could not produce that number.
        let payload = result["content"].as_str().unwrap_or_default();
        assert!(payload.contains("do-loop.txt"), "write_file não despachou: {payload}");
        assert!(
            payload.contains("\"bytes\":17"),
            "o input não chegou íntegro: {payload}"
        );
        assert!(
            !dir.join("do-loop.txt").exists(),
            "dry-run não pode escrever"
        );
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }


    /// What the loop needs from a transport: hand it a request body, get back
    /// whatever the API would have said. Boxed so the tests can return one
    /// without naming the closure's own type.
    type Transport = Box<dyn FnMut(Value) -> std::future::Ready<Result<Value, String>>>;

/// How the tests observe a loop: every body it sent, in order.
type Sent = Rc<RefCell<Vec<Value>>>;

/// Drives the loop from a scripted list of responses, recording every body
    /// the loop sent so the tests can assert on what it asked for.
    ///
    /// The recorder is shared because the closure has to own the queue; the
    /// caller gets a handle to read back what went out.
    fn scripted(responses: Vec<Value>) -> (Sent, Transport) {
        let mut queue = VecDeque::from(responses);
        let sent: Sent = Rc::new(RefCell::new(Vec::new()));
        let recorder = Rc::clone(&sent);
        let transport = move |body: Value| {
            recorder.borrow_mut().push(body);
            std::future::ready(Ok(queue.pop_front().unwrap_or_else(|| {
                json!({ "stop_reason": "end_turn", "content": [{ "type": "text", "text": "(sem resposta)" }] })
            })))
        };
        (sent, Box::new(transport))
    }

    /// How many calls the loop made.
    fn calls(sent: &Sent) -> usize {
        sent.borrow().len()
    }

    /// The body of the nth call.
    fn body(sent: &Sent, n: usize) -> Value {
        sent.borrow()[n].clone()
    }

    /// `tokio` is only pulled in for `rt`, so the current-thread runtime is
    /// built by hand: these tests never need a timer or a reactor.
    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime needs no async reactor")
            .block_on(future)
    }

    fn turn(stop: &str, text: &str) -> Value {
        json!({
            "stop_reason": stop,
            "content": [{ "type": "text", "text": text }],
        })
    }

    fn tool_call(id: &str, name: &str, input: Value) -> Value {
        json!({
            "id": id,
            "type": "tool_use",
            "name": name,
            "input": input,
        })
    }

/// A context pointing at an empty directory of its own.
///
/// Not `C:\`: the loop really dispatches whatever the scripted response asks
/// for, so a `list_dir` in a test would otherwise walk the whole disk.
fn ctx() -> ToolCtx {
    let dir = std::env::temp_dir().join(format!("coucou-loop-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    ToolCtx::new(dir)
}

    fn history(pairs: usize) -> Vec<Value> {
        let mut chat = Vec::new();
        for i in 0..pairs {
            chat.push(json!({ "role": "user", "content": format!("pergunta {i}") }));
            chat.push(json!({ "role": "assistant", "content": [{ "type": "text", "text": format!("resposta {i}") }] }));
        }
        chat
    }

    #[test]
    fn a_plain_answer_stops_after_one_call() {
        let (sent, transport) = scripted(vec![turn("end_turn", "olá")]);
        let mut chat = vec![json!({ "role": "user", "content": "diga olá" })];

        let reply = block_on(run_loop(
            &mut chat,
            "sys",
            &[],
            &ctx(),
            &AgentTurn::default(),
            "claude-opus-5",
            transport,
        ))
        .expect("one call is enough");

        assert_eq!(reply.text, "olá");
        assert_eq!(calls(&sent), 1, "end_turn must not loop");
        // The assistant turn is kept so the next question has context.
        assert_eq!(chat.len(), 2);
        assert_eq!(chat[1]["role"], "assistant");
    }

    #[test]
    fn every_call_carries_the_model_and_the_server_side_search() {
        let (sent, transport) = scripted(vec![turn("end_turn", "ok")]);
        let mut chat = vec![];
        block_on(run_loop(
            &mut chat,
            "sys",
            &[],
            &ctx(),
            &AgentTurn::default(),
            "claude-sonnet-5",
            transport,
        ))
        .expect("ok");

        let first = body(&sent, 0);
        assert_eq!(first["model"], "claude-sonnet-5");
        assert_eq!(first["system"], "sys");
        let tools = first["tools"].as_array().expect("tools is a list");
        assert_eq!(tools[0]["type"], "web_search_20260209");
    }

    #[test]
    fn a_tool_use_becomes_a_tool_result_and_the_loop_asks_again() {
        let (sent, transport) = scripted(vec![
            json!({
                "stop_reason": "tool_use",
                "content": [tool_call("tu_1", "read_file", json!({ "path": "a.rs" }))],
            }),
            turn("end_turn", "li o arquivo"),
        ]);
        let mut chat = vec![json!({ "role": "user", "content": "le a.rs" })];

        let reply = block_on(run_loop(
            &mut chat,
            "sys",
            &[],
            &ctx(),
            &AgentTurn::default(),
            "claude-opus-5",
            transport,
        ))
        .expect("the loop finishes on the second call");

        assert_eq!(calls(&sent), 2);
        assert_eq!(reply.text, "li o arquivo");
        // user, assistant(tool_use), user(tool_result), assistant(text)
        assert_eq!(chat.len(), 4);
        let result = &tool_result_turn(&chat)["content"][0];
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["tool_use_id"], "tu_1");
        // No tool is registered in this phase, so the call comes back an error
        // rather than silently pretending it worked.
        assert_eq!(result["is_error"], true);
    }

    #[test]
    fn text_before_a_tool_call_is_kept_and_joined_with_the_final_answer() {
        let (sent, transport) = scripted(vec![
            json!({
                "stop_reason": "tool_use",
                "content": [
                    { "type": "text", "text": "deixa eu ver" },
                    tool_call("tu_1", "list_dir", json!({})),
                ],
            }),
            turn("end_turn", "são 3 arquivos"),
        ]);
        let mut chat = vec![];
        let reply = block_on(run_loop(
            &mut chat,
            "sys",
            &[],
            &ctx(),
            &AgentTurn::default(),
            "claude-opus-5",
            transport,
        ))
        .expect("ok");

        assert_eq!(calls(&sent), 2);
        assert_eq!(reply.text, "deixa eu ver\nsão 3 arquivos");
    }

    #[test]
    fn two_tool_calls_in_one_turn_both_come_back() {
        let (_sent, transport) = scripted(vec![
            json!({
                "stop_reason": "tool_use",
                "content": [
                    tool_call("tu_1", "list_dir", json!({})),
                    tool_call("tu_2", "grep", json!({ "pattern": "fn main" })),
                ],
            }),
            turn("end_turn", "pronto"),
        ]);
        let mut chat = vec![];
        block_on(run_loop(
            &mut chat,
            "sys",
            &[],
            &ctx(),
            &AgentTurn::default(),
            "claude-opus-5",
            transport,
        ))
        .expect("ok");

        let results = tool_result_turn(&chat)["content"]
            .as_array()
            .expect("tool results");
        assert_eq!(results.len(), 2, "every tool_use needs an answer");
        let ids: Vec<&str> = results
            .iter()
            .filter_map(|r| r["tool_use_id"].as_str())
            .collect();
        assert_eq!(ids, ["tu_1", "tu_2"]);
    }

    #[test]
    fn server_side_tools_alone_end_the_loop() {
        // web_search resolves inside the same response: no client tool_use, so
        // there is nothing to feed back and the loop must not spin.
        let (sent, transport) = scripted(vec![json!({
            "stop_reason": "tool_use",
            "content": [
                { "type": "server_tool_use", "id": "srv_1", "name": "web_search" },
                { "type": "web_search_tool_result", "tool_use_id": "srv_1" },
                { "type": "text", "text": "achei" },
            ],
        })]);
        let mut chat = vec![];
        let reply = block_on(run_loop(
            &mut chat,
            "sys",
            &[],
            &ctx(),
            &AgentTurn::default(),
            "claude-opus-5",
            transport,
        ))
        .expect("ok");

        assert_eq!(calls(&sent), 1);
        assert_eq!(reply.text, "achei");
        assert_eq!(chat.len(), 1, "no tool_result is invented");
    }

    #[test]
    fn an_unexpected_stop_reason_is_an_error() {
        let (_, transport) = scripted(vec![turn("paused_for_compaction", "hm")]);
        let mut chat = vec![];
        let err = block_on(run_loop(
            &mut chat,
            "sys",
            &[],
            &ctx(),
            &AgentTurn::default(),
            "claude-opus-5",
            transport,
        ))
        .expect_err("an unknown stop must not be guessed at");
        assert_eq!(err, "stop_reason inesperado: paused_for_compaction");
    }

    #[test]
    fn a_refusal_is_reported_with_the_explanation() {
        let (_, transport) = scripted(vec![json!({
            "stop_reason": "refusal",
            "stop_details": { "explanation": "fora de política" },
            "content": [],
        })]);
        let mut chat = vec![];
        let err = block_on(run_loop(
            &mut chat,
            "sys",
            &[],
            &ctx(),
            &AgentTurn::default(),
            "claude-opus-5",
            transport,
        ))
        .expect_err("a refusal is surfaced, not swallowed");
        assert_eq!(err, "fora de política");
    }

    #[test]
    fn the_loop_gives_up_at_max_iters() {
        let mut responses = Vec::new();
        for _ in 0..10 {
            responses.push(json!({
                "stop_reason": "tool_use",
                "content": [tool_call("tu_x", "read_file", json!({}))],
            }));
        }
        let (sent, transport) = scripted(responses);
        let mut chat = vec![];

        let cfg = AgentTurn {
            max_iters: 3,
            ..AgentTurn::default()
        };
        let err = block_on(run_loop(
            &mut chat, "sys", &[], &ctx(), &cfg, "claude-opus-5", transport,
        ))
        .expect_err("the loop must be bounded");

        assert_eq!(err, "agent loop hit max_iters");
        assert_eq!(calls(&sent), 3, "exactly max_iters calls, no more");
    }

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let cfg = AgentTurn::default();
        assert_eq!(cfg.max_iters, 24);
        assert_eq!(cfg.tool_timeout_s, 60);
        assert_eq!(cfg.budget_tokens, 180_000);

        let ctx = ToolCtx::default();
        assert!(ctx.dry_run, "a tool must not write until asked to");
        assert!(ctx.allowed_shell.is_empty(), "empty allow-list blocks everything");
    }

    #[test]
    fn a_tool_result_always_goes_on_the_wire_as_a_string() {
        let structured = ToolResult::ok("tu_1", json!({ "files": 3 }));
        assert_eq!(
            structured.to_block(),
            json!({
                "type": "tool_result",
                "tool_use_id": "tu_1",
                "content": "{\"files\":3}",
                "is_error": false,
            })
        );

        let plain = ToolResult::err("tu_2", "sem permissão");
        assert_eq!(plain.to_block()["content"], "sem permissão");
        assert_eq!(plain.to_block()["is_error"], true);
    }

    /// The turn carrying `tool_result` blocks, wherever it landed.
    fn tool_result_turn(chat: &[Value]) -> &Value {
        chat.iter()
            .find(|m| {
                m["content"]
                    .as_array()
                    .map(|b| {
                        b.iter()
                            .any(|x| x["type"] == "tool_result")
                    })
                    .unwrap_or(false)
            })
            .expect("the loop must answer every tool_use")
    }

    #[test]
    fn an_over_budget_history_keeps_the_last_eight_messages() {
        let mut chat = history(12); // 24 messages
        assert!(approx_tokens(&chat) > 0);

        fit_budget(&mut chat, 200_000); // generous: nothing happens
        assert_eq!(chat.len(), 24);

        fit_budget(&mut chat, 1); // forced
        assert_eq!(chat.len(), 8, "the last eight survive");
        // The summary rides inside the first surviving turn rather than as a
        // message of its own: two user turns in a row is a request the API
        // rejects, and an extra message would also push past eight.
        let first = chat[0]["content"].as_str().expect("summary is text");
        assert!(first.starts_with(SUMMARY_PREFIX), "{first}");
        assert!(first.contains("pergunta 0"), "the summary keeps the gist");
        assert!(
            first.contains("pergunta 8"),
            "and the turn it was merged into is still there: {first}"
        );
    }

    #[test]
    fn the_cut_never_lands_between_a_tool_use_and_its_result() {
        let mut chat = history(6);
        // A tool exchange in the middle: assistant asks, user answers.
        chat.push(json!({
            "role": "assistant",
            "content": [tool_call("tu_1", "read_file", json!({}))],
        }));
        chat.push(json!({
            "role": "user",
            "content": [{ "type": "tool_result", "tool_use_id": "tu_1", "content": "ok" }],
        }));
        chat.push(json!({ "role": "assistant", "content": [{ "type": "text", "text": "pronto" }] }));

        fit_budget(&mut chat, 1);

        // Whatever survives has to start with a user turn that is not a lone
        // tool_result: that is what the API accepts as a conversation start.
        let first = &chat[0];
        assert_eq!(first["role"], "user");
        let blocks = first["content"].as_array();
        let only_results = blocks.map(|b| {
            !b.is_empty()
                && b.iter()
                    .all(|x| x["type"] == "tool_result")
        });
        assert!(only_results != Some(true), "history opens on a result");
    }

    #[test]
    fn the_summary_is_absorbed_rather_than_doubling_the_user_turn() {
        let mut chat = vec![
            json!({ "role": "user", "content": [{ "type": "text", "text": "pergunta antiga" }] }),
            json!({ "role": "assistant", "content": [{ "type": "text", "text": "resposta" }] }),
        ];
        prepend_summary(&mut chat, &["user: oi".to_string()]);

        assert_eq!(chat.len(), 2, "no extra user turn is inserted");
        let blocks = chat[0]["content"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0]["text"].as_str().unwrap().starts_with(SUMMARY_PREFIX));
        assert_eq!(blocks[1]["text"], "pergunta antiga");
    }

    #[test]
    fn a_short_history_is_left_completely_alone() {
        let original = history(2);
        let mut chat = original.clone();
        fit_budget(&mut chat, 1);
        assert_eq!(chat, original);
    }

    #[test]
    fn a_history_with_no_safe_cut_point_is_not_touched() {
        // Nine messages, all of them tool results: there is nowhere legal to
        // start, so leaving it whole beats corrupting it.
        let mut chat = Vec::new();
        for _ in 0..9 {
            chat.push(json!({
                "role": "user",
                "content": [{ "type": "tool_result", "tool_use_id": "tu", "content": "x" }],
            }));
        }
        let original = chat.clone();
        fit_budget(&mut chat, 1);
        assert_eq!(chat, original);
    }
}