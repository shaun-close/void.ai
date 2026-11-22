//! The file tools: read, list, glob, grep, edit, write — all on the conversation's overlay.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolKind, ToolOutput, arg_str, confine};
use crate::overlay::Overlay;

const READ_LINES: usize = 2000;
const GLOB_MAX: usize = 500;
const GREP_MAX: usize = 200;
const WALK_MAX: usize = 50_000;

pub fn all() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(Read),
        Arc::new(List),
        Arc::new(Glob),
        Arc::new(Grep),
        Arc::new(Edit),
        Arc::new(Write),
    ]
}

fn blocking(f: impl FnOnce() -> ToolOutput + Send + 'static) -> BoxFuture<'static, ToolOutput> {
    Box::pin(async move {
        tokio::task::spawn_blocking(f)
            .await
            .unwrap_or_else(|e| ToolOutput::err(format!("tool crashed: {e}")))
    })
}

fn path_arg(
    args: &Value,
    ctx: &ToolCtx,
    key: &str,
    default: Option<&str>,
) -> Result<PathBuf, ToolOutput> {
    let raw = match args.get(key).and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => match default {
            Some(d) => d.to_string(),
            None => return Err(ToolOutput::err(format!("missing string argument `{key}`"))),
        },
    };
    confine(&ctx.overlay, &ctx.hidden, &raw).map_err(ToolOutput::err)
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

/// Files under `start` in the merged view, skipping `.git` and ignored paths.
fn walk(ov: &Overlay, start: &Path) -> Vec<PathBuf> {
    let gi = ov.ignore_matcher();
    let mut files = Vec::new();
    let mut stack = vec![start.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(items) = ov.read_dir(&dir) else {
            continue;
        };
        for item in items {
            if item.name == ".git" {
                continue;
            }
            let rel = dir.join(&item.name);
            if ov.ignored(&gi, &rel, item.is_dir) {
                continue;
            }
            if item.is_dir {
                stack.push(rel);
            } else {
                files.push(rel);
                if files.len() >= WALK_MAX {
                    return files;
                }
            }
        }
    }
    files.sort();
    files
}

fn summary_of(args: &Value, key: &str, fallback: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .chars()
        .take(80)
        .collect()
}

struct Read;
impl Tool for Read {
    fn name(&self) -> &'static str {
        "read"
    }
    fn description(&self) -> &'static str {
        "Read a text file from the workspace. Returns numbered lines (at most 2000). Use offset (1-based line) and limit to page through long files."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "path":{"type":"string","description":"File path, relative to the workspace"},
            "offset":{"type":"integer","description":"First line to show (1-based)"},
            "limit":{"type":"integer","description":"How many lines to show (max 2000)"}},
            "required":["path"]})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn summary(&self, args: &Value) -> String {
        summary_of(args, "path", "?")
    }
    fn run(&self, args: Value, ctx: ToolCtx) -> BoxFuture<'static, ToolOutput> {
        blocking(move || {
            let rel = match path_arg(&args, &ctx, "path", None) {
                Ok(r) => r,
                Err(e) => return e,
            };
            let bytes = match ctx.overlay.read(&rel) {
                Ok(b) => b,
                Err(e) => return ToolOutput::err(e.to_string()),
            };
            if is_binary(&bytes) {
                return ToolOutput::err(format!("{} is a binary file", rel.display()));
            }
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().collect();
            let offset = args
                .get("offset")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .max(1) as usize;
            let limit = (args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(READ_LINES as u64) as usize)
                .clamp(1, READ_LINES);
            let start = offset - 1;
            let end = (start + limit).min(lines.len());
            let mut out: String = lines
                .iter()
                .enumerate()
                .skip(start)
                .take(end.saturating_sub(start))
                .map(|(i, l)| format!("{:>6}\t{l}\n", i + 1))
                .collect();
            if start > 0 || end < lines.len() {
                out.push_str(&format!(
                    "(file has {} lines; showing {}–{})",
                    lines.len(),
                    (start + 1).min(lines.len()),
                    end
                ));
            }
            ToolOutput::ok(out.trim_end().to_string())
        })
    }
}

