# Neon

A minimal coding agent in Rust. One tool (`bash`), Gemini over SSE, plugins as WASM.
See `DESIGN.md` for the contract and `AGENTS.md` for the build rules.

## Run

```sh
cargo build --release
cargo run                    # TUI
cargo run -- --plain         # plain REPL for pipes and dumb terminals
cargo run -- "list this folder"   # one task, then exit (same as -p)
cargo run -- --fixture fixtures/demo.jsonl   # no key, no network
cargo run -- --sessions      # list saved sessions, newest first
cargo run -- --resume        # continue the newest session
cargo test                   # fixture replay, offline
python3 scripts/tui-smoke.py # drives the TUI on a pty, no key
```

## Use it

First run writes a commented starter file at `~/.config/neon/config.yaml` (mode `0600`) and tells
you where it is. Put your Gemini key in `api_key`, never in the environment. With no key yet,
`neon --fixture <file>` still runs everything offline.

TUI keys: Enter sends a task, or steers a running turn. `Ctrl+C` stops a run, or quits when idle.
`Ctrl+D` quits. `PgUp`/`PgDown` scroll the transcript back. `Tab` completes a command. `Up`/`Down`
recall past input. Arrow keys and `Home`/`End` move the cursor.

TUI commands, and the plain REPL takes the same verbs with a colon: `/help`, `/quit`, `/tools`,
`/model <name>`, `/hist`, `/compact`, `/sessions`, `/resume <id>|last`, `/new`, and
`/plugin list|load|unload|reload`. Plugin changes queue and apply at the next turn, so a running
request never sees a changing tool set. A command that touches the history refuses while a turn
runs; `Ctrl+C` stops the turn first.

The status bar shows the model, the folder, a spinner with elapsed seconds while a turn runs, the
token estimate and its share of the budget, the plugin count, the scrollback position, and the last
turn's result. Errors read in red and name the fix, for example `raise max_tokens in config.yaml`.

Plugin rows come from `~/.config/neon/profiles/<profile>.yaml`, with patches stacked on
top: `profiles/<profile>.patch.yaml`, `~/.config/neon/patch.yaml`, then `--patch` files.
A patch targets a row by `id` and overrides only the fields it names. Plugin directories
are `~/.config/neon/plugins/<id>/plugin.yaml` plus `plugin.wasm`.

## What is in the box

- `src/agent.rs` — the turn/step loop, the `Sink` trait, and steering/abort `Controls`
- `src/ui.rs` — the ratatui front end: streaming transcript, tool cards, scrollback, color,
  status bar, and the `/command` layer
- `src/llm.rs` — the LLM seam: `GeminiAdapter` and `FixtureAdapter`
- `src/http.rs` — hand-written HTTP/1.1 over TLS (rustls + webpki-roots)
- `src/tools.rs` — the registry, the waterfall pipeline, and `bash`
- `src/plugin.rs` — WASM plugin loader (wasmi), capability grants, hot swap
- `src/profile.rs` — profile layers and patches for plugin rows
- `src/prompt.rs` — the short system prompt plus `AGENTS.md` discovery
- `src/config.rs` — YAML config from `~/.config/neon/config.yaml`

Milestone 1 is done: loop, one bash tool, streaming. Milestone 2 is done: ratatui TUI,
steering on Enter, Ctrl+C abort, tool cards, status bar. Milestone 3 is done: WASM plugins
(`wasmi`), capability grants, profiles and patches, hot swap at the turn boundary, and
`/plugin list|load|unload|reload`. Plugin authors live in the sibling repo
`~/src/neon-plugin`; fixture `.wasm` files under `fixtures/plugins/` let tests run with no
plugin build. Milestone 4 adds the session log, `--resume`, and compaction. Milestone 5 is the
usability pass: slash commands in both front ends, `/help`, scrollback, color, a spinner while a
turn runs, a first-run config template, `--sessions`, and error text that names the file to edit.

Keys and commands are listed under "Use it" above and in `neon --help`.

## Wire notes

Gemini v1beta takes the prompt as `systemInstruction`, not `system`, and rejects
`additionalProperties` in a function declaration schema. Schema `type` values are sent upper
_CASE. A stream can end without a TLS `close_notify`, so reads treat a bad close as end of
body: `http::read_all` and `http::each_line` keep whatever arrived.
