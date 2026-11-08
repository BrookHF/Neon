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

## What is in the box

- `src/agent.rs` — the turn/step loop, the `Sink` trait, and steering/abort `Controls`
- `src/ui.rs` — the ratatui front end, streaming transcript, tool cards, status bar
- `src/llm.rs` — the LLM seam: `GeminiAdapter` and `FixtureAdapter`
- `src/http.rs` — hand-written HTTP/1.1 over TLS (rustls + webpki-roots)
- `src/tools.rs` — the registry, the waterfall pipeline, and `bash`
- `src/prompt.rs` — the short system prompt plus `AGENTS.md` discovery
- `src/config.rs` — YAML config from `~/.config/neon/config.yaml`

Milestone 1 is done: loop, one bash tool, streaming. Milestone 2 is done: ratatui TUI,
steering on Enter, Ctrl+C abort, tool cards, status bar. Milestone 3 adds WASM plugins,
profiles, and hot swap. Milestone 4 adds the session log, `--resume`, and compaction.

TUI keys: Enter sends a task, or steers a running turn. Ctrl+C aborts a run, or quits when
idle. Ctrl+D quits when idle.

## Wire notes

Gemini v1beta takes the prompt as `systemInstruction`, not `system`, and rejects
`additionalProperties` in a function declaration schema. Schema `type` values are sent upper
_CASE. A stream can end without a TLS `close_notify`, so reads treat a bad close as end of
body: `http::read_all` and `http::each_line` keep whatever arrived.