struct List;
impl Tool for List {
    fn name(&self) -> &'static str {
        "list"
    }
    fn description(&self) -> &'static str {
        "List a directory in the workspace (directories end with /)."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string","description":"Directory, relative to the workspace (default: the workspace root)"}}})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn summary(&self, args: &Value) -> String {
        summary_of(args, "path", ".")
    }
    fn run(&self, args: Value, ctx: ToolCtx) -> BoxFuture<'static, ToolOutput> {
        blocking(move || {
            let rel = match path_arg(&args, &ctx, "path", Some(".")) {
                Ok(r) => r,
                Err(e) => return e,
            };
            match ctx.overlay.read_dir(&rel) {
                Ok(items) => ToolOutput::ok(
                    items
                        .into_iter()
                        .filter(|i| !(rel.as_os_str().is_empty() && i.name == ".git"))
                        .map(|i| {
                            if i.is_dir {
                                format!("{}/", i.name)
                            } else {
                                i.name
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                Err(e) => ToolOutput::err(e.to_string()),
            }
        })
    }
}

struct Glob;
impl Tool for Glob {
    fn name(&self) -> &'static str {
        "glob"
    }
    fn description(&self) -> &'static str {
        "Find files by glob pattern (e.g. **/*.rs), respecting .gitignore. Returns workspace-relative paths, at most 500."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "pattern":{"type":"string","description":"Glob pattern, matched against paths relative to `path`"},
            "path":{"type":"string","description":"Directory to search (default: the workspace root)"}},
            "required":["pattern"]})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn summary(&self, args: &Value) -> String {
        summary_of(args, "pattern", "?")
    }
    fn run(&self, args: Value, ctx: ToolCtx) -> BoxFuture<'static, ToolOutput> {
        blocking(move || {
            let pattern = match arg_str(&args, "pattern") {
                Ok(p) => p,
                Err(e) => return e,
            };
            let root = match path_arg(&args, &ctx, "path", Some(".")) {
                Ok(r) => r,
                Err(e) => return e,
            };
            let matcher = match globset::GlobBuilder::new(&pattern)
                .literal_separator(true)
                .build()
            {
                Ok(g) => g.compile_matcher(),
                Err(e) => return ToolOutput::err(format!("bad pattern: {e}")),
            };
            let hits: Vec<String> = walk(&ctx.overlay, &root)
                .into_iter()
                .filter(|p| matcher.is_match(p.strip_prefix(&root).unwrap_or(p)))
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            let n = hits.len();
            let mut out = hits
                .into_iter()
                .take(GLOB_MAX)
                .collect::<Vec<_>>()
                .join("\n");
            if n > GLOB_MAX {
                out.push_str(&format!("\n({} more)", n - GLOB_MAX));
            }
            if n == 0 {
                out = "no matches".into();
            }
            ToolOutput::ok(out)
        })
    }
}

struct Grep;
impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> &'static str {
        "Search file contents with a regular expression, respecting .gitignore. Returns path:line:text, at most 200 lines."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "pattern":{"type":"string","description":"Regular expression (Rust regex syntax)"},
            "path":{"type":"string","description":"Directory to search (default: the workspace root)"},
            "glob":{"type":"string","description":"Only search files whose path matches this glob"}},
            "required":["pattern"]})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    fn summary(&self, args: &Value) -> String {
        summary_of(args, "pattern", "?")
    }
    fn run(&self, args: Value, ctx: ToolCtx) -> BoxFuture<'static, ToolOutput> {
        blocking(move || {
            let pattern = match arg_str(&args, "pattern") {
                Ok(p) => p,
                Err(e) => return e,
            };
            let re = match regex::Regex::new(&pattern) {
                Ok(r) => r,
                Err(e) => return ToolOutput::err(format!("bad pattern: {e}")),
            };
            let root = match path_arg(&args, &ctx, "path", Some(".")) {
                Ok(r) => r,
                Err(e) => return e,
            };
            let filter = match args.get("glob").and_then(Value::as_str) {
                Some(g) => match globset::GlobBuilder::new(g)
                    .literal_separator(false)
                    .build()
                {
                    Ok(g) => Some(g.compile_matcher()),
                    Err(e) => return ToolOutput::err(format!("bad glob: {e}")),
                },
                None => None,
            };
            let mut out = Vec::new();
            let mut more = false;
            'files: for f in walk(&ctx.overlay, &root) {
                if filter.as_ref().is_some_and(|m| !m.is_match(&f)) {
                    continue;
                }
                let Ok(bytes) = ctx.overlay.read(&f) else {
                    continue;
                };
                if is_binary(&bytes) {
                    continue;
                }
                for (i, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
                    if re.is_match(line) {
                        if out.len() >= GREP_MAX {
                            more = true;
                            break 'files;
                        }
                        let line: String = line.chars().take(300).collect();
                        out.push(format!("{}:{}:{line}", f.display(), i + 1));
                    }
                }
            }
            if out.is_empty() {
                return ToolOutput::ok("no matches");
            }
            let mut s = out.join("\n");
            if more {
                s.push_str("\n(more matches not shown)");
            }
            ToolOutput::ok(s)
        })
    }
}

