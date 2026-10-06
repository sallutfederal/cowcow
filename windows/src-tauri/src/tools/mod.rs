// Client-side tools the model can call.
//
// A tool is described to the API exactly as Anthropic expects it (name,
// description, input_schema) and, when the model calls one, it runs here, on
// this machine — never on a server. Every tool answers with a `ToolResult`,
// including its failures: a refused shell command and a missing file are
// things the model should read and react to, not reasons to unwind the island.

mod fs;
mod job;
mod net;
mod patch;
mod shell;

use std::path::Path;

use serde_json::{json, Value};

use crate::claude::{ToolCtx, ToolResult};

/// One tool, as the model sees it.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema of the input object, as an `input_schema`.
    pub schema: Value,
}

impl ToolDef {
    /// The wire shape: Anthropic takes name / description / input_schema.
    pub fn to_anthropic(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.schema,
        })
    }
}

fn def(name: &str, description: &str, schema: Value) -> ToolDef {
    ToolDef {
        name: name.to_string(),
        description: description.to_string(),
        schema,
    }
}

/// Every tool this build offers, in the order the model sees them.
///
/// Reading and searching come before writing, and the shell comes last: that
/// order is the order of increasing consequence, and a model tends to reach for
/// the first thing that works.
pub fn registry() -> Vec<ToolDef> {
    vec![
        def(
            "read_file",
            "Read a UTF-8 text file. Returns the content, and with offset/limit only that window of lines. Paths are relative to the working directory.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File to read, relative to the working directory." },
                    "offset": { "type": "integer", "description": "First line to return, counting from 1." },
                    "limit": { "type": "integer", "description": "How many lines to return." }
                },
                "required": ["path"]
            }),
        ),
        def(
            "list_dir",
            "List the files under a directory, respecting .gitignore and skipping build and dependency folders.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory to list, relative to the working directory." },
                    "glob": { "type": "string", "description": "Filter, like *.rs or src/**." },
                    "max": { "type": "integer", "description": "How many entries to return. Default 200." }
                },
                "required": ["path"]
            }),
        ),
        def(
            "grep",
            "Search files for a regular expression. Returns matches with file, line and text, plus the total number found.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regular expression to search for." },
                    "path": { "type": "string", "description": "Where to search. Defaults to the whole working directory." },
                    "glob": { "type": "string", "description": "Filter, like *.rs." },
                    "max": { "type": "integer", "description": "How many matches to return. Default 100." }
                },
                "required": ["pattern"]
            }),
        ),
        def(
            "write_file",
            "Create or overwrite a file. Refuses paths outside the working directory.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File to write, relative to the working directory." },
                    "content": { "type": "string", "description": "The full new contents." }
                },
                "required": ["path", "content"]
            }),
        ),
        def(
            "apply_patch",
            "Apply a unified diff to files under the working directory. Either every hunk applies or no file changes.",
            json!({
                "type": "object",
                "properties": {
                    "diff": { "type": "string", "description": "A unified diff, with --- a/<file> and +++ b/<file> headers." }
                },
                "required": ["diff"]
            }),
        ),
        def(
            "run_shell",
            "Run one command. Only programs on the user's allow-list run, one command per call, with no chaining.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command line to run. No &&, ||, ; or pipes." },
                    "cwd": { "type": "string", "description": "Where to run it. Defaults to the working directory." }
                },
                "required": ["command"]
            }),
        ),
        def(
            "fetch_url",
            "Fetch a URL over http or https and return the body as text.",
            json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The http or https URL to fetch." },
                    "method": { "type": "string", "description": "Only GET." }
                },
                "required": ["url"]
            }),
        ),
    ]
}

/// Runs one tool call and packages the outcome for the API.
///
/// Never panics: a bad path, a refused command and a regex that does not
/// compile all come back as a `ToolResult` with `is_error`, because the model
/// is better at recovering from a message than the island is at surviving
/// without one.
pub async fn execute_tool(id: &str, name: &str, input: &Value, ctx: &ToolCtx) -> ToolResult {
    let cwd: &Path = ctx.cwd.as_path();

    match name {
        "read_file" => {
            let Some(path) = text(input, "path") else {
                return missing(id, "path");
            };
            fs::read_file(id, cwd, &path, number(input, "offset"), number(input, "limit")).await
        }
        "list_dir" => {
            let Some(path) = text(input, "path") else {
                return missing(id, "path");
            };
            fs::list_dir(id, cwd, &path, text(input, "glob").as_deref(), number(input, "max"))
        }
        "grep" => {
            let Some(pattern) = text(input, "pattern") else {
                return missing(id, "pattern");
            };
            fs::grep(
                id,
                cwd,
                &pattern,
                text(input, "path").as_deref(),
                text(input, "glob").as_deref(),
                number(input, "max"),
            )
        }
        "write_file" => {
            let (Some(path), Some(content)) = (text(input, "path"), text(input, "content")) else {
                return missing(id, "path e content");
            };
            fs::write_file(id, cwd, &path, &content, ctx.dry_run).await
        }
        "apply_patch" => {
            let Some(diff) = text(input, "diff") else {
                return missing(id, "diff");
            };
            patch::apply_patch(id, cwd, &diff, ctx.dry_run).await
        }
        "run_shell" => {
            let Some(command) = text(input, "command") else {
                return missing(id, "command");
            };
            let dir = text(input, "cwd").unwrap_or_else(|| ".".to_string());
            shell::run_shell(
                id,
                &command,
                &dir,
                &                ctx.allowed_shell,
                ctx.tool_timeout_s,
                ctx.dry_run,
            )
            .await
        }
        "fetch_url" => {
            let Some(url) = text(input, "url") else {
                return missing(id, "url");
            };
            net::fetch_url(id, &url, text(input, "method").as_deref()).await
        }
        other => ToolResult::err(id, format!("ferramenta desconhecida: '{other}'")),
    }
}

