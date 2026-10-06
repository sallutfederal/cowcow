// `run_shell`: the one tool that can do damage, so it is the one with rules.
//
// Three gates, in order, before anything is spawned:
//
//  1. the switch has to be on, and the first token has to be on the allow-list;
//  2. a shell as the first token is refused unless it was allowed on purpose,
//     which closes `cmd /C git status` and friends;
//  3. no chaining — `&&`, `||`, `;`, `|`, a backtick or `$(` is refused even
//     when the first token is allowed, so `git status && del /s` never runs.
//
// Then the command runs in a Job Object (see job.rs), so a timeout kills the
// whole tree and not just `cmd`.


use serde_json::{json, Value};
use tokio::process::Command;

use super::job::JobHandle;
use crate::claude::ToolResult;

/// CREATE_NO_WINDOW (winbase.h) — std does not re-export it; the value is
/// fixed by the Win32 SDK. Without it every command flashes a console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Shells that exist only to run something else. Allowing one of these makes
/// the allow-list meaningless, so they are refused unless named explicitly.
const SHELLS: &[&str] = &[
    "cmd", "cmd.exe", "powershell", "powershell.exe", "pwsh", "pwsh.exe", "bash", "bash.exe",
    "sh", "sh.exe", "zsh",
];

/// Characters that chain one command into another. Any of them in an allowed
/// command is refused rather than parsed.
const CHAINING: &[&str] = &["&&", "||", ";", "|", "`", "$("];

/// Wraps a payload as an error when the command did not succeed.
///
/// `ToolResult::err` takes a message, and a shell failure deserves the
/// structured payload more than a flattened string: the model reads
/// `exit_code` and `stdout` directly instead of parsing prose.
fn failure(id: &str, payload: Value) -> ToolResult {
    ToolResult {
        id: id.to_string(),
        content: payload,
        is_error: true,
    }
}

/// What the command produced.
struct ShellResult {
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
}

/// Runs `command` if the rules allow it.
///
/// Returns a `ToolResult`, never an error type: a refused or failed command is
/// something the model should read and react to, not something that should
/// unwind the island.
pub async fn run_shell(id: &str, command: &str, cwd: &str, allowed: &[String], timeout_s: u64, dry_run: bool) -> ToolResult {
    if let Some(reason) = refuse(command, allowed) {
        return ToolResult::err(id, reason);
    }

    if dry_run {
        return ToolResult::ok(
            id,
            json!({
                "would_run": command,
                "dry_run": true,
                "note": "shell dry-run: nothing was executed",
            }),
        );
    }

    match spawn_and_wait(command, cwd, timeout_s).await {
        Ok(result) => {
            let payload = json!({
                "stdout": result.stdout,
                "stderr": result.stderr,
                "exit_code": result.exit_code,
                "timed_out": result.timed_out,
            });
            if result.timed_out || result.exit_code != 0 {
                failure(id, payload)
            } else {
                ToolResult::ok(id, payload)
            }
        }
        Err(e) => ToolResult::err(id, e),
    }
}

/// The reason this command must not run, if it must not.
///
/// Split out from [`run_shell`] so the rules can be tested without spawning
/// anything, which matters: these gates are the whole security story.
fn refuse(command: &str, allowed: &[String]) -> Option<String> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Some("comando vazio".to_string());
    }
    // An empty allow-list means nothing was authorised.
    if allowed.is_empty() {
        return Some(
            "nenhum comando autorizado: a lista de allow-list está vazia".to_string()
        );
    }

    let first = trimmed
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();

    let is_allowed = allowed
        .iter()
        .any(|a| a.trim().to_lowercase() == first);
    if !is_allowed {
        return Some(format!(
            "comando '{first}' não está na allow-list ({})",
            allowed.join(", ")
        ));
    }

    // A shell that was allowed on purpose is allowed. One that was not is
    // refused even if it sits on the list by accident.
    if SHELLS.contains(&first.as_str()) {
        return Some(format!(
            "'{first}' é um shell: usá-lo burlaria a allow-list. Remova-o da lista para permitir."
        ));
    }

    for chain in CHAINING {
        if trimmed.contains(chain) {
            return Some(format!(
                "comando encadeado contém '{chain}': execute um comando por chamada"
            ));
        }
    }

    None
}

