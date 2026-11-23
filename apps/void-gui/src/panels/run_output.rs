//! Run output tabs: one per run, with a header (name, status, current step, duration, Stop,
//! Re-run) over the run's output. The output is a small terminal model: ANSI colours (parsed
//! with `vte`) mapped onto the void palette, `\r` rewriting the line (progress bars), a bounded
//! line buffer (oldest lines dropped), and compiler locations that open an editor at the line.

use std::collections::VecDeque;
use std::ops::Range;
use std::path::PathBuf;
use std::time::Instant;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontFamily, FontId, RichText, Sense, Ui};
use void_core::engine::Command;
use void_core::runner::{Location, RunId, RunOrigin};

use super::{Action, Panels};
use crate::state::UiState;
use crate::theme::{self, Palette, icon};

/// Lines kept per run; older ones are dropped (and counted).
pub const MAX_LINES: usize = 10_000;
/// Bytes kept of one line; the rest of it is dropped (the line shows it was cut).
pub const MAX_LINE_BYTES: usize = 4 * 1024;
/// Bytes kept per run in all, whatever the line count.
pub const MAX_BYTES: usize = 8 * 1024 * 1024;

/// A colour as the program asked for it; the palette is applied when drawing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    /// The 256-colour table (0–15 are the named colours).
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineKind {
    #[default]
    Output,
    /// "Step 2/3: …" before a step's output.
    Step,
    /// How the run ended.
    Note,
}

#[derive(Clone, Debug, Default)]
pub struct Line {
    pub kind: LineKind,
    /// Plain text (escapes interpreted, tabs expanded).
    pub text: String,
    /// Style runs: each applies from its byte offset to the next one's.
    pub styles: Vec<(usize, Style)>,
    /// The step that printed it (its `base_dirs` resolve the line's locations).
    pub step: usize,
    /// Longer than `MAX_LINE_BYTES`: the rest was dropped.
    pub cut: bool,
    /// Bumped on every change (the location cache is keyed by it).
    rev: u32,
    locations: Option<(u32, Vec<Location>)>,
}

impl Line {
    fn new(kind: LineKind, step: usize) -> Self {
        Line {
            kind,
            step,
            ..Line::default()
        }
    }

    /// The text in style runs.
    pub fn spans(&self) -> impl Iterator<Item = (Range<usize>, Style)> + '_ {
        self.styles
            .iter()
            .enumerate()
            .filter_map(|(i, (from, style))| {
                let to = self.styles.get(i + 1).map_or(self.text.len(), |n| n.0);
                (to > *from).then_some((*from..to, *style))
            })
    }

    /// The file locations in this line (cached until it changes). `bases`: its step's
    /// `base_dirs`. Spans index `text`.
    pub fn locations(&mut self, bases: &[PathBuf]) -> &[Location] {
        if self.kind != LineKind::Output || bases.is_empty() {
            return &[];
        }
        if self
            .locations
            .as_ref()
            .is_none_or(|(rev, _)| *rev != self.rev)
        {
            let found = void_core::runner::parse_locations(&self.text, bases);
            self.locations = Some((self.rev, found));
        }
        self.locations.as_ref().map_or(&[], |(_, l)| l.as_slice())
    }

    fn changed(&mut self) {
        self.rev = self.rev.wrapping_add(1);
    }
}

/// What the program wrote, interpreted: finished lines and the one being written.
#[derive(Default)]
struct Screen {
    lines: VecDeque<Line>,
    dropped: u64,
    /// Bytes of text in `lines`.
    bytes: usize,
    style: Style,
    step: usize,
    /// A `\r` was written: the next text rewrites the line.
    pending_cr: bool,
}

impl Screen {
    /// The line being written (a new one after a header or note).
    fn cur(&mut self) -> &mut Line {
        if self.lines.back().is_none_or(|l| l.kind != LineKind::Output) {
            self.lines.push_back(Line::new(LineKind::Output, self.step));
        }
        self.lines.back_mut().expect("just ensured")
    }

    fn clear_line(&mut self) {
        let l = self.cur();
        let n = l.text.len();
        l.text.clear();
        l.styles.clear();
        l.cut = false;
        l.changed();
        self.bytes -= n;
    }

