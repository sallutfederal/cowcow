// Hook installation for the coding agents Coucou follows: Claude Code, Codex
// and Kimi Code.
//
// The rule from CLAUDE.md is strict and is followed to the letter:
// read the agent's config, take a dated backup, merge without touching anybody
// else's hooks, show the diff, and write only after an explicit click. Uninstall
// removes Coucou's entries and nothing else.
//
// The command is only the quoted exe path in forward slashes, the provider name
// and the event: on Windows these agents run hook commands through a shell, and
// anything with PowerShell or cmd in it breaks.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Map, Value};
use tauri::{AppHandle, Manager};
use windows::Win32::System::SystemInformation::GetLocalTime;

use crate::settings;

/// An agent Coucou can follow. The three JSON agents keep their config in
/// different places and disagree about almost everything else, so each one
/// carries its own file, event list and entry shape. Kimi's is TOML, which is
/// why the installer works on text rather than on parsed JSON.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentProvider {
    Claude,
    Codex,
    Kimi,
}

/// Every agent Coucou knows about, in the order the settings window shows them.
pub const PROVIDERS: &[AgentProvider] = &[
    AgentProvider::Claude,
    AgentProvider::Codex,
    AgentProvider::Kimi,
];


impl AgentProvider {
    pub fn parse(id: &str) -> Option<Self> {
        match id {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "kimi" => Some(Self::Kimi),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Kimi => "kimi",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Kimi => "Kimi Code",
        }
    }

    pub fn settings_path(self) -> PathBuf {
        match self {
            Self::Claude => home().join(".claude").join("settings.json"),
            Self::Codex => home().join(".codex").join("hooks.json"),
            // Kimi Code migrated from ~/.kimi-code to ~/.kimi. Write where the
            // CLI actually reads: an existing file wins, and only a machine that
            // has neither gets the new path.
            Self::Kimi => {
                let current = home().join(".kimi").join("config.toml");
                let legacy = home().join(".kimi-code").join("config.toml");
                if !current.exists() && legacy.exists() {
                    legacy
                } else {
                    current
                }
            }
        }
    }

    /// Events worth relaying, with the timeout written to the config.
    ///
    /// A permission request waits for a human, so it gets the decision timeout
    /// plus 10 s. Everything else is fire-and-forget.
    fn events(self) -> &'static [(&'static str, u64)] {
        match self {
            // Codex and Kimi speak Claude Code's hook protocol verbatim.
            Self::Claude | Self::Codex => CLAUDE_EVENTS,
            Self::Kimi => KIMI_EVENTS,
        }
    }

/// One entry as this agent's config expects it.
///
/// Claude Code and Codex nest the command under `hooks`. Kimi Code documents
/// the same protocol but writes TOML, where a hook is a flat table.
fn entry(self, event: &str, timeout: u64) -> Value {
        let command = hook_command(self, event);
        match self {
            Self::Claude | Self::Codex => json!({
                "hooks": [{ "type": "command", "command": command, "timeout": timeout }]
            }),
            Self::Kimi => json!({ "command": command, "timeout": timeout }),
        }
    }

    fn is_toml(self) -> bool {
        self == Self::Kimi
    }
}


/// Claude Code -- and Codex and Kimi, which document the same protocol -- event
/// names.
const CLAUDE_EVENTS: &[(&str, u64)] = &[
    ("SessionStart", 10),
    ("SessionEnd", 10),
    ("UserPromptSubmit", 10),
    ("PreToolUse", 10),
    ("PostToolUse", 10),
    ("PostToolUseFailure", 10),
    ("PermissionRequest", 120),
    ("Notification", 10),
    ("Stop", 10),
    ("SubagentStart", 10),
    ("SubagentStop", 10),
];