/// Spawns through `cmd /C` and waits, killing the tree if it overruns.
async fn spawn_and_wait(command: &str, cwd: &str, timeout_s: u64) -> Result<ShellResult, String> {
    let mut builder = Command::new("cmd");
    builder
        .arg("/C")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // No console window: this runs under a desktop app, not a terminal.
    builder.creation_flags(CREATE_NO_WINDOW);

    // wait_with_output consumes the child, so no binding to keep mutable here.
    let child = builder.spawn().map_err(|e| format!("spawn: {e}"))?;

    // A microsecond-wide window between spawn and assign, in which the child
    // could fork before it is in the job. Closing it entirely needs
    // CREATE_SUSPENDED plus ResumeThread, which is not worth it for git/cargo.
    let job = match JobHandle::new() {
        Ok(job) => {
            if let Some(pid) = child.id() {
                if let Err(e) = job.assign_pid(pid) {
                    // Not fatal: the timeout still kills `cmd`. Only the
                    // guarantee about grandchildren weakens, and the app is
                    // already inside another job in that case.
                    crate::log::line(format!("job.assign_pid falhou: {e}"));
                }
            }
            Some(job)
        }
        Err(e) => {
            crate::log::line(format!("job indisponível: {e}"));
            None
        }
    };

    let wait = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_s),
        child.wait_with_output(),
    )
    .await;

    match wait {
        Ok(Ok(output)) => {
            // Explicit: the job is released once the command is done, so the
            // handle is not held for the rest of the turn.
            drop(job);
            Ok(ShellResult {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code().unwrap_or(-1),
                timed_out: false,
            })
        }
        Ok(Err(e)) => Err(format!("wait: {e}")),
        Err(_) => {
            // The point of the job: closing it here kills the command and
            // everything it started.
            drop(job);
            Ok(ShellResult {
                stdout: String::new(),
                stderr: format!("tempo esgotado apos {timeout_s}s"),
                exit_code: -1,
                timed_out: true,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_empty_allow_list_blocks_everything() {
        assert!(refuse("git status", &[]).is_some());
        assert!(refuse("anything at all", &[]).unwrap().contains("vazia"));
    }

    #[test]
    fn a_program_off_the_list_is_refused_by_name() {
        let err = refuse("curl https://example.com", &allowed(&["git", "ls"]))
            .expect("curl is not on the list");
        assert!(err.contains("curl"), "{err}");
        assert!(err.contains("não está na allow-list"), "{err}");
    }

    #[test]
    fn the_first_token_match_ignores_case() {
        assert!(refuse("GIT status", &allowed(&["git"])).is_none());
        assert!(refuse("git status", &allowed(&["GIT"])).is_none());
    }

    #[test]
    fn a_shell_is_refused_even_when_it_is_on_the_list() {
        for shell in ["cmd", "cmd.exe", "powershell", "pwsh", "bash", "sh"] {
            let err = refuse(&format!("{shell} /C git status"), &allowed(&["git", shell]))
                .expect("a shell must not be a way in");
            assert!(err.contains("shell"), "{shell}: {err}");
        }
    }

    #[test]
    fn chaining_is_refused_even_for_an_allowed_program() {
        for chained in [
            "git status && del /s x",
            "git status || echo no",
            "git status; del x",
            "git status | more",
            "git log `whoami`",
            "git log $(whoami)",
        ] {
            let err = refuse(chained, &allowed(&["git"])).expect("chaining is refused");
            assert!(err.contains("encadeado"), "{chained}: {err}");
        }
    }

    #[test]
    fn a_plain_allowed_command_passes_every_gate() {
        assert!(refuse("git status", &allowed(&["git"])).is_none());
        assert!(refuse("cargo build", &allowed(&["git", "cargo"])).is_none());
        assert!(refuse("ls -la", &allowed(&["ls"])).is_none());
    }

    #[tokio::test]
    async fn a_dry_run_reports_without_running_anything() {
        let result = run_shell(
            "tu_1",
            "git status",
            "C:\\",
            &allowed(&["git"]),
            60,
            true,
        )
        .await;
        assert!(!result.is_error, "{:?}", result.content);
        assert_eq!(result.content["would_run"], "git status");
        assert_eq!(result.content["dry_run"], true);
    }

    #[tokio::test]
    async fn a_refused_command_never_reaches_the_spawn() {
        let result = run_shell("tu_2", "curl evil.com", "C:\\", &allowed(&["git"]), 60, false).await;
        assert!(result.is_error);
        assert!(
            result.content.as_str().unwrap_or_default().contains("curl"),
            "{:?}",
            result.content
        );
    }

    #[tokio::test]
    async fn a_timeout_kills_the_command_and_reports_it() {
        // `ping -t` never ends on its own, so this can only finish by timing
        // out. 2s keeps the test quick.
        let result = run_shell(
            "tu_3",
            "ping -t 8.8.8.8",
            "C:\\",
            &allowed(&["ping"]),
            2,
            false,
        )
        .await;

        assert!(result.is_error, "a timeout is an error the model should see");
        assert_eq!(result.content["timed_out"], true, "{:?}", result.content);
        assert_eq!(result.content["exit_code"], -1);
    }

    #[tokio::test]
    async fn a_command_that_finishes_reports_its_output() {
        let result = run_shell(
            "tu_4",
            "echo coucou-agent",
            "C:\\",
            &allowed(&["echo"]),
            30,
            false,
        )
        .await;

        assert!(!result.is_error, "{:?}", result.content);
        assert_eq!(result.content["timed_out"], false);
        assert_eq!(result.content["exit_code"], 0);
        let stdout = result.content["stdout"].as_str().unwrap_or_default();
        assert!(stdout.contains("coucou-agent"), "{stdout}");
    }
}