    /// Text is about to be written: a pending `\r` empties the line first.
    fn begin_write(&mut self) {
        if std::mem::take(&mut self.pending_cr) {
            self.clear_line();
        }
    }

    fn write(&mut self, c: char) {
        self.begin_write();
        let style = self.style;
        let l = self.cur();
        if l.text.len() + c.len_utf8() > MAX_LINE_BYTES {
            l.cut = true;
            return;
        }
        if l.styles.last().is_none_or(|(_, s)| *s != style) {
            l.styles.push((l.text.len(), style));
        }
        l.text.push(c);
        l.changed();
        self.bytes += c.len_utf8();
        if self.bytes > MAX_BYTES {
            self.trim();
        }
    }

    fn newline(&mut self) {
        self.pending_cr = false;
        self.cur();
        self.lines.push_back(Line::new(LineKind::Output, self.step));
        self.trim();
    }

    fn backspace(&mut self) {
        let l = self.cur();
        if let Some(c) = l.text.pop() {
            while l.styles.last().is_some_and(|(at, _)| *at >= l.text.len()) {
                l.styles.pop();
            }
            l.changed();
            self.bytes -= c.len_utf8();
        }
    }

    /// A line of void's own; the program's next output starts a new line.
    fn push_own(&mut self, kind: LineKind, text: String) {
        self.pending_cr = false;
        if self
            .lines
            .back()
            .is_some_and(|l| l.kind == LineKind::Output && l.text.is_empty())
        {
            self.lines.pop_back();
        }
        let mut text = text;
        if text.len() > MAX_LINE_BYTES {
            let mut cut = MAX_LINE_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
        }
        self.bytes += text.len();
        let mut l = Line::new(kind, self.step);
        l.styles.push((0, Style::default()));
        l.text = text;
        self.lines.push_back(l);
        self.trim();
    }

    /// Drop the oldest lines until within the caps (the line being written stays).
    fn trim(&mut self) {
        while self.lines.len() > MAX_LINES || (self.bytes > MAX_BYTES && self.lines.len() > 1) {
            let Some(l) = self.lines.pop_front() else {
                break;
            };
            self.bytes -= l.text.len();
            self.dropped += 1;
        }
    }

    /// SGR (`ESC [ … m`): colours and attributes.
    fn sgr(&mut self, params: &vte::Params) {
        let ps: Vec<&[u16]> = params.iter().collect();
        if ps.is_empty() {
            self.style = Style::default();
            return;
        }
        let mut i = 0;
        while i < ps.len() {
            let p = ps[i];
            let st = &mut self.style;
            match p[0] {
                0 => *st = Style::default(),
                1 => st.bold = true,
                2 => st.dim = true,
                3 => st.italic = true,
                4 => st.underline = true,
                7 => st.inverse = true,
                21 | 22 => (st.bold, st.dim) = (false, false),
                23 => st.italic = false,
                24 => st.underline = false,
                27 => st.inverse = false,
                n @ 30..=37 => st.fg = Color::Indexed((n - 30) as u8),
                n @ 40..=47 => st.bg = Color::Indexed((n - 40) as u8),
                n @ 90..=97 => st.fg = Color::Indexed((n - 90 + 8) as u8),
                n @ 100..=107 => st.bg = Color::Indexed((n - 100 + 8) as u8),
                39 => st.fg = Color::Default,
                49 => st.bg = Color::Default,
                n @ (38 | 48) => {
                    // `38;5;n` / `38;2;r;g;b`, or the colon forms `38:5:n` / `38:2::r:g:b`.
                    let (color, used) = if p.len() > 1 {
                        (extended(&p[1..]), 0)
                    } else {
                        let rest: Vec<u16> = ps[i + 1..].iter().map(|q| q[0]).collect();
                        let used = match rest.first() {
                            Some(5) => 2,
                            Some(2) => 4,
                            _ => rest.len(),
                        };
                        let used = used.min(rest.len());
                        (extended(&rest[..used]), used)
                    };
                    if let Some(c) = color {
                        if n == 38 {
                            st.fg = c;
                        } else {
                            st.bg = c;
                        }
                    }
                    i += used;
                }
                _ => {}
            }
            i += 1;
        }
    }
}

