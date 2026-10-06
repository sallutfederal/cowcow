// `apply_patch`: a unified diff, applied all-or-nothing.
//
// The guarantee that matters: if any hunk fails, no file changes. So the whole
// diff is parsed and applied to an in-memory copy of each file first, and only
// a diff that applies cleanly everywhere is written to disk — each file through
// a temporary name and a rename, so a reader never sees a partial write.
//
// The file to patch comes from the diff's own `+++ b/<path>` header, relative
// to the working directory. A patch touching several files is applied to all of
// them or to none.

use std::collections::BTreeMap;
use std::path::Path;

use diffy::Patch;
use serde_json::{json, Value};

use super::fs;
use crate::claude::ToolResult;

/// Parses a unified diff into `path -> hunks`, as diffy sees it.
pub async fn apply_patch(id: &str, cwd: &Path, diff: &str, dry_run: bool) -> ToolResult {
    if diff.trim().is_empty() {
        return ToolResult::err(id, "diff vazio");
    }

    // Each file gets its own slice of the diff, parsed on its own: that is what
    // keeps a multi-file patch atomic — one file's bad hunk stops the lot.
    let files = split_by_file(diff);
    if files.is_empty() {
        return ToolResult::err(id, "diff sem cabeçalho de arquivo (+++ b/<arquivo>)");
    }

    // Stage 1: apply everything in memory. Nothing on disk moves yet.
    let mut staged: BTreeMap<String, String> = BTreeMap::new();
    let mut changed: Vec<Value> = Vec::new();

    for (path, body) in &files {
        let target = match fs::resolve(cwd, path) {
            Ok(t) => t,
            Err(e) => {
                // One bad path aborts the whole patch: that is the point.
                return ToolResult::err(id, e.to_string());
            }
        };

        let original = match std::fs::read_to_string(&target) {
            Ok(text) => text,
            Err(e) => {
                return ToolResult::err(
                    id,
                    format!("{path}: não pôde ser lido ({e}) — nada foi alterado"),
                )
            }
        };

        let piece = match Patch::from_str(body) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(id, format!("{path}: diff inválido: {e}")),
        };
        let patched = match diffy::apply(&original, &piece) {
            Ok(text) => text,
            Err(hunk) => {
                // `Err` carries the 1-based hunk number that did not apply.
                return ToolResult::err(
                    id,
                    format!("{path}: hunk {hunk} não aplica — nada foi alterado"),
                )
            }
        };

        if patched != original {
            changed.push(json!({
                "file": path,
                "before_bytes": original.len(),
                "after_bytes": patched.len(),
            }));
            staged.insert(path.clone(), patched);
        }
    }

    if staged.is_empty() {
        return ToolResult::ok(
            id,
            json!({
                "applied": [],
                "note": "o diff não mudaria nada",
                "dry_run": dry_run,
            }),
        );
    }

    if dry_run {
        return ToolResult::ok(
            id,
            json!({
                "would_apply": changed,
                "files": staged.keys().collect::<Vec<_>>(),
                "dry_run": true,
                "note": "nada foi escrito: dry-run ligado",
            }),
        );
    }

    // Stage 2: every hunk applied, so the disk writes can go ahead.
    let mut written = Vec::new();
    for (path, contents) in &staged {
        // `write_file` is the same atomic write the model gets, so there is one
        // implementation of "put this file on disk", not two.
        let result = fs::write_file(id, cwd, path, contents, false).await;
        if result.is_error {
            return ToolResult::err(
                id,
                format!(
                    "{path}: falha ao escrever ({}); arquivos anteriores podem ter sido alterados",
                    result.content
                ),
            );
        }
        written.push(json!({ "file": path, "bytes": contents.len() }));
    }

    ToolResult::ok(
        id,
        json!({
            "applied": written,
            "files": written.len(),
            "dry_run": false,
        }),
    )
}

/// Splits a multi-file diff on the `+++ b/x` headers.
///
/// Returns `path -> that file's slice of the original string`. The slices are
/// taken by byte offset rather than rebuilt from lines: a hunk's last context
/// line ends with `\n`, and that newline is what makes it match the file, so
/// re-joining lines would silently break every patch whose last line is context.
fn split_by_file(diff: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = diff.split_inclusive('\n').collect();
    let mut files: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, usize)> = None;

    for (index, line) in lines.iter().enumerate() {
        let Some(raw) = line.strip_prefix("+++ ") else {
            continue;
        };
        // Close the file we were collecting.
        if let Some((path, start)) = current.take() {
            let end = offset_of(&lines, index);
            files.push((path, diff[start..end].to_string()));
        }
        let trimmed = raw.trim();
        // `b/src/main.rs` -> `src/main.rs`. `/dev/null` means "create".
        let path = trimmed.strip_prefix("b/").unwrap_or(trimmed).to_string();
        current = Some((path, offset_of(&lines, index + 1)));
    }

    if let Some((path, start)) = current {
        files.push((path, diff[start..].to_string()));
    }
    files
}

