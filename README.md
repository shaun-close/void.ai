# void.ai

A personal, AI-first IDE and editor written in Rust: a docking egui GUI, a
UI-agnostic core, multiple model backends (OpenAI-compatible and Anthropic),
and an agent that works in a per-chat overlay sandbox you review before
anything touches your files.

Status: early and moving fast. Linux only for now (the sandbox uses
overlayfs, bubblewrap and Landlock).

## Layout

- `crates/void-core` — engine, backends, conversations, agent, overlay sandbox
- `crates/void-sandbox` — the Landlock helper the agent's commands run under
- `apps/void-gui` — the egui/egui_dock desktop app
- `docs/superpowers` — design specs and implementation plans

## Building

The workspace is managed with [moon](https://moonrepo.dev):

```sh
moon run void-gui:run      # launch the app
moon run workspace:format workspace:lint workspace:test   # the gate
```

Plain `cargo build` / `cargo test` work too.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT licence ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 licence, shall
be dual licensed as above, without any additional terms or conditions.

The bundled Phosphor icon font is MIT licensed; see
`apps/void-gui/assets/fonts/Phosphor-LICENSE-MIT.txt`.