/// The colour in `5;n` or `2;r;g;b` (colon form: `2;;r;g;b` with a colour-space id).
fn extended(p: &[u16]) -> Option<Color> {
    let byte = |v: &u16| u8::try_from(*v).ok();
    match p {
        [5, n, ..] => byte(n).map(Color::Indexed),
        [2, _, r, g, b] => Some(Color::Rgb(byte(r)?, byte(g)?, byte(b)?)),
        [2, r, g, b, ..] => Some(Color::Rgb(byte(r)?, byte(g)?, byte(b)?)),
        _ => None,
    }
}

impl vte::Perform for Screen {
    fn print(&mut self, c: char) {
        self.write(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.pending_cr = true,
            b'\t' => {
                self.begin_write();
                let col = self.cur().text.chars().count();
                for _ in 0..8 - col % 8 {
                    self.write(' ');
                }
            }
            0x08 => self.backspace(),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], _ignore: bool, c: char) {
        if !intermediates.is_empty() {
            return;
        }
        match c {
            'm' => self.sgr(params),
            // Erase in line. The cursor is at the end of the line, or at its start after a
            // `\r`: either way "to the end" after `\r`, and "to the start" or "all", clear it.
            'K' => {
                let mode = params.iter().next().map_or(0, |p| p[0]);
                if mode != 0 || self.pending_cr {
                    self.pending_cr = false;
                    self.clear_line();
                }
            }
            _ => {}
        }
    }
}

/// One run's output.
#[derive(Default)]
pub struct Term {
    parser: vte::Parser,
    screen: Screen,
}

impl Term {
    pub fn push(&mut self, text: &str) {
        self.parser.advance(&mut self.screen, text.as_bytes());
    }

    /// Output from now on belongs to `step`; a header line names it.
    pub fn start_step(&mut self, step: usize, header: String) {
        self.screen.step = step;
        self.screen.push_own(LineKind::Step, header);
    }

    /// A line of void's own (how the run ended).
    pub fn note(&mut self, text: String) {
        self.screen.push_own(LineKind::Note, text);
    }

    pub fn lines(&self) -> &VecDeque<Line> {
        &self.screen.lines
    }

    pub fn lines_mut(&mut self) -> &mut VecDeque<Line> {
        &mut self.screen.lines
    }

    /// Lines dropped from the front to stay within the caps.
    pub fn dropped(&self) -> u64 {
        self.screen.dropped
    }

    /// Everything kept, as plain text.
    pub fn plain_text(&self) -> String {
        let mut out = String::with_capacity(self.screen.bytes + self.screen.lines.len());
        for (i, l) in self.screen.lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&l.text);
        }
        out
    }
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    pub ok: bool,
    pub failed_step: Option<usize>,
    pub exit_code: Option<i32>,
    pub stopped: bool,
    pub error: Option<String>,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Default)]
pub struct StepInfo {
    pub label: String,
    pub base_dirs: Vec<PathBuf>,
}

/// A run as its output tab shows it.
pub struct RunView {
    pub name: String,
    /// What started it (Re-run sends the same).
    pub origin: RunOrigin,
    pub steps: usize,
    pub current: Option<usize>,
    pub step_info: Vec<StepInfo>,
    pub started: Instant,
    pub finished: Option<Finished>,
    pub term: Term,
}

impl RunView {
    pub fn new(name: String, steps: usize, origin: RunOrigin) -> Self {
        Self {
            name,
            origin,
            steps,
            current: None,
            step_info: vec![StepInfo::default(); steps],
            started: Instant::now(),
            finished: None,
            term: Term::default(),
        }
    }

    /// Step `index` started.
    pub fn step(&mut self, index: usize, info: StepInfo) {
        let header = format!(
            "Step {}/{}: {}",
            index + 1,
            self.steps.max(index + 1),
            info.label
        );
        self.term.start_step(index, header);
        if self.step_info.len() <= index {
            self.step_info.resize(index + 1, StepInfo::default());
        }
        self.step_info[index] = info;
        self.current = Some(index);
    }

    pub fn finish(&mut self, f: Finished) {
        let took = fmt_duration(f.duration_ms);
        let note = match self.status_of(&f) {
            RunStatus::Ok => format!("Finished in {took}"),
            RunStatus::Stopped => format!("Stopped after {took}"),
            _ => format!("{} · {took}", failure(&f)),
        };
        self.term.note(note);
        self.finished = Some(f);
    }

