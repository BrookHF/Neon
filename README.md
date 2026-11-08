# Neon

A minimal coding agent in Rust. One tool (`bash`), Gemini over SSE, plugins as WASM.
See `DESIGN.md` for the contract and `AGENTS.md` for the build rules.

## Run

```sh
cargo build --release
cargo run                    # TUI
cargo run -- --plain         # plain REPL for pipes and dumb terminals
cargo run -- -p "list this folder"   # print mode
cargo run -- --fixture fixtures/demo.jsonl   # no key, no network
cargo test                   # fixture replay, offline
python3 scripts/tui-smoke.py # drives the TUI on a pty, no key
```

Config lives at `~/.config/neon/config.yaml`. Put your Gemini key in `api_key`, keep the
file mode `0600`. `cargo run` reads it. `neon --help` lists the flags.

Plugin rows come from `~/.config/neon/profiles/<profile>.yaml`, with patches stacked on
top: `profiles/<profile>.patch.yaml`, `~/.config/neon/patch.yaml`, then `--patch` files.
A patch targets a row by `id` and overrides only the fields it names. Plugin directories
are `~/.config/neon/plugins/<id>/plugin.yaml` plus `plugin.wasm`.

## What is in the box

- `src/agent.rs` — the turn/step loop, the `Sink` trait, and steering/abort `Controls`
- `src/ui.rs` — the ratatui front end, streaming transcript, tool cards, status bar
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
plugin build. Milestone 4 adds the session log, `--resume`, and compaction.

TUI keys: Enter sends a task, or steers a running turn. Ctrl+C aborts a run, or quits when
idle. Ctrl+D quits when idle. `/plugin` commands queue and apply at the next turn, so a
running request never sees a changing tool set. In plain mode the commands are `:plugin ...`.

## Wire notes

Gemini v1beta takes the prompt as `systemInstruction`, not `system`, and rejects
`additionalProperties` in a function declaration schema. Schema `type` values are sent upper
_CASE. A stream can end without a TLS `close_notify`, so reads treat a bad close as end of
body: `http::read_all` and `http::each_line` keep whatever arrived.