struct Edit;
impl Tool for Edit {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn description(&self) -> &'static str {
        "Replace exact text in a file. old_string must match exactly once unless replace_all is true. Changes go to the pending review, not straight to disk."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "path":{"type":"string"},
            "old_string":{"type":"string","description":"Exact text to replace"},
            "new_string":{"type":"string","description":"Replacement text"},
            "replace_all":{"type":"boolean","description":"Replace every occurrence"}},
            "required":["path","old_string","new_string"]})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }
    fn summary(&self, args: &Value) -> String {
        summary_of(args, "path", "?")
    }
    fn run(&self, args: Value, ctx: ToolCtx) -> BoxFuture<'static, ToolOutput> {
        blocking(move || {
            let rel = match path_arg(&args, &ctx, "path", None) {
                Ok(r) => r,
                Err(e) => return e,
            };
            let (old, new) = match (arg_str(&args, "old_string"), arg_str(&args, "new_string")) {
                (Ok(o), Ok(n)) => (o, n),
                (Err(e), _) | (_, Err(e)) => return e,
            };
            if old.is_empty() {
                return ToolOutput::err("old_string is empty; use write to create a file");
            }
            let all = args
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let text = match ctx.overlay.read(&rel) {
                Ok(b) => String::from_utf8_lossy(&b).into_owned(),
                Err(e) => return ToolOutput::err(e.to_string()),
            };
            let n = text.matches(&old).count();
            if n == 0 {
                return ToolOutput::err(format!("old_string not found in {}", rel.display()));
            }
            if n > 1 && !all {
                return ToolOutput::err(format!(
                    "old_string matches {n} times in {}; add context to make it unique or set replace_all",
                    rel.display()
                ));
            }
            let updated = if all {
                text.replace(&old, &new)
            } else {
                text.replacen(&old, &new, 1)
            };
            match ctx.overlay.write(&rel, updated.as_bytes()) {
                Ok(()) => ToolOutput::ok(format!(
                    "edited {} ({n} replacement{})",
                    rel.display(),
                    if n == 1 { "" } else { "s" }
                )),
                Err(e) => ToolOutput::err(e.to_string()),
            }
        })
    }
}

