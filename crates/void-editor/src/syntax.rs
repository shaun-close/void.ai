//! Syntax highlighting (tree-sitter, syntect fallback) and the symbol outline.
//!
//! Spans are in char columns within a line; byte offsets stay inside this module.

use crate::buffer::Edit;
use crate::lang::{Lang, TS_LANGS};
use ropey::Rope;
use std::cmp::Reverse;
use std::ops::Range;
use std::sync::OnceLock;
use streaming_iterator::StreamingIterator;
pub use tree_sitter::Tree;
use tree_sitter::{InputEdit, Node, Parser, Query, QueryCursor};

/// Theme-independent highlight classes; the GUI maps these to colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hl {
    Keyword,
    Type,
    Function,
    String,
    Number,
    Comment,
    Constant,
    Macro,
    Attribute,
    Lifetime,
    Tag,
    Property,
    Variable,
    Namespace,
    Parameter,
    Operator,
    Punctuation,
    Heading,
    Link,
    Escape,
}

/// A coloured run of chars `start..end` within one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub hl: Hl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Impl,
    Module,
    Class,
    Interface,
    TypeAlias,
    Heading,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// Zero-based line of the declaration.
    pub line: usize,
}

/// Capture-name prefixes to [`Hl`]. A name tries its full dotted form, then drops trailing
/// segments, so `function.method` falls back to `function`.
const CAPTURES: &[(&str, Hl)] = &[
    ("keyword.operator", Hl::Operator),
    ("keyword", Hl::Keyword),
    ("include", Hl::Keyword),
    ("conditional", Hl::Keyword),
    ("repeat", Hl::Keyword),
    ("type", Hl::Type),
    ("constructor", Hl::Type),
    ("function.macro", Hl::Macro),
    ("function", Hl::Function),
    ("method", Hl::Function),
    ("macro", Hl::Macro),
    ("string.escape", Hl::Escape),
    ("string.special.url", Hl::Link),
    ("string", Hl::String),
    ("character", Hl::String),
    ("escape", Hl::Escape),
    ("number", Hl::Number),
    ("float", Hl::Number),
    ("comment", Hl::Comment),
    ("constant", Hl::Constant),
    ("boolean", Hl::Constant),
    ("attribute", Hl::Attribute),
    ("label", Hl::Lifetime),
    ("lifetime", Hl::Lifetime),
    ("tag", Hl::Tag),
    ("property", Hl::Property),
    ("field", Hl::Property),
    ("variable.parameter", Hl::Parameter),
    ("parameter", Hl::Parameter),
    ("variable", Hl::Variable),
    ("module", Hl::Namespace),
    ("namespace", Hl::Namespace),
    ("operator", Hl::Operator),
    ("punctuation", Hl::Punctuation),
    ("delimiter", Hl::Punctuation),
    ("text.title", Hl::Heading),
    ("markup.heading", Hl::Heading),
    ("title", Hl::Heading),
    ("text.uri", Hl::Link),
    ("text.reference", Hl::Link),
    ("markup.link", Hl::Link),
    ("text.literal", Hl::String),
    ("markup.raw", Hl::String),
];

fn hl_for_capture(name: &str) -> Option<Hl> {
    let mut name = name;
    loop {
        if let Some((_, hl)) = CAPTURES.iter().find(|(n, _)| *n == name) {
            return Some(*hl);
        }
        name = name.rsplit_once('.')?.0;
    }
}

/// A compiled highlights query with each capture already mapped to an [`Hl`].
struct TsConfig {
    query: Query,
    hl: Vec<Option<Hl>>,
}

fn ts_config(lang: &Lang) -> Option<&'static TsConfig> {
    static CONFIGS: [OnceLock<Option<TsConfig>>; TS_LANGS] = [const { OnceLock::new() }; TS_LANGS];
    CONFIGS[lang.ts_index()?]
        .get_or_init(|| {
            let query = Query::new(&lang.ts_language()?, &lang.highlights_query()?).ok()?;
            let hl = query
                .capture_names()
                .iter()
                .map(|n| hl_for_capture(n))
                .collect();
            Some(TsConfig { query, hl })
        })
        .as_ref()
}