/// A required field the model left out.
fn missing(id: &str, field: &str) -> ToolResult {
    ToolResult::err(id, format!("faltando '{field}'"))
}

/// A string field, if present and a string.
fn text(input: &Value, key: &str) -> Option<String> {
    input.get(key)?.as_str().map(str::to_string)
}

/// An integer field, if present and a number.
fn number(input: &Value, key: &str) -> Option<usize> {
    input.get(key)?.as_u64().map(|n| n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_has_the_seven_tools_and_valid_json_schemas() {
        let tools = registry();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "read_file",
                "list_dir",
                "grep",
                "write_file",
                "apply_patch",
                "run_shell",
                "fetch_url"
            ]
        );
        for tool in &tools {
            assert!(!tool.description.is_empty(), "{} sem descrição", tool.name);
            assert_eq!(
                tool.schema["type"], "object",
                "{} precisa de um schema object",
                tool.name
            );
        }
    }

    #[test]
    fn every_tool_declares_the_fields_the_dispatcher_reads() {
        // A schema that does not mention a required field would let the model
        // call the tool in a way that can only fail.
        let by_name = |name: &str| {
            registry()
                .into_iter()
                .find(|t| t.name == name)
                .expect("tool registered")
        };
        let required = |tool: &ToolDef| tool.schema["required"].to_string();

        assert!(required(&by_name("read_file")).contains("path"));
        assert!(required(&by_name("list_dir")).contains("path"));
        assert!(required(&by_name("grep")).contains("pattern"));
        assert!(required(&by_name("write_file")).contains("content"));
        assert!(required(&by_name("apply_patch")).contains("diff"));
        assert!(required(&by_name("run_shell")).contains("command"));
        assert!(required(&by_name("fetch_url")).contains("url"));
    }

    #[test]
    fn the_wire_shape_is_what_the_api_expects() {
        let tool = registry().remove(0);
        let wire = tool.to_anthropic();
        assert_eq!(wire["name"], "read_file");
        assert!(wire["input_schema"].is_object());
        assert!(wire["description"].is_string());
    }

    #[tokio::test]
    async fn an_unknown_tool_is_an_error_not_a_panic() {
        let result = execute_tool("t", "rm_rf", &json!({}), &ToolCtx::default()).await;
        assert!(result.is_error);
        assert!(result.content.as_str().unwrap().contains("rm_rf"));
    }

    #[tokio::test]
    async fn a_missing_required_field_names_the_field() {
        let result = execute_tool("t", "read_file", &json!({}), &ToolCtx::default()).await;
        assert!(result.is_error);
        assert!(result.content.as_str().unwrap().contains("path"));
    }

    /// A directory for one test, wiped on entry.
    ///
    /// Per test, not shared: tests run in parallel and a shared directory means
    /// one deletes the other's files mid-assert.
    fn dir_for(name: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("coucou-tools-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("a temp directory for this test");
        base
    }

    #[tokio::test]
    async fn the_dispatcher_reads_and_lists_through_the_cwd() {
        let dir = dir_for("dispatch_read");
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();

        let ctx = ToolCtx::new(dir.clone());

        let read = execute_tool(
            "t",
            "read_file",
            &json!({ "path": "a.rs" }),
            &ctx,
        )
        .await;
        assert!(!read.is_error, "{:?}", read.content);
        assert!(read.content["content"].as_str().unwrap().contains("fn a"));

        let list = execute_tool("t", "list_dir", &json!({ "path": "." }), &ctx).await;
        assert!(!list.is_error, "{:?}", list.content);
        assert_eq!(list.content["total"], 1);

        let grep = execute_tool(
            "t",
            "grep",
            &json!({ "pattern": "fn a" }),
            &ctx,
        )
        .await;
        assert!(!grep.is_error, "{:?}", grep.content);
        assert_eq!(grep.content["total"], 1);

    }

    #[tokio::test]
    async fn a_write_through_the_dispatcher_respects_dry_run() {
        let dir = dir_for("dispatch_write");
        let ctx = ToolCtx::new(dir.clone());

        let result = execute_tool(
            "t",
            "write_file",
            &json!({ "path": "novo.txt", "content": "oi" }),
            &ctx,
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(result.content["would_write"], "novo.txt");
        assert!(!dir.join("novo.txt").exists());
    }

    #[tokio::test]
    async fn run_shell_through_the_dispatcher_is_blocked_by_default() {
        // A fresh install has an empty allow-list: the dispatcher must refuse.
        let mut ctx = ToolCtx::new(std::env::temp_dir());
        ctx.dry_run = false;
        let result = execute_tool(
            "t",
            "run_shell",
            &json!({ "command": "git status" }),
            &ctx,
        )
        .await;

        assert!(result.is_error);
        assert!(result.content.as_str().unwrap().contains("vazia"));
    }
}