struct Write;
impl Tool for Write {
    fn name(&self) -> &'static str {
        "write"
    }
    fn description(&self) -> &'static str {
        "Create or overwrite a whole file. Changes go to the pending review, not straight to disk."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "path":{"type":"string"},
            "content":{"type":"string","description":"The complete file contents"}},
            "required":["path","content"]})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }
    fn summary(&self, args: &Value) -> String {
        summary_of(args, "path", "?")
    }
    fn run(&self, args: Value, ctx: ToolCtx) -> BoxFuture<'static, ToolOutput> {
        blocking(move || {
            let rel = match path_arg(&args, &ctx, "path", None) {
                Ok(r) => r,
                Err(e) => return e,
            };
            if rel.as_os_str().is_empty() {
                return ToolOutput::err("path is the workspace itself");
            }
            let content = match arg_str(&args, "content") {
                Ok(c) => c,
                Err(e) => return e,
            };
            if matches!(ctx.overlay.resolve(&rel), crate::overlay::Entry::Dir(_)) {
                return ToolOutput::err(format!("{} is a directory", rel.display()));
            }
            match ctx.overlay.write(&rel, content.as_bytes()) {
                Ok(()) => {
                    ToolOutput::ok(format!("wrote {} ({} bytes)", rel.display(), content.len()))
                }
                Err(e) => ToolOutput::err(e.to_string()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::overlay::Overlay;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    struct T {
        tmp: tempfile::TempDir,
        ws: PathBuf,
        ov: Overlay,
    }

    fn setup() -> T {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let ov = Overlay::open(&tmp.path().join("ovl"), &ws).unwrap();
        T { tmp, ws, ov }
    }

    fn ctx(t: &T, hidden: Vec<PathBuf>) -> ToolCtx {
        ToolCtx {
            workspace: t.ws.clone(),
            overlay: t.ov.clone(),
            hidden: Arc::new(hidden),
            cancel: tokio_util::sync::CancellationToken::new(),
            stream: None,
            sandbox: None,
            target_dir: t.tmp.path().join("target"),
        }
    }

    async fn run(t: &T, tool: &str, args: serde_json::Value) -> ToolOutput {
        let reg = Registry::builtin();
        reg.get(tool).unwrap().run(args, ctx(t, vec![])).await
    }

    #[test]
    fn confine_accepts_inside() {
        let t = setup();
        fs::create_dir_all(t.ws.join("src")).unwrap();
        assert_eq!(
            confine(&t.ov, &[], "src/main.rs").unwrap(),
            PathBuf::from("src/main.rs")
        );
        assert_eq!(
            confine(&t.ov, &[], "./src/../src/x").unwrap(),
            PathBuf::from("src/x")
        );
        let abs = t.ws.join("a.txt");
        assert_eq!(
            confine(&t.ov, &[], abs.to_str().unwrap()).unwrap(),
            PathBuf::from("a.txt")
        );
        assert_eq!(confine(&t.ov, &[], ".").unwrap(), PathBuf::new());
    }

    #[test]
    fn confine_refuses_dotdot() {
        let t = setup();
        assert!(confine(&t.ov, &[], "../outside").is_err());
        assert!(confine(&t.ov, &[], "a/../../b").is_err());
        assert!(confine(&t.ov, &[], "/etc/passwd").is_err());
    }

    #[test]
    fn confine_refuses_symlink_escape() {
        let t = setup();
        let outside = t.tmp.path().join("secret.txt");
        fs::write(&outside, "secret").unwrap();
        std::os::unix::fs::symlink(&outside, t.ws.join("lnk")).unwrap();
        assert!(confine(&t.ov, &[], "lnk").is_err());
        // A link that stays inside is fine and resolves.
        fs::write(t.ws.join("real.txt"), "r").unwrap();
        std::os::unix::fs::symlink("real.txt", t.ws.join("inner")).unwrap();
        assert_eq!(
            confine(&t.ov, &[], "inner").unwrap(),
            PathBuf::from("real.txt")
        );
        if crate::testutil::have_bwrap() {
            crate::testutil::bwrap_exec(&t.ov, "ln -s /etc/passwd p");
            assert!(
                confine(&t.ov, &[], "p").is_err(),
                "symlink made in the layer"
            );
        }
    }

    #[tokio::test]
    async fn read_tool_refuses_symlink_escape() {
        let t = setup();
        let outside = t.tmp.path().join("secret.txt");
        fs::write(&outside, "secret").unwrap();
        std::os::unix::fs::symlink(&outside, t.ws.join("lnk")).unwrap();
        let out = run(&t, "read", json!({"path": "lnk"})).await;
        assert!(out.is_error);
        assert!(!out.content.contains("secret\n"));
    }

    #[test]
    fn confine_refuses_hidden() {
        let t = setup();
        fs::create_dir_all(t.ws.join("keys")).unwrap();
        let hidden = vec![t.ws.join("keys")];
        assert!(confine(&t.ov, &hidden, "keys/id").is_err());
        assert!(confine(&t.ov, &hidden, "other").is_ok());
    }

    #[tokio::test]
    async fn read_numbers_lines_and_limits() {
        let t = setup();
        let body: String = (1..=2500).map(|i| format!("l{i}\n")).collect();
        fs::write(t.ws.join("big.txt"), body).unwrap();
        let out = run(&t, "read", json!({"path": "big.txt"})).await;
        assert!(!out.is_error);
        assert!(out.content.starts_with("     1\tl1\n"));
        assert!(out.content.contains("  2000\tl2000"));
        assert!(!out.content.contains("l2001"));
        assert!(out.content.contains("2500 lines"));
        let part = run(
            &t,
            "read",
            json!({"path": "big.txt", "offset": 10, "limit": 2}),
        )
        .await;
        assert!(part.content.starts_with("    10\tl10\n    11\tl11"));
    }

    #[tokio::test]
    async fn read_refuses_binary() {
        let t = setup();
        fs::write(t.ws.join("b.bin"), [0u8, 1, 2, 0]).unwrap();
        let out = run(&t, "read", json!({"path": "b.bin"})).await;
        assert!(out.is_error);
        assert!(out.content.contains("binary"));
    }

    #[tokio::test]
    async fn edit_requires_unique_match() {
        let t = setup();
        fs::write(t.ws.join("f.rs"), "let a = 1;\nlet a = 1;\n").unwrap();
        let none = run(
            &t,
            "edit",
            json!({"path":"f.rs","old_string":"zzz","new_string":"y"}),
        )
        .await;
        assert!(none.is_error);
        let many = run(
            &t,
            "edit",
            json!({"path":"f.rs","old_string":"let a","new_string":"let b"}),
        )
        .await;
        assert!(many.is_error);
        assert!(many.content.contains("2"));
    }

    #[tokio::test]
    async fn edit_replace_all() {
        let t = setup();
        fs::write(t.ws.join("f.rs"), "let a = 1;\nlet a = 1;\n").unwrap();
        let out = run(
            &t,
            "edit",
            json!({"path":"f.rs","old_string":"let a","new_string":"let b","replace_all":true}),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            t.ov.read(std::path::Path::new("f.rs")).unwrap(),
            b"let b = 1;\nlet b = 1;\n"
        );
        assert_eq!(
            fs::read_to_string(t.ws.join("f.rs")).unwrap(),
            "let a = 1;\nlet a = 1;\n"
        );
    }

    #[tokio::test]
    async fn write_lands_in_overlay() {
        let t = setup();
        let out = run(&t, "write", json!({"path":"new/x.txt","content":"hello"})).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(!t.ws.join("new").exists());
        assert_eq!(
            t.ov.read(std::path::Path::new("new/x.txt")).unwrap(),
            b"hello"
        );
    }

    #[tokio::test]
    async fn list_shows_merged_dir() {
        let t = setup();
        fs::write(t.ws.join("a"), "a").unwrap();
        fs::create_dir_all(t.ws.join("d")).unwrap();
        t.ov.write(std::path::Path::new("b"), b"b").unwrap();
        let out = run(&t, "list", json!({})).await;
        assert_eq!(out.content, "a\nb\nd/");
    }

    #[tokio::test]
    async fn glob_respects_gitignore() {
        let t = setup();
        fs::write(t.ws.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(t.ws.join("src")).unwrap();
        fs::create_dir_all(t.ws.join("target")).unwrap();
        fs::write(t.ws.join("src/main.rs"), "").unwrap();
        fs::write(t.ws.join("target/gen.rs"), "").unwrap();
        t.ov.write(std::path::Path::new("src/lib.rs"), b"").unwrap();
        let out = run(&t, "glob", json!({"pattern":"**/*.rs"})).await;
        assert_eq!(out.content, "src/lib.rs\nsrc/main.rs");
    }

    #[tokio::test]
    async fn grep_finds_in_overlay_edits() {
        let t = setup();
        fs::write(t.ws.join("a.txt"), "alpha\nbeta\n").unwrap();
        t.ov.write(std::path::Path::new("b.txt"), b"gamma\nalphabet\n")
            .unwrap();
        let out = run(&t, "grep", json!({"pattern":"alpha"})).await;
        assert_eq!(out.content, "a.txt:1:alpha\nb.txt:2:alphabet");
    }

    #[test]
    fn registry_specs_have_schemas() {
        let reg = Registry::builtin();
        let names: Vec<String> = ["read", "list", "glob", "grep", "edit", "write"]
            .map(String::from)
            .to_vec();
        let specs = reg.specs(&names);
        assert_eq!(specs.len(), 6);
        for s in specs {
            assert_eq!(s.parameters["type"], "object", "{}", s.name);
            assert!(!s.description.is_empty());
        }
    }
}
