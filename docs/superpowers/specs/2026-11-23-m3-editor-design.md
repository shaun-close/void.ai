# void.ai milestone 3 — editor, Vim, palette, moon runner

Status: approved by the user (2026-11-23). Builds on milestones 1–2.

## Intent

The user wants void.ai to become his **daily IDE, replacing JetBrains** (he uses RustRover, WebStorm, IDEA… with
IdeaVim). Milestone 3 lays the foundation for that: an editor that feels right with Vim keys and stays fast, plus the
two features he named as "first hour" needs that don't depend on a language server:

- the **Shift+Shift palette** (Search Everywhere), and
- **run configurations**, rebuilt around moon (which he always uses) because JetBrains' run configs end up as a long,
  badly organised list. He wants **task chains**, **groups**, and an **environment switcher** (named sets of env vars).

Jump to definition and find usages (his other two) come in milestone 4 with LSP.

## Roadmap (agreed order)

- **M3 (this):** editor core + Vim + file tree + tabs + Shift+Shift palette + moon runner + environment profiles.
- **M4:** LSP — jump to definition and find usages first, then diagnostics, completion, hover, rename, format. Rust
  (rust-analyzer, installed) first, then TypeScript. Workspace symbols join the palette.
- **M5:** project-wide find & replace, terminal panel, git (diff, blame, commit).
- **M6:** AI in the editor (actions on a selection, inline edits).

## Goals

1. **Editor tabs** that are ordinary dock tabs (split, float, tab like every panel), with:
   - a rope buffer (`ropey`) and custom drawing of only the visible lines, so a 100 000-line / 10 MB file opens in
     under ~200 ms and typing and scrolling stay at frame rate;
   - tree-sitter highlighting for the languages the user uses (Rust, TypeScript/TSX, JavaScript, JSON, TOML, YAML,
     Markdown, CSS, HTML, Bash, Python, C/C++), with syntect as the fallback for anything else;
   - line numbers (absolute or relative), current-line highlight, matching brackets, auto-indent (keep indentation,
     indent after an opening bracket), soft-wrap toggle, find/replace in the file, a status line (mode, position,
     language, line endings, encoding);
   - several tabs on one file share a single buffer;
   - save: `:w`/Ctrl+S atomically (keeping permissions, line endings, trailing newline); UTF-8 only (other encodings
     open read-only with a notice);
   - files changed on disk (by another program or by accepting agent changes) reload automatically when the buffer
     has no unsaved edits, otherwise a banner offers Reload / Keep mine.