pub struct Syntax {
    lang: Lang,
    tree: Option<Tree>,
    version: u64,
    /// Newest buffer version reported through [`Syntax::edit`]; older trees are refused.
    latest: u64,
    stale: bool,
}

impl Syntax {
    pub fn new(lang: Lang) -> Syntax {
        Syntax {
            lang,
            tree: None,
            version: 0,
            latest: 0,
            stale: false,
        }
    }

    pub fn lang(&self) -> &Lang {
        &self.lang
    }

    /// Buffer version the current tree was parsed from.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// True once an edit has been applied that the tree has not been re-parsed for.
    pub fn is_stale(&self) -> bool {
        self.stale
    }

    /// Parse the whole rope from scratch. Works on a cloned rope, so it can run on a thread;
    /// feed the result to [`Syntax::set_tree`]. `None` for languages without a grammar.
    pub fn parse_full(&self, rope: &Rope) -> Option<Tree> {
        let mut parser = Parser::new();
        parser.set_language(&self.lang.ts_language()?).ok()?;
        let len = rope.len_bytes();
        parser.parse_with_options(
            &mut |byte, _| -> &[u8] {
                if byte >= len {
                    return &[];
                }
                let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
                &chunk.as_bytes()[byte - chunk_start..]
            },
            None,
            None,
        )
    }

    /// Install a finished parse of buffer `version`. A tree older than the newest version
    /// seen by [`Syntax::edit`] is dropped (returns `false`): its positions no longer match the
    /// text and the caller should re-parse.
    pub fn set_tree(&mut self, tree: Tree, version: u64) -> bool {
        if version < self.latest {
            return false;
        }
        self.tree = Some(tree);
        self.version = version;
        self.latest = version;
        self.stale = false;
        true
    }

    /// Keep the current tree's positions in step with a buffer edit until a fresh parse lands.
    /// `version` is the buffer version after the edit; it makes [`Syntax::set_tree`] refuse
    /// parses of older text.
    pub fn edit(&mut self, edit: &Edit, version: u64) {
        self.latest = self.latest.max(version);
        if let Some(tree) = &mut self.tree {
            let point = |p: crate::buffer::Point| tree_sitter::Point {
                row: p.row,
                column: p.column,
            };
            tree.edit(&InputEdit {
                start_byte: edit.start_byte,
                old_end_byte: edit.old_end_byte,
                new_end_byte: edit.new_end_byte,
                start_position: point(edit.start_point),
                old_end_position: point(edit.old_end_point),
                new_end_position: point(edit.new_end_point),
            });
            self.stale = true;
        }
    }

    /// Spans for each line in `lines` (one entry per requested line, empty past the end),
    /// from the current tree even when stale. Work is proportional to the requested lines.
    pub fn highlight(&self, rope: &Rope, lines: Range<usize>) -> Vec<Vec<Span>> {
        let mut out = vec![Vec::new(); lines.len()];
        let total = rope.len_lines();
        let (first, last) = (lines.start.min(total), lines.end.min(total));
        if first >= last {
            return out;
        }
        let rows = match &self.lang {
            Lang::Plain => return out,
            Lang::Other(name) => syntect_rows(name, rope, first..last),
            _ => self.tree_rows(rope, first..last),
        };
        for (i, spans) in rows.into_iter().enumerate() {
            out[first - lines.start + i] = spans;
        }
        out
    }