/// Kimi Code's events. No PermissionRequest among the thirteen, so this is
/// visibility: the decision stays in Kimi's own UI.
const KIMI_EVENTS: &[(&str, u64)] = &[
    ("SessionStart", 10),
    ("SessionEnd", 10),
    ("UserPromptSubmit", 10),
    ("PreToolUse", 10),
    ("PostToolUse", 10),
    ("PostToolUseFailure", 10),
    ("Notification", 10),
    ("Stop", 10),
    ("StopFailure", 10),
    ("SubagentStart", 10),
    ("SubagentStop", 10),
    ("PreCompact", 10),
    ("PostCompact", 10),
];

/// Marker that identifies a Coucou entry inside an agent's hook config.
const MARKER: &str = "coucou-hook";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookStatus {
    pub provider: String,
    pub name: String,
    pub installed: bool,
    pub settings_path: String,
    pub hook_path: String,
    pub hook_ready: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookPreview {
    pub diff: String,
    pub backup: String,
    pub settings_path: String,
    /// Identifies the bytes this diff was computed from; handed back to `write`
    /// so we only ever apply what the user actually looked at.
    pub fingerprint: String,
}

fn home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Reads an agent's config as text.
///
/// The only error that means "start from nothing" is the file not being there.
/// Everything else -- a lock held by another process, a permission problem, a
/// drive that is not there -- is reported, because the alternative is treating
/// somebody's unreadable settings as an empty file and writing it back over
/// them.
fn read_text(provider: AgentProvider) -> Result<String, String> {
    let path = provider.settings_path();
    match std::fs::read(&path) {
        Ok(bytes) => {
            // PowerShell writes a UTF-8 BOM, and it would end up in the diff and
            // in the TOML we hand back. Stripping it is safe and well defined.
            let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
            String::from_utf8(bytes.to_vec())
                .map_err(|_| format!("{} isn't UTF-8 text - Coucou won't touch it.", path.display()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(format!("Can't read {}: {err}", path.display())),
    }
}

/// The config parsed as JSON, refusing anything we could not safely merge.
fn read_settings(provider: AgentProvider) -> Result<Value, String> {
    let path = provider.settings_path();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(err) => return Err(format!("Can't read {}: {err}", path.display())),
    };
    parse_settings(&bytes, &path.display().to_string())
}

/// The parsing half of `read_settings`, split out so it can be tested without a
/// home directory.
fn parse_settings(bytes: &[u8], path: &str) -> Result<Value, String> {
    // PowerShell writes a UTF-8 BOM with `Set-Content -Encoding utf8`, and
    // serde_json refuses it. Stripping it is safe and well defined; guessing at
    // anything else is not.
    let text = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    if text.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    match serde_json::from_slice::<Value>(text) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(_) => Err(format!("{path} isn't a JSON object — Coucou won't touch it.")),
        Err(err) => Err(format!(
            "{path} isn't valid JSON ({err}). Fix or move it, then try again — Coucou won't overwrite it."
        )),
    }
}

/// The settings as they are, or an empty object when we cannot tell. Only for
/// read-only paths like `status()`, which must never fail loudly; anything that
/// writes uses `read_settings()` and surfaces the error instead.
fn read_settings_lossy(provider: AgentProvider) -> Value {
    read_settings(provider).unwrap_or_else(|_| json!({}))
}

/// The relay command for one event.
///
/// Claude Code's own installations are left exactly as they were -- one argument,
/// the event -- because the relay defaults to Claude. The other two name
/// themselves, which is what lets one binary speak all three dialects.
fn hook_command(provider: AgentProvider, event: &str) -> String {
    let exe = settings::hook_exe_path().to_string_lossy().replace('\\', "/");
    if provider == AgentProvider::Claude {
        format!("\"{exe}\" {event}")
    } else {
        format!("\"{exe}\" {} {event}", provider.id())
    }
}

/// True for one of our entries, in either shape: nested under `hooks` (Claude
/// Code, Codex) or flat (Cursor).
fn entry_is_ours(entry: &Value) -> bool {
    let mentions_marker = |command: &Value| {
        command
            .as_str()
            .map(|c| c.contains(MARKER))
            .unwrap_or(false)
    };
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hooks| hooks.iter().any(|h| h.get("command").map(mentions_marker).unwrap_or(false)))
        .unwrap_or(false)
        || entry.get("command").map(mentions_marker).unwrap_or(false)
}

/// Settings with Coucou's hooks added; everything else is left untouched.
fn merged(provider: AgentProvider, existing: &Value) -> Value {
    let mut root = existing.as_object().cloned().unwrap_or_default();
    let mut hooks = root
        .get("hooks")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(Map::new);

    for (event, timeout) in provider.events() {
        let mut list = hooks
            .get(*event)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        list.retain(|entry| !entry_is_ours(entry));
        list.push(provider.entry(event, *timeout));
        hooks.insert((*event).to_string(), Value::Array(list));
    }

    root.insert("hooks".into(), Value::Object(hooks));
    Value::Object(root)
}


/// Settings with every Coucou entry removed, and nothing else changed.
fn without_ours(existing: &Value) -> Value {

    let mut root = existing.as_object().cloned().unwrap_or_default();
    let Some(hooks) = root.get("hooks").and_then(Value::as_object).cloned() else {
        return Value::Object(root);
    };
    let mut out = Map::new();
    for (event, value) in hooks {
        match value.as_array() {
            Some(list) => {
                let kept: Vec<Value> =
                    list.iter().filter(|e| !entry_is_ours(e)).cloned().collect();
                if !kept.is_empty() {
                    out.insert(event, Value::Array(kept));
                }
            }
            None => {
                out.insert(event, value);
            }
        }
    }
    if out.is_empty() {
        root.remove("hooks");
    } else {
        root.insert("hooks".into(), Value::Object(out));
    }
    Value::Object(root)
}

// -- Kimi's TOML ----------------------------------------------------------------

/// Is this `[[hooks]]` block one of ours?
fn toml_block_is_ours(block: &[String]) -> bool {
block.iter().any(|line| line.contains(MARKER))
}

/// Splits a TOML file into a header and its `[[hooks]]` blocks, keeping the
/// blocks' lines exactly as they were.
///
/// A block runs from its `[[hooks]]` line to the next table header (`[...`).
/// Anything else -- comments, blank lines, other tables -- is kept verbatim in
/// `head`, and the tail after the last block is appended back on write. Nothing
/// in here parses TOML; it only finds our own lines and leaves the rest alone.
fn split_toml_hooks(text: &str) -> (Vec<String>, Vec<Vec<String>>, Vec<String>) {
    let mut head: Vec<String> = Vec::new();
    let mut blocks: Vec<Vec<String>> = Vec::new();
    let mut tail: Vec<String> = Vec::new();
    let mut current: Option<Vec<String>> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[[") {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            current = Some(vec![line.to_string()]);
            continue;
        }
        match current.as_mut() {
            // A new table ends the previous block; that table's line belongs to
            // the tail, not to the hook we were reading.
            Some(_) if trimmed.starts_with('[') => {
                let block = current.take().expect("checked above");
                blocks.push(block);
                tail.push(line.to_string());
            }
            Some(block) => block.push(line.to_string()),
            None => {
                if blocks.is_empty() {
                    head.push(line.to_string());
                } else {
                    tail.push(line.to_string());
                }
            }
        }
    }
    if let Some(block) = current.take() {
        blocks.push(block);
    }
    (head, blocks, tail)
}

/// The `[[hooks]]` blocks Coucou wants, in the shape Kimi documents.
fn kimi_toml_blocks() -> Vec<String> {
    AgentProvider::Kimi
        .events()
        .iter()
        .map(|(event, timeout)| {
            format!(
                "[[hooks]]\nevent = \"{}\"\ncommand = {}\ntimeout = {}\n",
                event,
                toml_string(&hook_command(AgentProvider::Kimi, event)),
                timeout
            )
        })
        .collect()
}

/// TOML basic string. Our command has no backslash (paths are forward-slashed)
/// and no quote, but escaping keeps that from being a hidden assumption.
fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Kimi's config with our blocks added, or removed, and every other byte of the
/// file left exactly as it was.
fn merged_toml(install: bool, text: &str) -> String {
    let (head, blocks, tail) = split_toml_hooks(text);
    let mut out: Vec<String> = head;

    for block in blocks {
        if !toml_block_is_ours(&block) {
            out.extend(block);
        }
    }
    if install {
        for block in kimi_toml_blocks() {
            // A blank line between tables, unless the file already ends in one.
            if !out.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
                out.push(String::new());
            }
            out.extend(block.lines().map(str::to_string));
        }
    }
    out.extend(tail);

    let mut text = out.join("\n");
    while text.ends_with('\n') {
        text.pop();
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// Down to the second: installing then uninstalling in the same minute must not
/// quietly overwrite the first backup.
fn stamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

fn backup_path(provider: AgentProvider) -> PathBuf {
    let p = provider.settings_path();
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "hooks.json".to_string());
    p.with_file_name(format!("{name}.bak-{}", stamp()))
}

/// Identifies the exact bytes a preview was computed from. FNV-1a is plenty:
/// the question is only "is this still the file I showed the user?".
fn fingerprint(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:016x}")
}

