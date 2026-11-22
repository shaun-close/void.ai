//! Languages the editor knows and how to find them from a path.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lang {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Json,
    Toml,
    Yaml,
    Markdown,
    Css,
    Html,
    Bash,
    Python,
    C,
    Cpp,
    /// Highlighted by syntect; carries the syntect syntax name.
    Other(String),
    Plain,
}

impl Lang {
    /// Pick a language from the file name, then the extension.
    pub fn for_path(path: &Path) -> Lang {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        match name {
            "Cargo.lock" => return Lang::Toml,
            ".bashrc" | ".bash_profile" | ".bash_aliases" | ".zshrc" | ".zprofile" | ".profile" => {
                return Lang::Bash;
            }
            "Makefile" | "makefile" | "GNUmakefile" => return Lang::Other("Makefile".into()),
            _ => {}
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let other = |n: &str| Lang::Other(n.to_string());
        match ext.as_str() {
            "rs" => Lang::Rust,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
            "json" | "jsonc" => Lang::Json,
            "toml" => Lang::Toml,
            "yaml" | "yml" => Lang::Yaml,
            "md" | "markdown" => Lang::Markdown,
            "css" => Lang::Css,
            "html" | "htm" => Lang::Html,
            "sh" | "bash" | "zsh" => Lang::Bash,
            "py" | "pyi" => Lang::Python,
            "c" | "h" => Lang::C,
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Lang::Cpp,
            "go" => other("Go"),
            "java" => other("Java"),
            "rb" => other("Ruby"),
            "php" => other("PHP"),
            "sql" => other("SQL"),
            "lua" => other("Lua"),
            "xml" => other("XML"),
            "cs" => other("C#"),
            "hs" => other("Haskell"),
            "pl" | "pm" => other("Perl"),
            "r" => other("R"),
            "tex" => other("LaTeX"),
            "diff" | "patch" => other("Diff"),
            "mk" => other("Makefile"),
            _ => Lang::Plain,
        }
    }
}

/// Number of tree-sitter languages; indexes the per-language query cache.
pub(crate) const TS_LANGS: usize = 14;

impl Lang {
    /// Dense index of a tree-sitter backed language, `None` for syntect/plain.
    pub(crate) fn ts_index(&self) -> Option<usize> {
        Some(match self {
            Lang::Rust => 0,
            Lang::TypeScript => 1,
            Lang::Tsx => 2,
            Lang::JavaScript => 3,
            Lang::Json => 4,
            Lang::Toml => 5,
            Lang::Yaml => 6,
            Lang::Markdown => 7,
            Lang::Css => 8,
            Lang::Html => 9,
            Lang::Bash => 10,
            Lang::Python => 11,
            Lang::C => 12,
            Lang::Cpp => 13,
            Lang::Other(_) | Lang::Plain => return None,
        })
    }

    pub(crate) fn ts_language(&self) -> Option<tree_sitter::Language> {
        Some(match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::Json => tree_sitter_json::LANGUAGE.into(),
            Lang::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Lang::Yaml => tree_sitter_yaml::LANGUAGE.into(),
            Lang::Markdown => tree_sitter_md::LANGUAGE.into(),
            Lang::Css => tree_sitter_css::LANGUAGE.into(),
            Lang::Html => tree_sitter_html::LANGUAGE.into(),
            Lang::Bash => tree_sitter_bash::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::C => tree_sitter_c::LANGUAGE.into(),
            Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Lang::Other(_) | Lang::Plain => return None,
        })
    }

    /// The bundled highlights query; TypeScript and C++ build on their base grammar's query.
    pub(crate) fn highlights_query(&self) -> Option<String> {
        use tree_sitter_javascript as js;
        Some(match self {
            Lang::Rust => tree_sitter_rust::HIGHLIGHTS_QUERY.into(),
            Lang::TypeScript => format!(
                "{}\n{}",
                js::HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY
            ),
            Lang::Tsx => format!(
                "{}\n{}\n{}",
                js::HIGHLIGHT_QUERY,
                js::JSX_HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY
            ),
            Lang::JavaScript => format!("{}\n{}", js::HIGHLIGHT_QUERY, js::JSX_HIGHLIGHT_QUERY),
            Lang::Json => tree_sitter_json::HIGHLIGHTS_QUERY.into(),
            Lang::Toml => tree_sitter_toml_ng::HIGHLIGHTS_QUERY.into(),
            Lang::Yaml => tree_sitter_yaml::HIGHLIGHTS_QUERY.into(),
            Lang::Markdown => tree_sitter_md::HIGHLIGHT_QUERY_BLOCK.into(),
            Lang::Css => tree_sitter_css::HIGHLIGHTS_QUERY.into(),
            Lang::Html => tree_sitter_html::HIGHLIGHTS_QUERY.into(),
            Lang::Bash => tree_sitter_bash::HIGHLIGHT_QUERY.into(),
            Lang::Python => tree_sitter_python::HIGHLIGHTS_QUERY.into(),
            Lang::C => tree_sitter_c::HIGHLIGHT_QUERY.into(),
            Lang::Cpp => format!(
                "{}\n{}",
                tree_sitter_c::HIGHLIGHT_QUERY,
                tree_sitter_cpp::HIGHLIGHT_QUERY
            ),
            Lang::Other(_) | Lang::Plain => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_for_common_paths() {
        let l = |p: &str| Lang::for_path(Path::new(p));
        assert_eq!(l("src/main.rs"), Lang::Rust);
        assert_eq!(l("a/b.tsx"), Lang::Tsx);
        assert_eq!(l("a/b.ts"), Lang::TypeScript);
        assert_eq!(l("x.JS"), Lang::JavaScript);
        assert_eq!(l("Cargo.lock"), Lang::Toml);
        assert_eq!(l("Cargo.toml"), Lang::Toml);
        assert_eq!(l("/home/u/.bashrc"), Lang::Bash);
        assert_eq!(l("run.sh"), Lang::Bash);
        assert_eq!(l("a.yml"), Lang::Yaml);
        assert_eq!(l("README.md"), Lang::Markdown);
        assert_eq!(l("a.hpp"), Lang::Cpp);
        assert_eq!(l("a.h"), Lang::C);
        assert_eq!(l("main.go"), Lang::Other("Go".into()));
        assert_eq!(l("Makefile"), Lang::Other("Makefile".into()));
        assert_eq!(l("blob.bin"), Lang::Plain);
        assert_eq!(l("noext"), Lang::Plain);
    }
}
