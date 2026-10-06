// Filesystem tools: read, write, list, search.
//
// Every path is resolved inside `ToolCtx::cwd` — a model that says
// `../../Windows/System32` gets a path it cannot escape from, and the four
// tools never touch anything the user has not already pointed the agent at.

use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};

use crate::claude::ToolResult;

/// Hard ceiling on a single read. Above this the tool errors instead of
/// handing the model a megabyte it cannot use.
const MAX_READ: u64 = 1_048_576;
/// Default number of entries `list_dir` returns.
const MAX_ENTRIES: usize = 200;
/// Default number of matches `grep` returns.
const MAX_MATCHES: usize = 100;

/// Joins `path` onto the working directory and refuses to escape it.
///
/// `..` is resolved lexically rather than by touching the disk, so a path that
/// does not exist yet still cannot climb out.
pub fn resolve(cwd: &Path, path: &str) -> Result<PathBuf, String> {
    let raw = Path::new(path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };

    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                // `..` at the root has nowhere to go, and popping past the cwd
                // would leave the directory the user authorised.
                if !out.pop() || !out.starts_with(cwd) {
                    return Err(format!("'{path}' sai do diretório de trabalho"));
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    Ok(out)
}

/// `read_file`, with optional line window.
pub async fn read_file(
    id: &str,
    cwd: &Path,
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> ToolResult {
    let target = match resolve(cwd, path) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(id, e),
    };

    let meta = match tokio::fs::metadata(&target).await {
        Ok(m) => m,
        Err(e) => return ToolResult::err(id, format!("{path}: {e}")),
    };
    if meta.len() > MAX_READ {
        return ToolResult::err(
            id,
            format!(
                "{path} tem {} bytes, acima do limite de {MAX_READ}",
                meta.len()
            ),
        );
    }

    let text = match tokio::fs::read_to_string(&target).await {
        Ok(t) => t,
        Err(e) => return ToolResult::err(id, format!("{path}: {e}")),
    };

    // A window is asked for with offset/limit; without them the whole file.
    let content = match (offset, limit) {
        (None, None) => text.clone(),
        (offset, limit) => {
            let lines: Vec<&str> = text.lines().collect();
            let start = offset.unwrap_or(0).min(lines.len());
            let end = limit
                .map(|l| start.saturating_add(l).min(lines.len()))
                .unwrap_or(lines.len());
            let window = lines[start..end].join("\n");
            format!(
                "(linhas {}..{end} de {})\n{window}",
                start + 1,
                lines.len()
            )
        }
    };

    ToolResult::ok(
        id,
        json!({
            "path": path,
            "bytes": meta.len(),
            "lines": text.lines().count(),
            "content": content,
        }),
    )
}

/// `write_file`: atomic, and a no-op while `dry_run` is on.
pub async fn write_file(
    id: &str,
    cwd: &Path,
    path: &str,
    contents: &str,
    dry_run: bool,
) -> ToolResult {
    let target = match resolve(cwd, path) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(id, e),
    };

    let bytes = contents.len();
    if dry_run {
        return ToolResult::ok(
            id,
            json!({
                "would_write": path,
                "bytes": bytes,
                "dry_run": true,
                "note": "nada foi escrito: dry-run ligado",
            }),
        );
    }

    if let Some(parent) = target.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return ToolResult::err(id, format!("{}: {e}", parent.display()));
        }
    }

    // Write beside the target and rename over it: a reader either sees the old
    // file or the new one, never a half-written one.
    let temp = target.with_extension(format!("coucou-tmp-{}", std::process::id()));
    if let Err(e) = tokio::fs::write(&temp, contents).await {
        return ToolResult::err(id, format!("{}: {e}", temp.display()));
    }
    if let Err(e) = tokio::fs::rename(&temp, &target).await {
        let _ = tokio::fs::remove_file(&temp).await;
        return ToolResult::err(id, format!("{}: {e}", target.display()));
    }

    ToolResult::ok(
        id,
        json!({
            "wrote": path,
            "bytes": bytes,
            "dry_run": false,
        }),
    )
}