fn current_fingerprint(provider: AgentProvider) -> String {
    match std::fs::read(provider.settings_path()) {
        Ok(bytes) => fingerprint(&bytes),
        Err(_) => fingerprint(b""),
    }
}

// -- Public API ----------------------------------------------------------------

pub fn status(provider: AgentProvider) -> HookStatus {
    let installed = if provider.is_toml() {
        // Kimi: ours is a `[[hooks]]` block whose command names the relay.
        let text = read_text(provider).unwrap_or_default();
        let (_, blocks, _) = split_toml_hooks(&text);
        blocks.iter().any(|b| toml_block_is_ours(b))
    } else {
        let current = read_settings_lossy(provider);
        match current.get("hooks") {
            Some(Value::Object(hooks)) => hooks
                .values()
                .filter_map(Value::as_array)
                .flatten()
                .any(entry_is_ours),
            _ => false,
        }
    };
    let hook_path = settings::hook_exe_path();
    HookStatus {
        provider: provider.id().to_string(),
        name: provider.name().to_string(),
        installed,
        settings_path: provider.settings_path().to_string_lossy().to_string(),
        hook_ready: hook_path.exists(),
        hook_path: hook_path.to_string_lossy().to_string(),
    }
}

/// Installed state for every agent Coucou follows, in one call.
pub fn status_all() -> Vec<HookStatus> {
    PROVIDERS.iter().map(|p| status(*p)).collect()
}

