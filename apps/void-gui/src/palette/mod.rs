//! The Shift+Shift palette (also Ctrl+Shift+P): one fuzzy search over files, open tabs,
//! commands, moon tasks, run configs, environment profiles and the current file's symbols.

use std::borrow::Cow;
use std::path::PathBuf;

use crate::commands::CommandId;
use crate::theme;

pub mod shift;
pub mod sources;
#[cfg(test)]
mod tests;

pub use shift::DoubleShift;

/// An open editor tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabRef {
    pub id: u32,
    pub path: PathBuf,
    /// What the palette shows and matches (workspace-relative when inside it).
    pub label: String,
}

/// Something the palette can find.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// A workspace file, relative to the workspace root.
    File(PathBuf),
    Tab(TabRef),
    Command(CommandId),
    /// A moon target (`project:task`).
    Task(String),
    RunConfig {
        id: String,
        name: String,
    },
    /// An environment profile, by name (never its values).
    Env(String),
    /// A symbol in the current editor tab; `line` is zero-based, as `Syntax::outline` gives it.
    Symbol {
        name: String,
        line: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    File,
    Tab,
    Command,
    Task,
    RunConfig,
    Env,
    Symbol,
}

impl Item {
    pub fn kind(&self) -> Kind {
        match self {
            Item::File(_) => Kind::File,
            Item::Tab(_) => Kind::Tab,
            Item::Command(_) => Kind::Command,
            Item::Task(_) => Kind::Task,
            Item::RunConfig { .. } => Kind::RunConfig,
            Item::Env(_) => Kind::Env,
            Item::Symbol { .. } => Kind::Symbol,
        }
    }

    /// What the query is matched against.
    pub fn text(&self) -> Cow<'_, str> {
        match self {
            Item::File(p) => p.to_string_lossy(),
            Item::Tab(t) => Cow::Borrowed(&t.label),
            Item::Command(c) => Cow::Borrowed(crate::commands::def(*c).label),
            Item::Task(t) | Item::Env(t) => Cow::Borrowed(t),
            Item::RunConfig { name, .. } | Item::Symbol { name, .. } => Cow::Borrowed(name),
        }
    }
}

pub type Score = u32;

/// How many results the palette shows.
pub const LIMIT: usize = 60;

/// The query's first character can narrow the search to one kind of thing: `>` commands,
/// `@` symbols, `!` runs (moon tasks and run configs), `$` environments, `/` files (open tabs
/// included). Returns the prefix and the rest of the query.
pub fn split_prefix(query: &str) -> (Option<char>, &str) {
    let query = query.trim_start();
    match query.chars().next() {
        Some(c @ ('>' | '@' | '!' | '$' | '/')) => (Some(c), query[1..].trim_start()),
        _ => (None, query),
    }
}

fn allowed(prefix: Option<char>, kind: Kind) -> bool {
    match prefix {
        None => true,
        Some('>') => kind == Kind::Command,
        Some('@') => kind == Kind::Symbol,
        Some('!') => matches!(kind, Kind::Task | Kind::RunConfig),
        Some('$') => kind == Kind::Env,
        Some('/') => matches!(kind, Kind::File | Kind::Tab),
        Some(_) => false,
    }
}

/// The best `limit` matches for `query` in `items`, best first (a prefix alone lists its kind
/// in the order given). The palette itself keeps a [`Ranker`], which narrows as you type.
#[cfg(test)]
pub fn rank(query: &str, items: &[Item], limit: usize) -> Vec<(Item, Score)> {
    Ranker::default()
        .rank(query, items, limit)
        .into_iter()
        .map(|(i, s)| (items[i].clone(), s))
        .collect()
}

/// What the last [`Ranker::rank`] call did (tests check the work stays bounded).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RankStats {
    /// Items the matcher looked at.
    pub scored: usize,
    /// Items that matched.
    pub matched: usize,
    /// Items that went through the final sort.
    pub sorted: usize,
}

/// [`rank`] that remembers the last query's matches, so typing one more character only
/// re-scores what still could match. Call [`Ranker::reset`] when the items change.
pub struct Ranker {
    pub stats: RankStats,
    /// The last query (prefix, rest) and how many items it ran over.
    last: Option<(Option<char>, String, usize)>,
    /// Indices of the items the last query matched, in order.
    survivors: Vec<usize>,
    paths: nucleo_matcher::Matcher,
    plain: nucleo_matcher::Matcher,
    buf: Vec<char>,
}