    fn tree_rows(&self, rope: &Rope, rows: Range<usize>) -> Vec<Vec<Span>> {
        let empty = vec![Vec::new(); rows.len()];
        let (Some(tree), Some(config)) = (&self.tree, ts_config(&self.lang)) else {
            return empty;
        };
        let len = rope.len_bytes();
        let from = rope.line_to_byte(rows.start);
        let to = if rows.end >= rope.len_lines() {
            len
        } else {
            rope.line_to_byte(rows.end)
        };
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(from..to);
        let text = |node: Node| {
            let r = node.byte_range();
            rope.byte_slice(clamp_byte(rope, r.start)..clamp_byte(rope, r.end))
                .chunks()
                .map(str::as_bytes)
        };
        // (start byte, end byte, pattern, hl)
        let mut found: Vec<(usize, usize, usize, Hl)> = Vec::new();
        let mut captures = cursor.captures(&config.query, tree.root_node(), text);
        while let Some((m, idx)) = captures.next() {
            let capture = m.captures()[*idx];
            if let Some(hl) = config.hl[capture.index as usize] {
                let r = capture.node.byte_range();
                let (start, end) = (clamp_byte(rope, r.start), clamp_byte(rope, r.end));
                if start < end {
                    found.push((start, end, m.pattern_index, hl));
                }
            }
        }
        // Inner nodes paint over outer ones; for the same range a later pattern wins.
        found.sort_by_key(|&(s, e, pattern, _)| (Reverse(e - s), pattern));

        let mut cells: Vec<Vec<Option<Hl>>> = rows
            .clone()
            .map(|r| vec![None; line_chars(rope, r)])
            .collect();
        for (s, e, _, hl) in found {
            let (row_s, row_e) = (rope.byte_to_line(s), rope.byte_to_line(e));
            for row in row_s.max(rows.start)..=row_e.min(rows.end - 1) {
                let line_start = rope.line_to_char(row);
                let cells = &mut cells[row - rows.start];
                let col_s = if row == row_s {
                    rope.byte_to_char(s) - line_start
                } else {
                    0
                };
                let col_e = if row == row_e {
                    rope.byte_to_char(e) - line_start
                } else {
                    cells.len()
                };
                let col_e = col_e.min(cells.len());
                if col_s < col_e {
                    cells[col_s..col_e].fill(Some(hl));
                }
            }
        }
        cells.into_iter().map(|c| runs(&c)).collect()
    }

    /// Functions, types, impls, modules and classes in the current tree, in source order.
    pub fn outline(&self, rope: &Rope) -> Vec<Symbol> {
        let mut out = Vec::new();
        if let Some(tree) = &self.tree {
            collect_symbols(&self.lang, tree.root_node(), rope, false, 0, &mut out);
        }
        out
    }
}

/// Clamp `byte` into the rope and back to the start of the char it falls in, so it is always
/// a valid slice boundary even when a stale tree disagrees with the text.
fn clamp_byte(rope: &Rope, byte: usize) -> usize {
    rope.char_to_byte(rope.byte_to_char(byte.min(rope.len_bytes())))
}

/// Chars in `row` excluding its line break.
fn line_chars(rope: &Rope, row: usize) -> usize {
    let line = rope.line(row);
    let n = line.len_chars();
    if n > 0 && line.char(n - 1) == '\n' {
        n - 1
    } else {
        n
    }
}

/// Collapse per-char highlights into spans of equal runs.
fn runs(cells: &[Option<Hl>]) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    for (i, cell) in cells.iter().enumerate() {
        let Some(hl) = *cell else { continue };
        match spans.last_mut() {
            Some(last) if last.end == i && last.hl == hl => last.end = i + 1,
            _ => spans.push(Span {
                start: i,
                end: i + 1,
                hl,
            }),
        }
    }
    spans
}

// ---- syntect fallback -------------------------------------------------------------------

/// Lines of context syntect re-reads before the window to recover its parse state.
const SYNTECT_CONTEXT: usize = 200;

fn syntect_set() -> &'static syntect::parsing::SyntaxSet {
    static SET: OnceLock<syntect::parsing::SyntaxSet> = OnceLock::new();
    SET.get_or_init(syntect::parsing::SyntaxSet::load_defaults_newlines)
}