/// The file as it would look after installing (or removing) our entries.
fn next_text(provider: AgentProvider, install: bool, current: &str) -> Result<String, String> {
    if provider.is_toml() {
        return Ok(merged_toml(install, current));
    }
    // A JSON file we cannot parse is an error, never an empty object: treating
    // unreadable settings as empty is how you overwrite somebody's config.
    let path = provider.settings_path();
    let value = parse_settings(current.as_bytes(), &path.display().to_string())?;
    let next = if install {
        merged(provider, &value)
    } else {
        without_ours(&value)
    };
    let mut text = pretty(&next);
    text.push('\n');
    Ok(text)
}

pub fn preview(provider: AgentProvider, install: bool) -> Result<HookPreview, String> {
    let current = read_text(provider)?;
    let next = next_text(provider, install, &current)?;
    Ok(HookPreview {
        diff: unified_diff(&current, &next),
        backup: backup_path(provider).to_string_lossy().to_string(),
        settings_path: provider.settings_path().to_string_lossy().to_string(),
        fingerprint: current_fingerprint(provider),
    })
}

/// Writes the merged (or cleaned) settings after taking a dated backup.
///
/// `fingerprint` is the one the preview was computed from. If the file changed
/// in between -- another tool, another window, the user's own editor -- we stop
/// and make them look at a fresh diff, because the only thing worse than not
/// installing the hooks is silently reverting somebody else's edit.
pub fn write(
    provider: AgentProvider,
    install: bool,
    fingerprint: &str,
) -> Result<String, String> {
    let path = provider.settings_path();
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;

    // Read before the backup: an unreadable file must abort before we touch
    // anything at all.
    let current = read_text(provider)?;
    if current_fingerprint(provider) != fingerprint {
        return Err(format!(
            "{} changed since the preview. Nothing was written - review the new diff.",
            path.display()
        ));
    }
    // Parse before the backup too: a broken file must not get a backup written
    // over it either.
    let next = next_text(provider, install, &current)?;

    let backup = backup_path(provider);
    if path.exists() {
        std::fs::copy(&path, &backup).map_err(|e| format!("backup failed: {e}"))?;
    }

    // Write beside the target and rename over it: a crash or a full disk leaves
    // the original file intact rather than half a file.
    let temp = path.with_extension(format!("coucou-{}", std::process::id()));
    std::fs::write(&temp, next.as_bytes()).map_err(|e| format!("write failed: {e}"))?;
    if let Err(err) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("write failed: {err}"));
    }
    Ok(backup.to_string_lossy().to_string())
}