impl Default for Ranker {
    fn default() -> Self {
        use nucleo_matcher::{Config, Matcher};
        Ranker {
            stats: RankStats::default(),
            last: None,
            survivors: Vec::new(),
            paths: Matcher::new(Config::DEFAULT.match_paths()),
            plain: Matcher::new(Config::DEFAULT),
            buf: Vec::new(),
        }
    }
}

/// A query whose matches are a subset of any shorter query it extends. nucleo's operators
/// (`!` not, `^` prefix, `$` suffix, `'` exact, `\\` escapes) break that, so those start over.
fn narrows(query: &str) -> bool {
    !query.contains(['!', '^', '$', '\'', '\\'])
}

impl Ranker {
    pub fn reset(&mut self) {
        self.last = None;
        self.survivors.clear();
    }

    /// Indices into `items` of the best `limit` matches, best first.
    pub fn rank(&mut self, query: &str, items: &[Item], limit: usize) -> Vec<(usize, Score)> {
        use nucleo_matcher::Utf32Str;
        use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};

        self.stats = RankStats::default();
        let (prefix, q) = split_prefix(query);
        let q = q.trim_end();
        if q.is_empty() {
            self.reset();
            return items
                .iter()
                .enumerate()
                .filter(|(_, it)| allowed(prefix, it.kind()))
                .take(limit)
                .map(|(i, _)| (i, 0))
                .collect();
        }
        let extends = self.last.as_ref().is_some_and(|(p, old, n)| {
            *p == prefix
                && *n == items.len()
                && q.starts_with(old.as_str())
                && narrows(old)
                && narrows(q)
        });
        let candidates: Vec<usize> = if extends {
            std::mem::take(&mut self.survivors)
        } else {
            (0..items.len())
                .filter(|&i| allowed(prefix, items[i].kind()))
                .collect()
        };
        let pattern = Pattern::parse(q, CaseMatching::Smart, Normalization::Smart);
        // (score, text length, index): better score, then shorter text, then list order.
        let mut hits: Vec<(Score, usize, usize)> = Vec::new();
        for i in candidates {
            let item = &items[i];
            let text = item.text();
            let matcher = match item.kind() {
                Kind::File | Kind::Tab => &mut self.paths,
                _ => &mut self.plain,
            };
            if let Some(s) = pattern.score(Utf32Str::new(&text, &mut self.buf), matcher) {
                hits.push((s, text.len(), i));
            }
            self.stats.scored += 1;
        }
        self.stats.matched = hits.len();
        self.survivors = hits.iter().map(|h| h.2).collect();
        self.last = Some((prefix, q.to_string(), items.len()));
        let order = |a: &(Score, usize, usize), b: &(Score, usize, usize)| {
            b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2))
        };
        // Only the best `limit` are sorted: picking them is linear in the matches.
        if hits.len() > limit {
            if limit == 0 {
                return Vec::new();
            }
            hits.select_nth_unstable_by(limit - 1, order);
            hits.truncate(limit);
        }
        self.stats.sorted = hits.len();
        hits.sort_unstable_by(order);
        hits.into_iter().map(|(s, _, i)| (i, s)).collect()
    }
}

/// What the user picked: the item, and whether Ctrl+Enter asked for a new tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pick {
    pub item: Item,
    pub new_tab: bool,
}

/// The palette's state between frames.
#[derive(Default)]
pub struct PaletteUi {
    open: bool,
    query: String,
    /// Position in `rows` (as shown, grouped).
    selected: usize,
    /// Give the search field focus (just opened).
    focus: bool,
    /// Bring the selected row into view (the keys moved it).
    scroll: bool,
    cands: sources::Candidates,
    /// The file list version the candidates were built from.
    built: u64,
    ranker: Ranker,
    /// The query `rows` answer.
    ranked: Option<String>,
    /// Candidate indices, grouped by kind (each kind where its best match ranked).
    rows: Vec<usize>,
    pub files: sources::FileIndex,
    pub recent: sources::Recent,
}

impl PaletteUi {
    pub fn is_open(&self) -> bool {
        self.open
    }

    #[cfg(test)]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Open on `cands` with an empty query (recent files and open tabs).
    pub fn open(&mut self, cands: sources::Candidates) {
        self.open = true;
        self.query.clear();
        self.focus = true;
        self.set_candidates(cands);
    }

    pub fn close(&mut self) {
        self.open = false;
        self.cands = Default::default();
        self.rows.clear();
        self.ranked = None;
        self.ranker.reset();
    }

    /// The workspace's files changed since the open palette's list was built.
    pub fn stale(&self) -> bool {
        self.open && self.built != self.files.version()
    }