/// Scope prefixes, innermost scope first; the first match wins.
const SCOPES: &[(&str, Option<Hl>)] = &[
    ("punctuation.definition.string", Some(Hl::String)),
    ("punctuation.definition.comment", Some(Hl::Comment)),
    ("punctuation", None),
    ("comment", Some(Hl::Comment)),
    ("string", Some(Hl::String)),
    ("constant.character.escape", Some(Hl::Escape)),
    ("constant.numeric", Some(Hl::Number)),
    ("constant", Some(Hl::Constant)),
    ("keyword.operator", Some(Hl::Operator)),
    ("keyword", Some(Hl::Keyword)),
    // Grammars put `func`, `fn`, `class` and builtin type names all under `storage`.
    ("storage", Some(Hl::Keyword)),
    ("entity.name.function", Some(Hl::Function)),
    ("entity.name.tag", Some(Hl::Tag)),
    ("entity.name", Some(Hl::Type)),
    ("entity.other.attribute-name", Some(Hl::Attribute)),
    ("entity.other.inherited-class", Some(Hl::Type)),
    ("support.function", Some(Hl::Function)),
    ("support.type", Some(Hl::Type)),
    ("support.class", Some(Hl::Type)),
    ("support.constant", Some(Hl::Constant)),
    ("variable.parameter", Some(Hl::Parameter)),
    ("variable.language", Some(Hl::Keyword)),
    ("variable", Some(Hl::Variable)),
    ("markup.heading", Some(Hl::Heading)),
    ("markup.underline.link", Some(Hl::Link)),
];

fn hl_for_scope(scope: &str) -> Option<Option<Hl>> {
    SCOPES
        .iter()
        .find(|(prefix, _)| {
            scope
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
        })
        .map(|(_, hl)| *hl)
}

fn hl_for_stack(stack: &syntect::parsing::ScopeStack) -> Option<Hl> {
    for scope in stack.as_slice().iter().rev() {
        if let Some(Some(hl)) = hl_for_scope(&scope.build_string()) {
            return Some(hl);
        }
    }
    None
}

fn syntect_rows(name: &str, rope: &Rope, rows: Range<usize>) -> Vec<Vec<Span>> {
    use syntect::parsing::{ParseState, ScopeStack};
    let set = syntect_set();
    let mut out = vec![Vec::new(); rows.len()];
    let Some(syntax) = set.find_syntax_by_name(name) else {
        return out;
    };
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    for row in rows.start.saturating_sub(SYNTECT_CONTEXT)..rows.end {
        let mut line = rope.line(row).to_string();
        if !line.ends_with('\n') {
            line.push('\n');
        }
        let Ok(ops) = state.parse_line(&line, set) else {
            return out;
        };
        let mut cells = vec![None; line.chars().count() - 1];
        let (mut pos, mut char_pos) = (0, 0);
        let mut paint = |stack: &ScopeStack, from: usize, to: usize, char_pos: &mut usize| {
            let n = line[from..to].chars().count();
            let hl = hl_for_stack(stack);
            let end = (*char_pos + n).min(cells.len());
            if *char_pos < end {
                cells[*char_pos..end].fill(hl);
            }
            *char_pos += n;
        };
        for (at, op) in &ops {
            if *at > pos {
                paint(&stack, pos, *at, &mut char_pos);
                pos = *at;
            }
            if stack.apply(op).is_err() {
                return out;
            }
        }
        if row >= rows.start {
            paint(&stack, pos, line.len(), &mut char_pos);
            out[row - rows.start] = runs(&cells);
        }
    }
    out
}

// ---- outline ----------------------------------------------------------------------------

const MAX_DEPTH: usize = 200;

fn text_of(rope: &Rope, node: Node) -> String {
    let r = node.byte_range();
    rope.byte_slice(clamp_byte(rope, r.start)..clamp_byte(rope, r.end))
        .to_string()
}

fn field_text(rope: &Rope, node: Node, field: &str) -> Option<String> {
    node.child_by_field_name(field).map(|n| text_of(rope, n))
}

/// The identifier at the bottom of a C/C++ declarator chain (`*foo(int)` -> `foo`).
fn declarator_name(rope: &Rope, node: Node) -> Option<String> {
    let mut n = node;
    while let Some(d) = n.child_by_field_name("declarator") {
        n = d;
    }
    (n != node).then(|| text_of(rope, n))
}