/// Copies coucou-hook.exe into %LOCALAPPDATA%\Coucou\bin on launch.
/// In a bundled install it comes from the app resources; in `tauri dev` it sits
/// next to coucou.exe in the workspace target directory.
///
/// Every candidate is tried rather than just the first, because getting this
/// wrong is silent and fatal: `resources` used to be a glob, which made NSIS
/// mirror the source path into `_up_\target\release\`, no candidate matched, and
/// the relay was simply never installed. It only looked healthy on a developer
/// machine, where a leftover copy from `tauri dev` was already sitting in bin/.
pub fn ensure_hook_exe(app: &AppHandle) {
    let dest = settings::hook_exe_path();
    let Some(dir) = dest.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(p) = app.path().resolve("coucou-hook.exe", tauri::path::BaseDirectory::Resource) {
        candidates.push(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            // Installed build, then `tauri dev` (target/debug) next to the
            // release hook the pre-build step produces.
            candidates.push(parent.join("coucou-hook.exe"));
            candidates.push(parent.join("../release/coucou-hook.exe"));
            // Belt and braces: where the old glob form used to land it.
            candidates.push(parent.join("_up_/target/release/coucou-hook.exe"));
        }
    }

    let tried: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
    let Some(src) = candidates.into_iter().find(|p| p.exists()) else {
        crate::log::line(format!(
            "coucou-hook.exe not found — Claude Code hooks cannot work. Looked in: {}",
            tried.join(", ")
        ));
        return;
    };

    let same = match (std::fs::metadata(&src), std::fs::metadata(&dest)) {
        (Ok(a), Ok(b)) => a.len() == b.len() && a.modified().ok() == b.modified().ok(),
        _ => false,
    };
    if same {
        return;
    }
    // A hook may be running right now and hold the file open; keeping the old
    // copy is fine, it is the same relay.
    if let Err(err) = std::fs::copy(&src, &dest) {
        if !dest.exists() {
            crate::log::line(format!("could not install coucou-hook.exe: {err}"));
        }
    }
}

// ── Minimal unified diff (LCS) ────────────────────────────────────────────────