    /// New lists while open (the file walk landed): the query stays, the ranking starts over.
    pub fn refresh(&mut self, cands: sources::Candidates) {
        self.set_candidates(cands);
    }

    fn set_candidates(&mut self, cands: sources::Candidates) {
        self.cands = cands;
        self.built = self.files.version();
        self.ranker.reset();
        self.ranked = None;
    }

    /// Follow the workspace folder: another one starts with no recent files and no list.
    pub fn set_root(&mut self, root: Option<&std::path::Path>) {
        let before = self.files.version();
        self.files.set_root(root);
        if self.files.version() != before {
            self.recent.clear();
            self.close();
        }
    }

    /// Rank again when the query changed since the last frame.
    fn rerank(&mut self) {
        if self.ranked.as_deref() == Some(self.query.as_str()) {
            return;
        }
        let (prefix, rest) = split_prefix(&self.query);
        let hits: Vec<usize> = if prefix.is_none() && rest.trim().is_empty() {
            (0..self.cands.front).take(LIMIT).collect()
        } else {
            self.ranker
                .rank(&self.query, &self.cands.items, LIMIT)
                .into_iter()
                .map(|(i, _)| i)
                .collect()
        };
        let items = &self.cands.items;
        let mut kinds: Vec<Kind> = Vec::new();
        for &i in &hits {
            if !kinds.contains(&items[i].kind()) {
                kinds.push(items[i].kind());
            }
        }
        self.rows = kinds
            .iter()
            .flat_map(|k| hits.iter().copied().filter(|&i| items[i].kind() == *k))
            .collect();
        self.ranked = Some(self.query.clone());
        self.selected = 0;
        self.scroll = true;
    }

    /// Draw the palette (when open) and handle its keys: Up/Down move, Enter picks, Ctrl+Enter
    /// picks into a new tab, Esc or a click outside closes. Call it before the panels, so the
    /// keys it takes never reach them.
    pub fn show(&mut self, ctx: &egui::Context, pal: &theme::Palette) -> Option<Pick> {
        use egui::{Key, Modifiers};
        if !self.open {
            return None;
        }
        let (esc, up, down, ctrl_enter, enter) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::CTRL, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Enter),
            )
        });
        if esc {
            self.close();
            return None;
        }
        let screen = ctx.content_rect();
        let width = (screen.width() - 32.0).clamp(200.0, 640.0);
        let id = egui::Id::unique("palette");
        let area = egui::Modal::default_area(id.with("area")).anchor(
            egui::Align2::CENTER_TOP,
            egui::vec2(0.0, (screen.height() * 0.12).min(96.0)),
        );
        let frame = egui::Frame::new()
            .fill(pal.bg_raised)
            .stroke(egui::Stroke::new(1.0, pal.border))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::same(10))
            .shadow(ctx.global_style().visuals.popup_shadow);
        let mut pick = None;
        let resp = egui::Modal::new(id)
            .area(area)
            .frame(frame)
            .backdrop_color(egui::Color32::from_black_alpha(if pal.dark { 110 } else { 50 }))
            .show(ctx, |ui| {
                ui.set_width(width);
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .id(id.with("query"))
                        .hint_text("Search everywhere      > commands   @ symbols   ! runs   $ environments   / files")
                        .desired_width(f32::INFINITY)
                        .margin(egui::Margin::symmetric(8, 6)),
                );
                if self.focus || !field.has_focus() {
                    field.request_focus();
                    self.focus = false;
                }
                self.rerank();
                let n = self.rows.len();
                if n > 0 {
                    if down {
                        self.selected = (self.selected + 1) % n;
                        self.scroll = true;
                    }
                    if up {
                        self.selected = (self.selected + n - 1) % n;
                        self.scroll = true;
                    }
                    if enter || ctrl_enter {
                        pick = Some(Pick {
                            item: self.cands.items[self.rows[self.selected.min(n - 1)]].clone(),
                            new_tab: ctrl_enter,
                        });
                    }
                }
                ui.add_space(4.0);
                if n == 0 {
                    let empty = self.query.trim().is_empty();
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(if empty {
                            "Type to search the workspace's files, open tabs, commands, moon tasks, run configs and environments"
                        } else {
                            "Nothing matches"
                        })
                        .color(pal.text_faint),
                    );
                    ui.add_space(4.0);
                    return;
                }
                let recent = self.query.trim().is_empty();
                let max_h = (screen.height() * 0.6).min(460.0);
                // The modal's area lays out in last frame's size: ask for the full height
                // (shrunk to the rows) or a longer list would stay as short as the last one.
                egui::ScrollArea::vertical()
                    .max_height(max_h)
                    .min_scrolled_height(max_h)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let mut group = None;
                        for (pos, &i) in self.rows.iter().enumerate() {
                            let item = &self.cands.items[i];
                            if group != Some(item.kind()) {
                                group = Some(item.kind());
                                heading(ui, pal, group_name(item.kind(), recent));
                            }
                            let selected = pos == self.selected;
                            let r = row(ui, pal, item, &self.cands.details[i], selected);
                            if selected && self.scroll {
                                r.scroll_to_me(None);
                            }
                            if r.clicked() {
                                pick = Some(Pick {
                                    item: item.clone(),
                                    new_tab: ui.input(|i| i.modifiers.ctrl),
                                });
                            }
                        }
                    });
                self.scroll = false;
            });
        if pick.is_some() || resp.should_close() {
            self.close();
        }
        pick
    }
}