/// `list_dir`: walks with `ignore`, so `.git`, `node_modules` and `target` never
/// show up and `.gitignore` is respected.
pub fn list_dir(
    id: &str,
    cwd: &Path,
    path: &str,
    glob: Option<&str>,
    max: Option<usize>,
) -> ToolResult {
    let root = match resolve(cwd, path) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(id, e),
    };
    if !root.is_dir() {
        return ToolResult::err(id, format!("{path} não é um diretório"));
    }

    let limit = max.unwrap_or(MAX_ENTRIES);
    let mut walker = ignore::WalkBuilder::new(&root);
    walker
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .standard_filters(true);

    let mut entries: Vec<Value> = Vec::new();
    let mut total = 0usize;
    for entry in walker.build().filter_map(|e| e.ok()) {
        // The root itself is not a result.
        if entry.path() == root {
            continue;
        }
        let file_path = entry.path().to_path_buf();
        if !keep(&file_path, glob) {
            continue;
        }
        total += 1;
        if entries.len() >= limit {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let size = if is_dir {
            0
        } else {
            entry.metadata().map(|m| m.len()).unwrap_or(0)
        };
        let relative = entry
            .path()
            .strip_prefix(&root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        entries.push(json!({
            "path": relative,
            "is_dir": is_dir,
            "size": size,
        }));
    }

    ToolResult::ok(
        id,
        json!({
            "root": path,
            "entries": entries,
            "returned": entries.len(),
            "total": total,
            "truncated": total > limit,
        }),
    )
}

/// `grep`: regex over the tree, `.gitignore` respected.
pub fn grep(
    id: &str,
    cwd: &Path,
    pattern: &str,
    path: Option<&str>,
    glob: Option<&str>,
    max: Option<usize>,
) -> ToolResult {
    let root = match path {
        Some(p) => match resolve(cwd, p) {
            Ok(resolved) => resolved,
            Err(e) => return ToolResult::err(id, e),
        },
        None => cwd.to_path_buf(),
    };
    if !root.exists() {
        let shown = path.unwrap_or(".");
        return ToolResult::err(id, format!("{shown} não existe"));
    }

    let matcher = match grep_regex::RegexMatcherBuilder::new()
        .case_insensitive(false)
        .build(pattern)
    {
        Ok(m) => m,
        Err(e) => return ToolResult::err(id, format!("padrão inválido: {e}")),
    };

    let mut builder = grep_searcher::SearcherBuilder::new();
    builder.line_number(true);
    let mut searcher = builder.build();

    let limit = max.unwrap_or(MAX_MATCHES);
    let mut sink = MatchSink {
        current: String::new(),
        limit,
        hits: Vec::new(),
        total: 0,
    };

    let mut walker = ignore::WalkBuilder::new(&root);
    walker
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .parents(true)
        .standard_filters(true);
    for entry in walker.build().filter_map(|e| e.ok()) {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let file_path = entry.path().to_path_buf();
        if !keep(&file_path, glob) {
            continue;
        }
        // `SinkMatch` carries no path of its own, so the sink is told which
        // file it is being fed before each search.
        sink.current = file_path.to_string_lossy().replace('\\', "/");
        // A file we cannot read is skipped, not fatal: one binary in the tree
        // should not end the search.
        let _ = searcher.search_path(&matcher, &file_path, &mut sink);
        if sink.total >= limit {
            break;
        }
    }

    let truncated = sink.total > sink.hits.len();
    ToolResult::ok(
        id,
        json!({
            "pattern": pattern,
            "matches": sink.hits,
            "returned": sink.hits.len(),
            "total": sink.total,
            "truncated": truncated,
        }),
    )
}

/// Collects matches, stopping at `limit` but still counting the rest.
struct MatchSink {
    /// The file being searched, set by the caller before each search.
    current: String,
    limit: usize,
    hits: Vec<Value>,
    total: usize,
}

impl grep_searcher::Sink for MatchSink {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &grep_searcher::SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        self.total += 1;
        if self.hits.len() < self.limit {
            let text = String::from_utf8_lossy(mat.bytes());
            let line = text.lines().next().unwrap_or("").trim().to_string();
            self.hits.push(json!({
                "file": self.current,
                "line": mat.line_number().unwrap_or(0),
                "text": line,
            }));
        }
        // Keep going: the total has to be honest even past the limit.
        Ok(true)
    }
}