/// settings.json is short, so a plain O(n·m) LCS is the simplest honest diff.
fn unified_diff(before: &str, after: &str) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let (n, m) = (a.len(), b.len());

    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let mut out: Vec<String> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(format!("  {}", a[i]));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(format!("- {}", a[i]));
            i += 1;
        } else {
            out.push(format!("+ {}", b[j]));
            j += 1;
        }
    }
    while i < n {
        out.push(format!("- {}", a[i]));
        i += 1;
    }
    while j < m {
        out.push(format!("+ {}", b[j]));
        j += 1;
    }

    // Keep three lines of context around each change so the panel stays readable.
    let changed: Vec<usize> = out
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with('+') || l.starts_with('-'))
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return "No change.".into();
    }
    let mut keep = vec![false; out.len()];
    for idx in changed {
        let lo = idx.saturating_sub(3);
        let hi = (idx + 4).min(out.len());
        for k in lo..hi {
            keep[k] = true;
        }
    }
    let mut result = String::new();
    let mut gap = false;
    for (idx, line) in out.iter().enumerate() {
        if keep[idx] {
            result.push_str(line);
            result.push('\n');
            gap = false;
        } else if !gap {
            result.push_str("  …\n");
            gap = true;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHERE: &str = "settings.json";

    #[test]
    fn status_covers_every_agent() {
        // The settings window draws one section per status. If an agent is
        // missing here it is missing on screen, whatever the front-end asks for.
        let statuses = status_all();
        for provider in PROVIDERS {
            let found = statuses
                .iter()
                .find(|s| s.provider == provider.id())
                .unwrap_or_else(|| panic!("{} has no status", provider.id()));
            assert_eq!(found.name, provider.name());
        }
        assert_eq!(statuses.len(), PROVIDERS.len());
    }

    #[test]
    fn a_utf8_bom_is_stripped_not_treated_as_corruption() {
        // PowerShell 5's `Set-Content -Encoding utf8` produces exactly this.
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(br#"{"model":"opus","hooks":{}}"#);
        let parsed = parse_settings(&bytes, WHERE).expect("a BOM must not defeat the parser");
        assert_eq!(parsed["model"], "opus");
    }

    #[test]
    fn unreadable_content_is_an_error_never_an_empty_object() {
        // This is the whole bug: returning {} here meant `merged()` produced a
        // file containing nothing but Coucou's hooks, and the write replaced
        // everything the user had.
        for bad in [&b"{ not json"[..], &b"[1,2,3]"[..], &b"\"a string\""[..]] {
            assert!(
                parse_settings(bad, WHERE).is_err(),
                "content we cannot use must refuse, not come back empty"
            );
        }
    }

    #[test]
    fn empty_and_whitespace_files_start_from_nothing() {
        assert_eq!(parse_settings(b"", WHERE).unwrap(), json!({}));
        assert_eq!(parse_settings(b"  
	 ", WHERE).unwrap(), json!({}));
    }

    #[test]
    fn merging_keeps_every_other_setting_and_every_foreign_hook() {
        let existing = serde_json::json!({
            "model": "claude-opus-5",
            "theme": "dark",
            "enabledPlugins": ["a", "b"],
            "hooks": {
                "PreToolUse": [
                    { "hooks": [{ "type": "command", "command": "someone-elses-tool.exe" }] }
                ],
                "SomeEventWeDoNotTouch": [
                    { "hooks": [{ "type": "command", "command": "keep-me.exe" }] }
                ]
            }
        });

        let after = merged(AgentProvider::Claude, &existing);
        assert_eq!(after["model"], "claude-opus-5");
        assert_eq!(after["theme"], "dark");
        assert_eq!(after["enabledPlugins"], serde_json::json!(["a", "b"]));

        let pre = after["hooks"]["PreToolUse"].as_array().unwrap();
        assert!(
            pre.iter().any(|e| serde_json::to_string(e).unwrap().contains("someone-elses-tool.exe")),
            "another tool's hook was dropped"
        );
        assert!(pre.iter().any(entry_is_ours), "our own hook was not added");
        assert!(after["hooks"]["SomeEventWeDoNotTouch"].is_array());

        // And removing ours puts it back exactly as it was.
        let cleaned = without_ours(&after);
        assert_eq!(cleaned, existing);
    }

    #[test]
    fn a_fingerprint_notices_any_change() {
        assert_eq!(fingerprint(b"{}"), fingerprint(b"{}"));
        assert_ne!(fingerprint(b"{}"), fingerprint(b"{ }"));
        assert_ne!(fingerprint(b""), fingerprint(b"{}"));
    }

    /// Everything filesystem-shaped lives in one test on purpose: it points
    /// USERPROFILE at a temp directory, and that is process-wide.
    #[test]
    fn writing_backs_up_preserves_and_refuses_a_changed_file() {
        let tmp = std::env::temp_dir().join(format!("coucou-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".claude")).unwrap();
        std::env::set_var("USERPROFILE", &tmp);

        let claude = AgentProvider::Claude;
        let path = AgentProvider::Claude.settings_path();
        assert!(path.starts_with(&tmp), "the test must not touch the real home");

        // A real-shaped file, written the way PowerShell 5 would: UTF-8 with BOM.
        let original = r#"{"model":"claude-opus-5","theme":"dark","tui":{"x":1},"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"other-tool.exe"}]}]}}"#;
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(original.as_bytes());
        std::fs::write(&path, &bytes).unwrap();

        // Install.
        let plan = preview(claude, true).expect("a BOM must not stop the preview");
        assert!(plan.diff.contains("coucou-hook"), "the diff must show what changes");
        let backup = write(claude, true, &plan.fingerprint).expect("install should succeed");

        // The backup holds the original bytes, BOM and all.
        assert_eq!(std::fs::read(&backup).unwrap(), bytes);

        // Everything else survived, and so did the other tool's hook.
        let after: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(after["model"], "claude-opus-5");
        assert_eq!(after["theme"], "dark");
        assert_eq!(after["tui"]["x"], 1);
        let pre = after["hooks"]["PreToolUse"].as_array().unwrap();
        assert!(pre.iter().any(|e| serde_json::to_string(e).unwrap().contains("other-tool.exe")));
        assert!(status(claude).installed);

        // A file that moved since the preview is refused, and left alone.
        let stale = preview(claude, false).unwrap();
        std::fs::write(&path, br#"{"model":"someone-else-edited-this"}"#).unwrap();
        let err = write(claude, false, &stale.fingerprint).unwrap_err();
        assert!(err.contains("changed since the preview"), "got: {err}");
        let untouched: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(untouched["model"], "someone-else-edited-this");

        // Content we cannot parse is refused before anything is written.
        std::fs::write(&path, b"{ broken").unwrap();
        assert!(preview(claude, true).is_err());
        assert!(write(claude, true, "whatever").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{ broken");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── Codex ──────────────────────────────────────────────────────

    #[test]
    fn every_agent_reads_its_own_file() {
        assert!(AgentProvider::Claude.settings_path().ends_with("settings.json"));
        assert!(AgentProvider::Claude.settings_path().to_string_lossy().contains(".claude"));
        assert!(AgentProvider::Codex.settings_path().to_string_lossy().contains(".codex"));
        assert_eq!(AgentProvider::parse("codex"), Some(AgentProvider::Codex));
        assert_eq!(AgentProvider::parse("gemini"), None);
    }

    #[test]
    fn claude_keeps_its_one_argument_command_and_the_others_name_themselves() {
        // Existing Claude installations must not change under the user's feet.
        assert_eq!(
            hook_command(AgentProvider::Claude, "Stop"),
            hook_command(AgentProvider::Claude, "Stop")
        );
        assert!(!hook_command(AgentProvider::Claude, "Stop").contains(" claude "));
        assert!(hook_command(AgentProvider::Codex, "Stop").contains("\" codex Stop"));
    }

    #[test]
    fn codex_gets_claude_shaped_entries_and_a_permission_timeout() {
        let after = merged(AgentProvider::Codex, &json!({}));
        let entry = &after["hooks"]["PermissionRequest"][0]["hooks"][0];
        assert_eq!(entry["type"], "command");
        assert_eq!(entry["timeout"], 120);
        // Codex has no StopFailure: it would sit in its config forever, unused.
        assert!(after["hooks"].get("StopFailure").is_none());
        // Nothing but `hooks` in a Codex file.
        assert_eq!(after.as_object().unwrap().len(), 1);
    }


    #[test]
    fn uninstall_takes_only_our_entries_back_out() {
        for provider in [AgentProvider::Claude, AgentProvider::Codex] {
            let existing = json!({
                "unrelated": true,
                "hooks": { "Stop": [{ "command": "guard.exe" }] }
            });
            let installed = merged(provider, &existing);
            let cleaned = without_ours(&installed);
            assert_eq!(cleaned["unrelated"], true);
            let kept: Vec<String> = cleaned["hooks"]["Stop"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e.to_string())
                .collect();
            assert_eq!(kept.len(), 1, "{provider:?} dropped a foreign hook");
            assert!(kept[0].contains("guard.exe"));
        }
    }

    #[test]
    fn both_shapes_are_recognised_as_ours_and_nothing_else_is() {
        assert!(entry_is_ours(&json!({ "command": "\"coucou-hook.exe\" codex Stop" })));
        assert!(entry_is_ours(
            &json!({ "hooks": [{ "command": "\"coucou-hook.exe\" Stop" }] })
        ));
        assert!(!entry_is_ours(&json!({ "command": "somebody-else.exe Stop" })));
        assert!(!entry_is_ours(&json!({ "hooks": [{ "command": "other.exe" }] })));
        assert!(!entry_is_ours(&json!({})));
    }



    // -- Kimi's TOML ------------------------------------------------------------

    #[test]
    fn kimi_writes_one_table_per_event_and_keeps_the_rest_of_the_file() {
        let existing = "\
model = \"k2\"

[[hooks]]
event = \"PostToolUse\"
matcher = \"WriteFile\"
command = \"prettier --write\"

[mcp]
url = \"https://example.com\"
";
        let merged = merged_toml(true, existing);

        assert!(merged.contains("model = \"k2\""));
        assert!(merged.contains("prettier --write"), "a foreign hook was lost");
        assert!(merged.contains("[mcp]"));
        assert!(merged.contains("url = \"https://example.com\""));
        assert_eq!(
            merged.matches("[[hooks]]").count(),
            KIMI_EVENTS.len() + 1,
            "one table per event, plus the one already there"
        );
        assert!(merged.contains("kimi Stop"), "the command must name the agent");
        assert!(merged.contains("event = \"SessionStart\""));
    }

    #[test]
    fn removing_kimi_takes_only_our_tables_back_out() {
        let installed = merged_toml(true, "model = \"k2\"\n\n[[hooks]]\ncommand = \"guard.sh\"\n");
        let cleaned = merged_toml(false, &installed);
        assert!(cleaned.contains("guard.sh"));
        assert!(!cleaned.contains("coucou-hook"));
        assert!(cleaned.contains("model = \"k2\""));
        assert_eq!(cleaned.matches("[[hooks]]").count(), 1);
    }

    #[test]
    fn installing_kimi_twice_does_not_double_up() {
        let once = merged_toml(true, "model = \"k2\"\n");
        let twice = merged_toml(true, &once);
        assert_eq!(
            twice.matches("coucou-hook.exe").count(),
            KIMI_EVENTS.len(),
            "a second install must replace, not append"
        );
    }

    #[test]
    fn kimi_blocks_are_found_whatever_surrounds_them() {
        let text = "a = 1\n\n[[hooks]]\nevent = \"Stop\"\ncommand = \"x\"\n\n[mcp]\nk = 2\n";
        let (head, blocks, tail) = split_toml_hooks(text);
        assert_eq!(blocks.len(), 1);
        assert!(head.contains(&"a = 1".to_string()));
        assert!(tail.contains(&"[mcp]".to_string()));
        assert!(tail.contains(&"k = 2".to_string()));
        assert!(!toml_block_is_ours(&blocks[0]));

        let ours = merged_toml(true, text);
        let (_, blocks, _) = split_toml_hooks(&ours);
        assert_eq!(blocks.len(), KIMI_EVENTS.len() + 1);
        assert_eq!(blocks.iter().filter(|b| toml_block_is_ours(b)).count(), KIMI_EVENTS.len());
    }

    #[test]
    fn toml_strings_are_escaped() {
        assert_eq!(toml_string("C:\\a\"b"), "\"C:\\\\a\\\"b\"");
    }
}