    fn status_of(&self, f: &Finished) -> RunStatus {
        if f.ok {
            RunStatus::Ok
        } else if f.stopped {
            RunStatus::Stopped
        } else {
            RunStatus::Failed
        }
    }

    pub fn status(&self) -> RunStatus {
        self.finished
            .as_ref()
            .map_or(RunStatus::Running, |f| self.status_of(f))
    }

    /// One line for the header: what it's doing, or how it ended.
    pub fn status_text(&self) -> String {
        match &self.finished {
            None => match self.current {
                Some(i) => format!("Running step {}/{}", i + 1, self.steps.max(i + 1)),
                None => "Starting…".into(),
            },
            Some(f) if f.ok => "Finished".into(),
            Some(f) if f.stopped => "Stopped".into(),
            Some(f) => failure(f),
        }
    }

    pub fn duration_ms(&self) -> u64 {
        self.finished
            .as_ref()
            .map_or(self.started.elapsed().as_millis() as u64, |f| f.duration_ms)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Ok,
    Failed,
    Stopped,
}

fn failure(f: &Finished) -> String {
    if let Some(e) = &f.error {
        return format!("Couldn't start: {e}");
    }
    let step = f
        .failed_step
        .map_or_else(|| "A step".to_string(), |i| format!("Step {}", i + 1));
    match f.exit_code {
        Some(code) => format!("{step} failed (exit code {code})"),
        None => format!("{step} was killed"),
    }
}

pub fn fmt_duration(ms: u64) -> String {
    let secs = ms as f64 / 1000.0;
    if secs < 60.0 {
        format!("{secs:.1} s")
    } else {
        let s = ms / 1000;
        format!("{} min {:02} s", s / 60, s % 60)
    }
}

impl std::fmt::Debug for RunView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunView")
            .field("name", &self.name)
            .field("current", &self.current)
            .field("finished", &self.finished)
            .field("lines", &self.term.lines().len())
            .finish_non_exhaustive()
    }
}

/// The command that runs exactly what this run ran again: its config (by id) or its task.
/// Err says why it can't (its config was deleted since).
pub fn rerun_command(state: &UiState, run: RunId) -> Result<Command, &'static str> {
    let view = state.run_views.get(&run).ok_or("This run is gone")?;
    match &view.origin {
        RunOrigin::Config { id } => {
            if super::runs::find_config(&state.runs, id).is_none() {
                return Err("Its run config has been deleted");
            }
            Ok(Command::RunConfig {
                config_id: id.clone(),
            })
        }
        RunOrigin::Task { target, args } => Ok(Command::RunTask {
            target: target.clone(),
            args: args.clone(),
        }),
    }
}

fn status_look(pal: &Palette, status: RunStatus) -> (&'static str, Color32) {
    match status {
        RunStatus::Running => (icon::PLAY, pal.accent2),
        RunStatus::Ok => (icon::CHECK_CIRCLE, pal.ok),
        RunStatus::Failed => (icon::X_CIRCLE, pal.err),
        RunStatus::Stopped => (icon::STOP, pal.warn),
    }
}

/// The tab title: a status icon in its colour and the run's name.
pub fn title(state: &UiState, run: RunId) -> egui::WidgetText {
    use egui::text::{LayoutJob, TextFormat};
    let pal = theme::palette(state.config.theme);
    let (name, status) = state
        .run_views
        .get(&run)
        .map_or(("run", RunStatus::Stopped), |v| {
            (v.name.as_str(), v.status())
        });
    let (glyph, color) = status_look(&pal, status);
    let size = theme::body_size();
    let mut job = LayoutJob::default();
    job.append(
        glyph,
        0.0,
        TextFormat {
            font_id: FontId::new(size, FontFamily::Name(theme::ICONS.into())),
            color,
            valign: egui::Align::Center,
            ..Default::default()
        },
    );
    job.append(
        name,
        size * 0.45,
        TextFormat {
            font_id: FontId::proportional(size),
            color: Color32::PLACEHOLDER,
            valign: egui::Align::Center,
            ..Default::default()
        },
    );
    job.into()
}