/// Classify `node`. Returns the symbol (kind, name) and whether its children are searched too.
fn classify(
    lang: &Lang,
    node: Node,
    rope: &Rope,
    in_container: bool,
) -> Option<(SymbolKind, String, bool)> {
    use SymbolKind::*;
    let func = if in_container { Method } else { Function };
    let name = |f: &str| field_text(rope, node, f);
    let (kind, name, container) = match (lang, node.kind()) {
        (Lang::Rust, "function_item" | "function_signature_item") => (func, name("name")?, false),
        (Lang::Rust, "struct_item" | "union_item") => (Struct, name("name")?, false),
        (Lang::Rust, "enum_item") => (Enum, name("name")?, false),
        (Lang::Rust, "type_item") => (TypeAlias, name("name")?, false),
        (Lang::Rust, "trait_item") => (Trait, name("name")?, true),
        (Lang::Rust, "mod_item") => (Module, name("name")?, true),
        (Lang::Rust, "impl_item") => {
            let ty = name("type")?;
            let full = match name("trait") {
                Some(t) => format!("{t} for {ty}"),
                None => ty,
            };
            (Impl, full, true)
        }
        (
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript,
            "function_declaration" | "generator_function_declaration",
        ) => (Function, name("name")?, false),
        (
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript,
            "class_declaration" | "abstract_class_declaration",
        ) => (Class, name("name")?, true),
        (Lang::TypeScript | Lang::Tsx | Lang::JavaScript, "method_definition") => {
            (Method, name("name")?, false)
        }
        (Lang::TypeScript | Lang::Tsx, "interface_declaration") => {
            (Interface, name("name")?, false)
        }
        (Lang::TypeScript | Lang::Tsx, "type_alias_declaration") => {
            (TypeAlias, name("name")?, false)
        }
        (Lang::TypeScript | Lang::Tsx, "enum_declaration") => (Enum, name("name")?, false),
        (Lang::TypeScript | Lang::Tsx, "internal_module") => (Module, name("name")?, true),
        (Lang::Python, "function_definition") => (func, name("name")?, false),
        (Lang::Python, "class_definition") => (Class, name("name")?, true),
        (Lang::Bash, "function_definition") => (Function, field_text(rope, node, "name")?, false),
        (Lang::C | Lang::Cpp, "function_definition") => (func, declarator_name(rope, node)?, false),
        (Lang::C | Lang::Cpp, "struct_specifier" | "union_specifier") => {
            node.child_by_field_name("body")?;
            (Struct, name("name")?, true)
        }
        (Lang::C | Lang::Cpp, "enum_specifier") => {
            node.child_by_field_name("body")?;
            (Enum, name("name")?, false)
        }
        (Lang::Cpp, "class_specifier") => {
            node.child_by_field_name("body")?;
            (Class, name("name")?, true)
        }
        (Lang::Cpp, "namespace_definition") => (Module, name("name")?, true),
        (Lang::Markdown, "atx_heading" | "setext_heading") => {
            let text = text_of(rope, node);
            let line = text.lines().next().unwrap_or("");
            (
                Heading,
                line.trim().trim_matches('#').trim().to_string(),
                false,
            )
        }
        _ => return None,
    };
    (!name.is_empty()).then_some((kind, name, container))
}