2. **Vim emulation** ported from the reference editor's engine (`a private reference editor`, its 10
   tests ported too) behind a `VimHost` trait, then extended with **marks** (`m`, `'`, `` ` ``), **visual-block**
   (Ctrl+V), a **jump list** (Ctrl+O / Ctrl+I), and ex commands for tabs (`:e`, `:q`, `:wa`, `:qa`). Macros and
   motions must be solid — they're what the user uses most. The system clipboard backs the `+`/`*` registers. A setting
   turns Vim off (plain editing) for anyone else.
3. **Disk view and chat view per tab.** Each editor tab has a switch: **Disk** (default — the real file) or a chat's
   view (that chat's merged overlay). In a chat view, saving writes into that chat's pending layer, so it appears in
   Review. The tab and status line are clearly marked (accent colour, chat name) whenever a tab isn't on Disk. A file
   with pending changes in any chat shows a banner on its Disk tab: "void has pending changes in ‹chat›" with **Review**
   and **View void's version**.
4. **File tree** panel: the workspace's files (gitignored entries dimmed, `.git` hidden), open, new file/folder,
   rename, delete (to the desktop trash), reveal the active file. These act on the real disk (they're the user's actions).
5. **Shift+Shift palette** (also Ctrl+Shift+P): one fuzzy search box over
   - files in the workspace (gitignore-aware),
   - open tabs,
   - commands (every menu action),
   - moon tasks and saved run configs (Enter runs),
   - environment profiles (Enter switches),
   - symbols in the current file (from tree-sitter; workspace symbols arrive with M4).
   Results are grouped by kind with icons, ranked by a fuzzy matcher (`nucleo-matcher`); prefixes narrow the search
   (`>` commands, `@` symbols, `!` run, `$` env).
6. **Moon runner**:
   - **Discovery**: tasks come from moon itself (`moon query tasks`), grouped by moon project automatically, refreshed
     when `moon.yml`/`.moon/` change.
   - **Run configs**: saved entries in the user's own named **groups** (folders, reorderable). A run config is a **chain**
     of steps run in order; each step is a moon target or a shell command with arguments, and is set to *stop the chain
     on failure* or *continue*. moon's own task dependencies still apply within a step.
   - **Runs**: each run gets a dockable output panel with live output (ANSI colours rendered), the exit status and
     duration; `path:line:col` locations (rustc, tsc, generic) are clickable and open the file there. Stop, Re-run,
     and **Re-run last** (a global key). Several runs can go at once.
   - The user's runs are **not sandboxed** — they're his, like a terminal.
7. **Environment profiles**: named sets of variables per workspace (e.g. dev, staging), one active at a time, shown in
   the status bar and switched from it or the palette. A value is literal, from an environment variable, or the output
   of a command (like API keys), so secrets needn't sit in a file. Profiles apply to runs (and the terminal in M5),
   **never** to the agent's sandbox.

## Non-goals (M3)

- Anything needing a language server (M4): go to definition, usages, diagnostics, completion, hover, rename.
- Project-wide find & replace, terminal, git UI (M5). AI-in-editor (M6).
- Multi-cursor (beyond Vim visual-block), code folding, minimap, debugger.
- Non-UTF-8 editing.

## Anti-goals

- A Vim mode that feels "almost right": motions, counts, operators, dot-repeat and macros must behave like Vim/IdeaVim
  for everything listed; a gap is better than a wrong behaviour.
- Typing or scrolling latency anyone can feel, or opening big files freezing the UI.
- Ever writing a file the user didn't save (no surprise writes); never silently overwriting a file changed on disk.
- Environment profile values (secrets) leaking into the agent sandbox, logs or conversation files.
- Another long flat list of run configs: grouping is the default shape.

## Constraints

- Everything from M1/M2: UI-agnostic core crates, egui + egui_dock GUI, moon workspace, Rust 1.99.1, the void theme.
- moon 2.5.0 is on PATH via proto; the runner must use the moon on the workspace's PATH (proto shims).
- `rust-analyzer` and `clangd` are installed (relevant for M4, not M3).
- Must coexist with the agent: the overlay API from M2 (`Overlay::read/write/resolve/changes`) is how chat views read
  and write.

## Approaches considered (editor)

| Option | Trade-offs | Outcome |
|---|---|---|
| **A. Own editor**: ropey buffer, custom egui drawing of visible lines, tree-sitter highlighting (syntect fallback), Vim engine ported from the reference editor | Full control of Vim feel, speed and agent integration; we build every editing feature ourselves | **Chosen by the user** |
| B. Reuse Helix's internals (buffers, syntax, LSP client) | Saves work (especially LSP); Helix crates are git-only (placeholders on crates.io), unstable API, and a selection-first model that fights Vim | Rejected |
| C. egui's text-box-based code editor widget (`egui_code_editor`) | Quick start; no large-file performance, no Vim, a ceiling for an IDE | Rejected |

Other decisions in conversation:

- Editor views: **both** Disk and chat views with a per-tab switch; **Disk by default** (the user).
- Run configs: moon-powered, **chains**, **groups**, **environment switcher** (the user); stored in void.ai's
  per-workspace data, not the repo (proposal accepted; "save to repo" may come later).
- Order of LSP features in M4: definition and usages first (the user's priorities).

## Architecture

```
crates/
  void-core/       (M1–M2) + runner module (moon discovery, run configs, env profiles, run processes)
  void-editor/     NEW, UI-agnostic: Buffer (ropey), edits + undo/redo (grouped), selections, line endings,
                   tree-sitter syntax (incremental parse, highlight spans per visible range, outline symbols),
                   search, file load/save/watch, Disk vs Overlay backing
  void-vim/        NEW, UI-agnostic: Vim engine over a VimHost trait (port of the reference editor + marks, visual-block, jumps)
apps/void-gui/
  panels/editor.rs        editor tab: drawing, input → Vim/plain, status line, banners, view switch
  panels/files.rs         file tree
  panels/palette.rs       Shift+Shift overlay (not a dock tab: a modal popup)
  panels/runs.rs          run configs + groups + env profiles panel
  panels/run_output.rs    one dock tab per run
```

- **Buffers** are owned by the GUI-side `Workspace` editor state (one per path per view), shared by tabs on the same
  file and view. The engine (void-core) stays the owner of conversations; overlays are opened through `Store`.
- **Vim host**: `void-vim` asks its host for text, selection, edits, undo, scrolling, prompts (ex / search), and
  clipboard — the `VimHost` interface from the reference editor, in Rust. `void-editor`'s buffer implements it; the GUI feeds keys.
- **Highlighting**: tree-sitter grammars as crates (`tree-sitter-rust`, `-typescript`, …) with their `highlights.scm`
  queries; parse incrementally on edit; highlight only visible lines; theme colours from the void palette.
- **Runner**: `moon query tasks` (JSON) for discovery; runs via `tokio::process` in the engine runtime with the active
  env profile merged into the environment; output streamed as events like tool output; stored per workspace:
  `workspaces/<id>/runs.toml` (groups, configs) and `env.toml` (profiles; values as literal / env / cmd).
- **Keys**: see "Ctrl keys belong to Vim" below.

## Done means (M3)

1. Open a 100 000-line Rust file from the tree: appears in < 200 ms, highlighted; scrolling and typing stay smooth.
2. Vim: the reference editor's 10 tests pass on the port, plus tests for marks, visual-block, jumps, macros over several lines,
   counts with operators and text objects, dot-repeat. Editing a real file with Vim keys and `:w` works.
3. A tab switches between Disk and a chat's view; saving in a chat view shows the change in Review; the Disk tab of a
   file with pending changes shows the banner.
4. Shift+Shift finds files, tabs, commands, moon tasks, run configs, env profiles and symbols; Enter does the right
   thing for each.
5. Moon tasks are discovered and grouped; a run config with a 2-step chain and an env profile runs, streams output,
   stops on a failing step when set to, and clicking an error location opens the file at that line.
6. Env profiles switch from the status bar; values from commands are resolved at run time and never written to logs
   or conversations; the agent's sandbox never sees them.
7. Gate: `moon run :format :lint :test :build`, plus kittest UI tests and screenshots.

## Calls made after the builder review (the user can override)

- **Saving in a chat view goes through the engine.** Disk saves always work. While that chat's agent turn is active,
  a chat-view save is refused with a clear message ("void is working in this chat — save to Disk, or wait"), so a
  save never races a mounted overlay or gets undone by Retry. At the start of the chat's next turn the agent gets a
  note, like rejected hunks: "The user edited ‹path› in your pending changes".
- **Big files:** the first frame may be plain text; tree-sitter parses in the background and highlighting appears
  shortly after (target: under ~1 s for 100 000 lines / a few MB). The bar is a 100 000-line Rust file.
- **moon discovery:** `moon query tasks` prints JSON already in moon 2.5.0 (no `--json` flag; checked).

- **Ctrl keys belong to Vim** in the editor when Vim is on (chosen by the user, knowingly departing from his IdeaVim
  setup, which gives all 43 to the IDE): Ctrl+V visual-block, Ctrl+R redo, Ctrl+D/U/F/B/E/Y scrolling, Ctrl+O/I
  jumps, Ctrl+A/X increment/decrement, Ctrl+W window-free (no-op), Ctrl+[ as Esc, Ctrl+C as cancel. App shortcuts
  that don't clash with Vim stay global: Shift+Shift and Ctrl+Shift+P (palette), Ctrl+S (save), Ctrl+Shift+C /
  Ctrl+Shift+V (system clipboard copy / paste, terminal-style), Ctrl+Shift+F10 (re-run last). Recent files live in
  the palette (no Ctrl+E). No per-key Vim/IDE table in M3; the setting that turns Vim off remains.

## Left to the builder

- Exact key bindings beyond those named; drawing details; undo grouping rules; how tree-sitter grammars are bundled
  (crates vs. compiled at build); the `nucleo` vs. similar matcher choice if nucleo proves awkward; output ANSI parser
  crate.
- Whether autosave exists (my default: off; `:w`/Ctrl+S save, with an "unsaved" dot on the tab) — the user can ask for
  JetBrains-style autosave later.