pub fn show(ui: &mut Ui, p: &mut Panels, run: RunId) {
    let pal = theme::palette_of(ui);
    let rerun = rerun_command(p.state, run);
    let Some(view) = p.state.run_views.get_mut(&run) else {
        ui.label(RichText::new("This run's output is gone.").color(pal.text_faint));
        return;
    };
    header(ui, &pal, p.engine, run, view, rerun);
    ui.add_space(2.0);
    ui.separator();
    output(ui, &pal, run, view, p.actions);
}

fn header(
    ui: &mut Ui,
    pal: &Palette,
    engine: &void_core::engine::EngineHandle,
    run: RunId,
    view: &RunView,
    rerun: Result<Command, &'static str>,
) {
    let status = view.status();
    let (glyph, color) = status_look(pal, status);
    ui.horizontal_wrapped(|ui| {
        ui.label(theme::ico(glyph).color(color));
        ui.label(
            RichText::new(&view.name)
                .family(FontFamily::Name(theme::BOLD.into()))
                .color(pal.text),
        );
        ui.label(RichText::new(view.status_text()).color(color).small());
        if status == RunStatus::Running
            && let Some(step) = view.current.and_then(|i| view.step_info.get(i))
        {
            ui.label(
                RichText::new(&step.label)
                    .monospace()
                    .small()
                    .color(pal.text_dim),
            );
        }
        ui.label(theme::ico(icon::TIMER).color(pal.text_faint))
            .on_hover_text("Duration");
        ui.label(
            RichText::new(fmt_duration(view.duration_ms()))
                .small()
                .color(pal.text_dim),
        );
        if status == RunStatus::Running {
            if ui.button(theme::itext(icon::STOP, "Stop")).clicked() {
                engine.send(Command::StopRun(run));
            }
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(250));
        } else {
            let r = ui.add_enabled(
                rerun.is_ok(),
                egui::Button::new(theme::itext(icon::ARROW_CLOCKWISE, "Re-run")),
            );
            match rerun {
                Ok(cmd) => {
                    if r.on_hover_text("Run it again").clicked() {
                        engine.send(cmd);
                    }
                }
                Err(why) => {
                    r.on_disabled_hover_text(why);
                }
            }
        }
        if ui
            .button(theme::itext(icon::COPY, "Copy"))
            .on_hover_text("Copy the output")
            .clicked()
        {
            ui.ctx().copy_text(view.term.plain_text());
        }
    });
}

fn output(ui: &mut Ui, pal: &Palette, run: RunId, view: &mut RunView, actions: &mut Vec<Action>) {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    let RunView {
        term, step_info, ..
    } = view;
    let dropped = term.dropped();
    let marker = usize::from(dropped > 0);
    let rows = term.lines().len() + marker;
    let mut open = None;
    // Rows sit flush (show_rows reads the spacing of the ui it's called on).
    ui.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::both()
        .id_salt(("run-output", run.0))
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show_rows(ui, row_h, rows, |ui, range| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for i in range {
                if i < marker {
                    ui.label(
                        RichText::new(format!("… {dropped} earlier lines dropped"))
                            .font(font.clone())
                            .color(pal.text_faint),
                    );
                    continue;
                }
                let line = &mut term.lines_mut()[i - marker];
                let bases = step_info
                    .get(line.step)
                    .map_or(&[][..], |s| s.base_dirs.as_slice());
                if let Some(at) = row(ui, pal, &font, row_h, line, bases) {
                    open = Some(at);
                }
            }
        });
    if let Some((path, line)) = open {
        actions.push(Action::OpenAt { path, line });
    }
}