fn collect_symbols(
    lang: &Lang,
    node: Node,
    rope: &Rope,
    in_container: bool,
    depth: usize,
    out: &mut Vec<Symbol>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut descend_container = in_container;
    if let Some((kind, name, container)) = classify(lang, node, rope, in_container) {
        out.push(Symbol {
            name,
            kind,
            line: node.start_position().row,
        });
        if !container {
            return;
        }
        // Only impls, traits and classes make the functions inside them methods.
        descend_container = in_container
            || matches!(
                kind,
                SymbolKind::Impl | SymbolKind::Trait | SymbolKind::Class | SymbolKind::Struct
            );
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_symbols(lang, child, rope, descend_container, depth + 1, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Buffer;
    use std::path::Path;

    fn spans_of(lang: Lang, src: &str) -> Vec<Vec<Span>> {
        let rope = Rope::from_str(src);
        let mut s = Syntax::new(lang);
        if let Some(t) = s.parse_full(&rope) {
            s.set_tree(t, 0);
        }
        s.highlight(&rope, 0..rope.len_lines())
    }

    fn text_of(src: &str, line: usize, sp: &Span) -> String {
        src.lines()
            .nth(line)
            .unwrap()
            .chars()
            .skip(sp.start)
            .take(sp.end - sp.start)
            .collect()
    }

    fn find<'a>(src: &str, spans: &'a [Vec<Span>], line: usize, text: &str) -> Option<&'a Span> {
        spans[line].iter().find(|s| text_of(src, line, s) == text)
    }

    #[test]
    fn rust_keywords_and_strings_highlighted() {
        let src = "fn main() { let s = \"hi\"; }";
        let spans = spans_of(Lang::Rust, src);
        assert_eq!(find(src, &spans, 0, "fn").unwrap().hl, Hl::Keyword);
        assert_eq!(find(src, &spans, 0, "\"hi\"").unwrap().hl, Hl::String);
        assert_eq!(find(src, &spans, 0, "main").unwrap().hl, Hl::Function);
    }

    #[test]
    fn tsx_highlights() {
        let src = "const App = () => <div className=\"x\">{1}</div>;\nfunction f(a: number): string { return 'a'; }";
        let spans = spans_of(Lang::Tsx, src);
        assert_eq!(find(src, &spans, 0, "const").unwrap().hl, Hl::Keyword);
        assert_eq!(find(src, &spans, 0, "div").unwrap().hl, Hl::Tag);
        assert_eq!(find(src, &spans, 1, "function").unwrap().hl, Hl::Keyword);
        assert_eq!(find(src, &spans, 1, "number").unwrap().hl, Hl::Type);
        assert_eq!(find(src, &spans, 1, "'a'").unwrap().hl, Hl::String);
    }

    #[test]
    fn highlight_spans_align_with_multibyte() {
        let src = "let a = \"é🦀x\"; // ñ";
        let spans = spans_of(Lang::Rust, src);
        let s = find(src, &spans, 0, "\"é🦀x\"").unwrap();
        assert_eq!((s.start, s.end, s.hl), (8, 13, Hl::String));
        let c = spans[0].iter().find(|s| s.hl == Hl::Comment).unwrap();
        assert_eq!(text_of(src, 0, c), "// ñ");
    }

    #[test]
    fn incremental_edit_then_reparse_matches_full() {
        let mut buf = Buffer::new("fn a() {}\nfn b() {}\nfn c() { \"s\" }\n");
        let mut s = Syntax::new(Lang::Rust);
        assert!(s.set_tree(s.parse_full(buf.rope()).unwrap(), buf.version()));
        // A multibyte, multi-line insert on line 0 shifts every later line.
        let edit = buf.replace(3..4, "été() {}\nfn z");
        s.edit(&edit, buf.version());
        assert!(s.is_stale());
        let rope = buf.rope();
        let fresh = spans_of(Lang::Rust, &rope.to_string());
        let stale = s.highlight(rope, 0..rope.len_lines() + 3);
        assert_eq!(stale.len(), rope.len_lines() + 3);
        // Lines the edit did not touch keep their (shifted) spans from the stale tree.
        assert_eq!(stale[2], fresh[2]);
        assert_eq!(stale[3], fresh[3]);
        assert!(s.set_tree(s.parse_full(rope).unwrap(), buf.version()));
        assert!(!s.is_stale());
        assert_eq!(s.highlight(rope, 0..rope.len_lines()), fresh);
    }

    #[test]
    fn set_tree_ignores_trees_older_than_the_latest_edit() {
        let mut buf = Buffer::new("fn a() {}");
        let mut s = Syntax::new(Lang::Rust);
        assert!(s.set_tree(s.parse_full(buf.rope()).unwrap(), buf.version()));
        let old = s.parse_full(buf.rope()).unwrap();
        let old_version = buf.version();
        let edit = buf.replace(0..0, "// c\n");
        s.edit(&edit, buf.version());
        assert!(!s.set_tree(old, old_version), "older parse must be refused");
        assert!(s.is_stale());
        assert_eq!(s.version(), old_version);
        let rope = buf.rope();
        assert!(s.set_tree(s.parse_full(rope).unwrap(), buf.version()));
        assert!(!s.is_stale());
    }

    #[test]
    fn stale_tree_with_shrunk_text_does_not_panic() {
        let mut s = Syntax::new(Lang::Rust);
        let long = Rope::from_str("fn é() { let s = \"🦀🦀\"; }\nfn b() {}\n");
        s.set_tree(s.parse_full(&long).unwrap(), 0);
        // Text changed without telling the syntax: offsets are out of range or mid-char.
        for text in ["", "é", "🦀", "fn"] {
            let rope = Rope::from_str(text);
            s.highlight(&rope, 0..3);
            s.outline(&rope);
        }
    }

    #[test]
    fn outline_functions_in_modules_are_not_methods() {
        let rope = Rope::from_str("mod tests { fn t() {} }\ntrait T { fn m(); }\n");
        let mut s = Syntax::new(Lang::Rust);
        s.set_tree(s.parse_full(&rope).unwrap(), 0);
        let o: Vec<_> = s
            .outline(&rope)
            .into_iter()
            .map(|x| (x.name, x.kind))
            .collect();
        assert_eq!(
            o,
            vec![
                ("tests".into(), SymbolKind::Module),
                ("t".into(), SymbolKind::Function),
                ("T".into(), SymbolKind::Trait),
                ("m".into(), SymbolKind::Method),
            ]
        );
    }

    #[test]
    fn outline_bash_both_function_forms() {
        let rope = Rope::from_str("foo() {\n  echo a\n}\nfunction bar {\n  echo b\n}\n");
        let mut s = Syntax::new(Lang::Bash);
        s.set_tree(s.parse_full(&rope).unwrap(), 0);
        let o: Vec<_> = s
            .outline(&rope)
            .into_iter()
            .map(|x| (x.name, x.line))
            .collect();
        assert_eq!(o, vec![("foo".into(), 0), ("bar".into(), 3)]);
    }

    #[test]
    fn outline_python_and_typescript() {
        let py = Rope::from_str("class A:\n    def m(self):\n        pass\n\ndef f():\n    pass\n");
        let mut s = Syntax::new(Lang::Python);
        s.set_tree(s.parse_full(&py).unwrap(), 0);
        let o: Vec<_> = s
            .outline(&py)
            .into_iter()
            .map(|x| (x.name, x.kind))
            .collect();
        assert_eq!(
            o,
            vec![
                ("A".into(), SymbolKind::Class),
                ("m".into(), SymbolKind::Method),
                ("f".into(), SymbolKind::Function)
            ]
        );
        let ts = Rope::from_str("class C { m() {} }\nfunction g() {}\ninterface I {}\n");
        let mut s = Syntax::new(Lang::TypeScript);
        s.set_tree(s.parse_full(&ts).unwrap(), 0);
        let o: Vec<_> = s
            .outline(&ts)
            .into_iter()
            .map(|x| (x.name, x.kind))
            .collect();
        assert_eq!(
            o,
            vec![
                ("C".into(), SymbolKind::Class),
                ("m".into(), SymbolKind::Method),
                ("g".into(), SymbolKind::Function),
                ("I".into(), SymbolKind::Interface)
            ]
        );
    }

    #[test]
    fn outline_rust() {
        let src = "struct Foo;\n\nimpl Foo {\n    fn bar(&self) {}\n}\n\nfn top() {}\nmod m {}\n";
        let rope = Rope::from_str(src);
        let mut s = Syntax::new(Lang::Rust);
        s.set_tree(s.parse_full(&rope).unwrap(), 0);
        let o: Vec<(String, SymbolKind, usize)> = s
            .outline(&rope)
            .into_iter()
            .map(|x| (x.name, x.kind, x.line))
            .collect();
        assert_eq!(
            o,
            vec![
                ("Foo".into(), SymbolKind::Struct, 0),
                ("Foo".into(), SymbolKind::Impl, 2),
                ("bar".into(), SymbolKind::Method, 3),
                ("top".into(), SymbolKind::Function, 6),
                ("m".into(), SymbolKind::Module, 7),
            ]
        );
    }

    #[test]
    fn toml_and_json_highlight() {
        let t = "[package]\nname = \"x\"\nn = 5\n";
        let spans = spans_of(Lang::Toml, t);
        assert_eq!(find(t, &spans, 1, "\"x\"").unwrap().hl, Hl::String);
        assert_eq!(find(t, &spans, 2, "5").unwrap().hl, Hl::Number);
        let j = "{\"a\": [1, true, \"b\"]}";
        let spans = spans_of(Lang::Json, j);
        assert_eq!(find(j, &spans, 0, "1").unwrap().hl, Hl::Number);
        assert!(find(j, &spans, 0, "\"b\"").is_some_and(|s| s.hl == Hl::String));
    }

    #[test]
    fn every_tree_sitter_language_has_a_working_query() {
        let samples = [
            (Lang::Rust, "fn f() {}"),
            (Lang::TypeScript, "let x: number = 1;"),
            (Lang::Tsx, "let x = <a/>;"),
            (Lang::JavaScript, "let x = 1;"),
            (Lang::Json, "[1]"),
            (Lang::Toml, "a = 1"),
            (Lang::Yaml, "a: 1"),
            (Lang::Markdown, "# T\n\n```rust\nx\n```\n"),
            (Lang::Css, "a { color: red; }"),
            (Lang::Html, "<p class=\"x\">hi</p>"),
            (Lang::Bash, "echo \"hi\""),
            (Lang::Python, "def f():\n    return 1"),
            (Lang::C, "int main() { return 0; }"),
            (Lang::Cpp, "class A { int x; };"),
        ];
        for (lang, src) in samples {
            let spans = spans_of(lang.clone(), src);
            assert!(
                spans.iter().any(|l| !l.is_empty()),
                "{lang:?} produced no spans"
            );
        }
    }

    #[test]
    fn syntect_highlights_other_languages() {
        let src = "package main\nfunc main() { x := \"hi\" }\n";
        let spans = spans_of(Lang::Other("Go".into()), src);
        assert_eq!(
            find(src, &spans, 1, "\"hi\"").map(|s| s.hl),
            Some(Hl::String)
        );
        assert_eq!(
            find(src, &spans, 1, "func").map(|s| s.hl),
            Some(Hl::Keyword)
        );
    }

    #[test]
    fn plain_for_unknown_binaryish() {
        let lang = Lang::for_path(Path::new("blob.bin"));
        assert_eq!(lang, Lang::Plain);
        let src = "\u{0}\u{1}ELF\u{7f} fn \"x\"";
        let spans = spans_of(lang.clone(), src);
        assert!(spans.iter().all(Vec::is_empty));
        let rope = Rope::from_str(src);
        let s = Syntax::new(lang);
        assert!(s.parse_full(&rope).is_none());
        assert!(s.outline(&rope).is_empty());
    }

    /// Highlighting a window costs about the same in a 100k-line file as in a small one.
    /// Timed against that small-file baseline (each the fastest of several runs, after a
    /// warm-up that compiles the queries), so a loaded machine slows both alike.
    #[test]
    fn highlight_of_a_huge_file_is_proportional_to_the_window() {
        use std::time::{Duration, Instant};
        let file = |lines: usize| {
            let mut src = String::new();
            for i in 0..lines {
                src.push_str(&format!(
                    "fn f{i}(a: u32) -> u32 {{ let s = \"x\"; a + {i} }}\n"
                ));
            }
            Rope::from_str(&src)
        };
        let fastest = |lines: usize| -> (Duration, Vec<Vec<Span>>) {
            let rope = file(lines);
            let mut s = Syntax::new(Lang::Rust);
            s.set_tree(s.parse_full(&rope).unwrap(), 0);
            let at = lines / 2;
            let mut best = Duration::MAX;
            let mut spans = Vec::new();
            for _ in 0..7 {
                let t = Instant::now();
                spans = s.highlight(&rope, at..at + 60);
                best = best.min(t.elapsed());
            }
            (best, spans)
        };
        let _ = fastest(100); // compiles the highlight queries
        let (small, _) = fastest(1_000);
        let (huge, spans) = fastest(100_000);
        println!("TIMING highlight of 60 lines: 1k-line file {small:?}, 100k-line file {huge:?}");
        assert_eq!(spans.len(), 60);
        assert!(spans.iter().all(|l| !l.is_empty()));
        assert!(
            huge < small * 10 + Duration::from_millis(5),
            "highlight took {huge:?} in a 100k-line file, {small:?} in a 1k-line one"
        );
    }
}