/// The small glob shapes a model actually uses: `*.rs`, `src/**`, `main.rs`.
fn keep(path: &Path, glob: Option<&str>) -> bool {
    let Some(pattern) = glob else {
        return true;
    };
    let text = path.to_string_lossy().replace('\\', "/");
    match pattern.split_once("**") {
        Some((prefix, suffix)) => {
            let suffix = suffix.trim_start_matches('/');
            text.contains(prefix) && (suffix.is_empty() || text.ends_with(suffix))
        }
        None => match pattern.strip_prefix('*') {
            Some(ext) => text.ends_with(ext),
            None => text.contains(pattern),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory for one test, wiped on entry.
    ///
    /// Named per test so tests running in parallel cannot delete each other's
    /// files; the pid only keeps two `cargo test` runs apart. Cleared on entry
    /// rather than on exit, so a failing test leaves its evidence behind.
    fn dir_for(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("coucou-fs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("a temp directory for this test");
        base
    }

    #[test]
    fn a_relative_path_lands_inside_the_working_directory() {
        let cwd = Path::new("C:\\work");
        assert_eq!(
            resolve(cwd, "src\\main.rs").unwrap(),
            PathBuf::from("C:\\work\\src\\main.rs")
        );
    }

    #[test]
    fn parent_directories_cannot_climb_out() {
        let cwd = Path::new("C:\\work");
        assert!(resolve(cwd, "../../Windows").is_err());
        assert!(resolve(cwd, "..\\..").is_err());
    }

    #[test]
    fn a_path_that_returns_to_the_same_place_is_fine() {
        let cwd = Path::new("C:\\work");
        assert!(resolve(cwd, "src/../src/main.rs").is_ok());
    }

    #[tokio::test]
    async fn a_dry_run_write_reports_and_writes_nothing() {
        let dir = dir_for("write_dry_run");
        let target = dir.join("dry-run.txt");
        let result = write_file("t", &dir, "dry-run.txt", "oi", true).await;

        assert!(!result.is_error);
        assert_eq!(result.content["would_write"], "dry-run.txt");
        assert_eq!(result.content["bytes"], 2);
        assert!(!target.exists(), "dry-run não pode criar arquivo");
    }

    #[tokio::test]
    async fn a_real_write_creates_the_file_and_its_parents() {
        let dir = dir_for("write_real");
        let result = write_file("t", &dir, "nested/deep/file.txt", "conteudo", false).await;

        assert!(!result.is_error, "{:?}", result.content);
        assert_eq!(result.content["bytes"], 8);
        assert_eq!(
            std::fs::read_to_string(dir.join("nested").join("deep").join("file.txt")).unwrap(),
            "conteudo"
        );
    }

    #[tokio::test]
    async fn reading_a_file_that_is_not_there_is_an_error() {
        let dir = dir_for("read_missing");
        let result = read_file("t", &dir, "missing.txt", None, None).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn reading_with_a_window_returns_only_those_lines() {
        let dir = dir_for("read_window");
        write_file("t", &dir, "lines.txt", "a\nb\nc\nd\ne", false).await;

        let all = read_file("t", &dir, "lines.txt", None, None).await;
        assert_eq!(all.content["lines"], 5);

        let window = read_file("t", &dir, "lines.txt", Some(1), Some(2)).await;
        let content = window.content["content"].as_str().unwrap();
        assert!(content.contains("(linhas 2..3 de 5)"), "{content}");
        // Compared as lines, not as a substring: the header itself contains a
        // 'd' in "de", which a `!contains('d')` check would trip over.
        let lines: Vec<&str> = content.lines().skip(1).collect();
        assert_eq!(lines, ["b", "c"], "a janela tem só as linhas 2 e 3");
    }

    #[tokio::test]
    async fn listing_a_directory_reports_entries_and_a_total() {
        let dir = dir_for("list_dir");
        write_file("t", &dir, "a.rs", "fn a() {}", false).await;
        write_file("t", &dir, "b.rs", "fn b() {}", false).await;

        let result = list_dir("t", &dir, ".", None, None);
        assert!(!result.is_error, "{:?}", result.content);
        assert_eq!(result.content["total"], 2);
        let entries = result.content["entries"].as_array().unwrap();
        assert!(entries.iter().any(|e| e["path"] == "a.rs"));
    }

    #[tokio::test]
    async fn grep_finds_matches_and_reports_the_real_total() {
        let dir = dir_for("grep_matches");
        write_file("t", &dir, "code.rs", "fn main() {}\nfn main() {}\n", false).await;

        let result = grep("t", &dir, "fn main", None, None, Some(1));
        assert!(!result.is_error, "{:?}", result.content);
        assert_eq!(result.content["total"], 2, "o total conta o que foi cortado");
        assert_eq!(result.content["returned"], 1);
        assert_eq!(result.content["truncated"], true);
        let hits = result.content["matches"].as_array().unwrap();
        assert_eq!(hits[0]["line"], 1);
        assert!(hits[0]["text"].as_str().unwrap().contains("fn main"));
    }

    #[test]
    fn an_invalid_pattern_is_an_error_not_a_panic() {
        let dir = dir_for("grep_bad_pattern");
        let result = grep("t", &dir, "([unclosed", None, None, None);
        assert!(result.is_error);
        assert!(result.content.as_str().unwrap().contains("padrão"));
    }

    #[test]
    fn the_glob_shapes_a_model_uses_are_covered() {
        assert!(keep(Path::new("src/main.rs"), Some("*.rs")));
        assert!(!keep(Path::new("src/main.txt"), Some("*.rs")));
        assert!(keep(Path::new("src/a/b.rs"), Some("src/**")));
        assert!(keep(Path::new("src/a/main.rs"), Some("main.rs")));
        assert!(keep(Path::new("anything"), None));
    }
}