/// One line of output; its locations are links. Returns the location clicked.
fn row(
    ui: &mut Ui,
    pal: &Palette,
    font: &FontId,
    row_h: f32,
    line: &mut Line,
    bases: &[PathBuf],
) -> Option<(PathBuf, usize)> {
    let locations = line.locations(bases).to_vec();
    let line = &*line;
    let mut clicked = None;
    ui.horizontal(|ui| {
        ui.set_min_height(row_h);
        let mut pos = 0;
        for loc in &locations {
            if loc.span.start > pos {
                ui.add(egui::Label::new(job(pal, font, line, pos..loc.span.start, false)).extend());
            }
            let r = ui
                .add(
                    egui::Label::new(job(pal, font, line, loc.span.clone(), true))
                        .extend()
                        .selectable(false)
                        .sense(Sense::click()),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(format!("Open {} at line {}", loc.path.display(), loc.line));
            if r.clicked() {
                clicked = Some((loc.path.clone(), loc.line));
            }
            pos = loc.span.end;
        }
        if pos < line.text.len() || locations.is_empty() {
            ui.add(egui::Label::new(job(pal, font, line, pos..line.text.len(), false)).extend());
        }
        if line.cut {
            ui.label(
                RichText::new(" …(line cut)")
                    .font(font.clone())
                    .color(pal.text_faint),
            );
        }
    });
    clicked
}

/// `range` of `line` in its colours; `link`: underlined, as a location.
fn job(pal: &Palette, font: &FontId, line: &Line, range: Range<usize>, link: bool) -> LayoutJob {
    let mut job = LayoutJob::default();
    let base = match line.kind {
        LineKind::Output => None,
        LineKind::Step => Some(pal.accent),
        LineKind::Note => Some(pal.text_dim),
    };
    for (r, style) in line.spans() {
        let (from, to) = (r.start.max(range.start), r.end.min(range.end));
        if from >= to {
            continue;
        }
        let (mut fg, mut bg) = (
            base.unwrap_or_else(|| ansi_color(pal, style.fg, pal.text)),
            ansi_color(pal, style.bg, Color32::TRANSPARENT),
        );
        if style.inverse {
            (fg, bg) = (
                if bg == Color32::TRANSPARENT {
                    pal.bg_panel
                } else {
                    bg
                },
                fg,
            );
        }
        if style.bold && !pal.dark {
            fg = theme::lerp(fg, Color32::BLACK, 0.2);
        } else if style.bold {
            fg = theme::lerp(fg, Color32::WHITE, 0.2);
        }
        if style.dim {
            fg = theme::lerp(fg, pal.bg_panel, 0.4);
        }
        if link {
            fg = theme::lerp(fg, pal.accent2, 0.5);
        }
        let underline = if style.underline || link {
            egui::Stroke::new(1.0, fg)
        } else {
            egui::Stroke::NONE
        };
        job.append(
            &line.text[from..to],
            0.0,
            TextFormat {
                font_id: font.clone(),
                color: fg,
                background: bg,
                italics: style.italic,
                underline,
                ..Default::default()
            },
        );
    }
    if job.text.is_empty() {
        // An empty line still takes a row.
        job.append(
            "",
            0.0,
            TextFormat {
                font_id: font.clone(),
                ..Default::default()
            },
        );
    }
    job
}

/// A program's colour on the void palette (the 16 named colours follow the palette; the
/// 256-colour cube and truecolor are used as given).
pub fn ansi_color(pal: &Palette, c: Color, default: Color32) -> Color32 {
    let named = |n: u8| match n {
        0 => {
            if pal.dark {
                pal.text_faint
            } else {
                pal.text
            }
        }
        1 => pal.err,
        2 => pal.ok,
        3 => pal.warn,
        4 => theme::lerp(pal.accent, pal.accent2, 0.45),
        5 => pal.accent3,
        6 => pal.accent2,
        _ => pal.text_dim,
    };
    match c {
        Color::Default => default,
        Color::Indexed(n @ 0..=7) => named(n),
        Color::Indexed(8) => pal.text_faint,
        Color::Indexed(15) => pal.text,
        Color::Indexed(n @ 9..=14) => {
            let toward = if pal.dark {
                Color32::WHITE
            } else {
                Color32::BLACK
            };
            theme::lerp(named(n - 8), toward, 0.25)
        }
        Color::Indexed(n @ 16..=231) => {
            let i = n - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            Color32::from_rgb(level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        Color::Indexed(n) => {
            let v = 8 + 10 * (n - 232);
            Color32::from_rgb(v, v, v)
        }
        Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(l: &Line) -> Vec<(&str, Style)> {
        l.spans().map(|(r, s)| (&l.text[r], s)).collect()
    }

    fn texts(t: &Term) -> Vec<&str> {
        t.lines().iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn ansi_spans_and_cr_handling() {
        let mut t = Term::default();
        t.push("plain \x1b[1;31merror\x1b[0m: x\n");
        let red_bold = Style {
            fg: Color::Indexed(1),
            bold: true,
            ..Style::default()
        };
        assert_eq!(
            spans(&t.lines()[0]),
            [
                ("plain ", Style::default()),
                ("error", red_bold),
                (": x", Style::default())
            ]
        );
        // A progress bar rewrites its line; CRLF is a plain line end.
        t.push("10%\r20%\r");
        t.push("30%\n");
        t.push("a\r\nb");
        // Output can stop mid-line; the next chunk continues it.
        t.push("c\n");
        assert_eq!(texts(&t)[1..4], ["30%", "a", "bc"]);
        // An escape split across chunks, 256 colours and truecolor, then a reset.
        t.push("\x1b[3");
        t.push("2mg\x1b[38;5;208mo\x1b[38;2;1;2;3mr\x1b[mn\x1b[92mB\x1b[39m\n");
        let fg = |c| Style {
            fg: c,
            ..Style::default()
        };
        assert_eq!(
            spans(&t.lines()[4]),
            [
                ("g", fg(Color::Indexed(2))),
                ("o", fg(Color::Indexed(208))),
                ("r", fg(Color::Rgb(1, 2, 3))),
                ("n", Style::default()),
                ("B", fg(Color::Indexed(10))),
            ]
        );
        // "\r\x1b[K" (erase the line) before rewriting it, and tabs as spaces.
        t.push("old\r\x1b[Knew\ta\n");
        assert_eq!(t.lines()[5].text, "new     a");
        // The line being written is there too (empty after a newline).
        assert_eq!(t.lines().len(), 7);
        assert_eq!(t.lines()[6].text, "");
        // A truecolor followed by more attributes in the same sequence.
        t.push("\x1b[38;2;1;2;3;1mq\x1b[0m\n");
        assert_eq!(
            spans(&t.lines()[6]),
            [(
                "q",
                Style {
                    fg: Color::Rgb(1, 2, 3),
                    bold: true,
                    ..Style::default()
                }
            )]
        );
    }

    #[test]
    fn output_is_capped_and_long_lines_cut() {
        let mut t = Term::default();
        for i in 0..MAX_LINES + 50 {
            t.push(&format!("line {i}\n"));
        }
        assert!(t.lines().len() <= MAX_LINES);
        assert_eq!(
            t.dropped() as usize + t.lines().len(),
            MAX_LINES + 51,
            "every line kept or counted"
        );
        assert_eq!(
            t.lines()[t.lines().len() - 2].text,
            format!("line {}", MAX_LINES + 49)
        );
        // One enormous line (a minified bundle, say) is kept only in part.
        t.push(&"x".repeat(MAX_LINE_BYTES * 20));
        t.push("\n");
        let long = &t.lines()[t.lines().len() - 2];
        assert_eq!(long.text.len(), MAX_LINE_BYTES);
        assert!(long.cut);
        // A byte budget holds even with few lines.
        let mut t = Term::default();
        let chunk = format!("{}\n", "y".repeat(MAX_LINE_BYTES - 1));
        for _ in 0..(MAX_BYTES / MAX_LINE_BYTES) * 2 {
            t.push(&chunk);
        }
        let kept: usize = t.lines().iter().map(|l| l.text.len()).sum();
        assert!(kept <= MAX_BYTES, "{kept}");
        assert!(t.dropped() > 0);
    }

    #[test]
    fn steps_start_on_a_fresh_line() {
        let mut t = Term::default();
        t.start_step(0, "Step 1/2: a".into());
        t.push("half a line");
        t.start_step(1, "Step 2/2: b".into());
        t.push("next\n");
        let kinds: Vec<_> = t
            .lines()
            .iter()
            .map(|l| (l.kind, l.step, l.text.as_str()))
            .collect();
        assert_eq!(
            kinds[..4],
            [
                (LineKind::Step, 0, "Step 1/2: a"),
                (LineKind::Output, 0, "half a line"),
                (LineKind::Step, 1, "Step 2/2: b"),
                (LineKind::Output, 1, "next"),
            ]
        );
    }
}