fn group_name(kind: Kind, recent: bool) -> &'static str {
    match kind {
        Kind::File if recent => "Recent files",
        Kind::File => "Files",
        Kind::Tab => "Open tabs",
        Kind::Command => "Commands",
        Kind::Task => "moon tasks",
        Kind::RunConfig => "Run configs",
        Kind::Env => "Environments",
        Kind::Symbol => "Symbols",
    }
}

fn heading(ui: &mut egui::Ui, pal: &theme::Palette, text: &str) {
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(text)
            .small()
            .family(egui::FontFamily::Name(theme::BOLD.into()))
            .color(pal.text_faint),
    );
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map_or_else(|| path.to_string_lossy(), |n| n.to_string_lossy())
        .into_owned()
}

/// An item's icon, colour and title.
fn look(pal: &theme::Palette, item: &Item) -> (&'static str, egui::Color32, String) {
    use crate::panels::files::file_icon;
    match item {
        Item::File(p) => {
            let name = file_name(p);
            let (ic, color) = file_icon(pal, &name);
            (ic, color, name)
        }
        Item::Tab(t) => {
            let mut name = file_name(&t.path);
            let (ic, color) = file_icon(pal, &name);
            if t.label.ends_with(" · chat") {
                name.push_str(" · chat");
            }
            (ic, color, name)
        }
        Item::Command(c) => {
            let d = crate::commands::def(*c);
            (d.icon, pal.text_dim, d.label.to_string())
        }
        Item::Task(t) => (theme::icon::TERMINAL, pal.ok, t.clone()),
        Item::RunConfig { name, .. } => (theme::icon::PLAY, pal.ok, name.clone()),
        Item::Env(name) => (theme::icon::GLOBE, pal.accent2, name.clone()),
        Item::Symbol { name, .. } => (theme::icon::BRACKETS_CURLY, pal.accent, name.clone()),
    }
}

/// One result: icon, title, and its detail in faint small text after it.
fn row(
    ui: &mut egui::Ui,
    pal: &theme::Palette,
    item: &Item,
    detail: &str,
    selected: bool,
) -> egui::Response {
    use egui::{Align2, FontFamily, FontId, Sense, TextStyle, pos2, vec2};
    // Sizes from this ui's style, never the process-wide ones (tests run apps side by side).
    let body = TextStyle::Body.resolve(ui.style());
    let small = TextStyle::Small.resolve(ui.style());
    let row_h = (body.size * 1.9).round();
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
    let (ic, color, title) = look(pal, item);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::accesskit::Role::Button, true, &title));
    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);
        if selected {
            painter.rect_filled(
                rect,
                egui::CornerRadius::same(6),
                theme::with_alpha(pal.accent, 48),
            );
        } else if resp.hovered() {
            painter.rect_filled(rect, egui::CornerRadius::same(6), pal.bg_hover);
        }
        let mid = rect.center().y;
        let x = rect.left() + 8.0;
        painter.text(
            pos2(x, mid),
            Align2::LEFT_CENTER,
            ic,
            FontId::new(body.size, FontFamily::Name(theme::ICONS.into())),
            color,
        );
        let at = painter.text(
            pos2(x + body.size * 1.7, mid),
            Align2::LEFT_CENTER,
            &title,
            body,
            pal.text,
        );
        if !detail.is_empty() {
            painter.text(
                pos2(at.right() + small.size * 0.9, mid),
                Align2::LEFT_CENTER,
                detail,
                small,
                pal.text_faint,
            );
        }
    }
    resp
}
