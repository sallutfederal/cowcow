// Client-side tools the model can call.
//
// A tool is described to the API exactly as Anthropic expects it (name,
// description, input_schema) and, when the model calls one, the call is
// executed here, on this machine — never on a server. Phase 1 of the agent
// work introduces the vocabulary (ToolDef) and the dispatcher; the concrete
// tools land in fs.rs, shell.rs, net.rs and patch.rs.

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

/// The tools this build offers.
///
/// Empty in phase 1: the agent loop exists and is exercised, but no client-side
/// tool is wired up yet. A model that calls a name outside this list gets a
/// `tool_result` with `is_error`, which is the correct answer for a name the
/// build does not have — the same thing that happens when a model invents a
/// tool.
pub fn registry() -> Vec<ToolDef> {
    Vec::new()
}

/// Runs one tool call and packages the outcome for the API.
///
/// Never panics: every failure comes back as a `ToolResult` with `is_error`,
/// because a tool that crashed the app would take the chat down with it.
pub async fn execute_tool(
    id: &str,
    name: &str,
    _input: &Value,
    _ctx: &ToolCtx,
) -> ToolResult {
    let known = registry().iter().any(|t| t.name == name);
    if known {
        // Reached only once a tool is registered; kept total so the dispatcher
        // stays exhaustive as fs/shell/net/patch land.
        return ToolResult::err(id, format!("'{name}' ainda não executa"));
    }
    ToolResult::err(id, format!("ferramenta desconhecida: '{name}'"))
}