/// Byte offset of the start of line `index`, counting the newlines themselves.
fn offset_of(lines: &[&str], index: usize) -> usize {
    lines
        .iter()
        .take(index)
        .map(|l| l.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory for one test, wiped on entry.
    ///
    /// Per test, not shared: tests run in parallel and a shared directory means
    /// one deletes the other's files mid-assert. Cleared on entry so a failure
    /// leaves its state behind for reading.
    fn dir_for(name: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("coucou-patch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("a temp directory for this test");
        base
    }

    fn diff_for(path: &str, from: &str, to: &str) -> String {
        format!(
            "--- a/{path}\n+++ b/{path}\n@@ -1,2 +1,2 @@\n-{from}\n+{to}\n"
        )
    }

    #[test]
    fn a_diff_splits_into_one_entry_per_file() {
        let diff = format!(
            "{}{}",
            diff_for("a.txt", "um", "dois"),
            diff_for("b.txt", "tres", "quatro")
        );
        let files = split_by_file(&diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, "a.txt");
        assert_eq!(files[1].0, "b.txt");
    }

    #[test]
    fn the_b_prefix_is_stripped_from_the_path() {
        let files = split_by_file(&diff_for("src/main.rs", "a", "b"));
        assert_eq!(files[0].0, "src/main.rs");
    }

    #[tokio::test]
    async fn a_dry_run_patch_reports_without_touching_the_file() {
        let dir = dir_for("dry_run");
        let file = dir.join("code.rs");
        std::fs::write(&file, "fn um() {}\nfn dois() {}\n").unwrap();

        let result = apply_patch(
            "t",
            &dir,
            "--- a/code.rs\n+++ b/code.rs\n@@ -1,2 +1,2 @@\n-fn um() {}\n+fn uno() {}\n fn dois() {}\n",
            true,
        )
        .await;

        assert!(!result.is_error, "{:?}", result.content);
        assert_eq!(result.content["dry_run"], true);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "fn um() {}\nfn dois() {}\n",
            "dry-run não pode escrever"
        );
    }

    #[tokio::test]
    async fn a_patch_that_applies_writes_the_new_contents() {
        let dir = dir_for("applies");
        let file = dir.join("code.rs");
        std::fs::write(&file, "fn um() {}\nfn dois() {}\n").unwrap();

        let result = apply_patch(
            "t",
            &dir,
            "--- a/code.rs\n+++ b/code.rs\n@@ -1,2 +1,2 @@\n-fn um() {}\n+fn uno() {}\n fn dois() {}\n",
            false,
        )
        .await;

        assert!(!result.is_error, "{:?}", result.content);
        assert_eq!(result.content["files"], 1);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "fn uno() {}\nfn dois() {}\n"
        );
    }

    #[tokio::test]
    async fn a_hunk_that_does_not_apply_leaves_every_file_alone() {
        let dir = dir_for("hunk_fails");
        let first = dir.join("a.txt");
        let second = dir.join("b.txt");
        std::fs::write(&first, "um\n").unwrap();
        std::fs::write(&second, "tres\n").unwrap();

        // The first hunk applies, the second does not: nothing may be written.
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@ -1,1 +1,1 @@\n-um\n+dois\n\
             --- a/b.txt\n+++ b/b.txt\n@@ -1,1 +1,1 @@\n-nao-existe\n+novo\n";
        let result = apply_patch("t", &dir, diff, false).await;

        assert!(result.is_error);
        assert!(
            result.content.as_str().unwrap_or_default().contains("não aplica"),
            "{:?}",
            result.content
        );
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "um\n", "a.txt foi alterado");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "tres\n");
    }

    #[tokio::test]
    async fn patching_a_file_that_is_not_there_is_an_error() {
        let dir = dir_for("missing_file");
        let result = apply_patch(
            "t",
            &dir,
            "--- a/missing.txt\n+++ b/missing.txt\n@@ -1,1 +1,1 @@\n-a\n+b\n",
            false,
        )
        .await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn an_empty_diff_is_refused() {
        let dir = dir_for("empty_diff");
        let result = apply_patch("t", &dir, "   \n", false).await;
        assert!(result.is_error);
    }
}