# Neon — a minimal coding agent in Rust

Design locked by interview with Fang, 2026-11-08. Lineage: Pi for the minimal loop and
minimal prompt, DeepSeek Harness (dsh) / Cordis for "everything is a plugin".

## Goals

1. One binary, no runtime needed: nothing must be installed to run it on Linux
   (no Node, no Python, no OpenSSL). Build-time crates are fine.
2. Minimal and functional, like Pi: short system prompt, one tool at first (`bash`).
3. Extensible like dsh: plugins are the whole extension surface, and they swap in flight.

## Shape

Core is thin and patchable. Everything else is a row a profile can override.

| Service key | Owns | Replaceable by |
|---|---|---|
| `ctx.llm` | Provider adapter seam, stream vocabulary | config row or plugin |
| `ctx.tools` | Tool registry + waterfall execution pipeline | plugin |
| `ctx.sessions` | Append-only JSONL session log | plugin |
| `ctx.systemPrompt` | Prompt-section assembly | plugin, `AGENTS.md` |
| `ctx.agentLoop` | Turn/step driver | plugin |
| `ctx.plugins` | WASM loader, capability grants, hot swap | core only |

Cordis ideas we copy, simplified: services claim a stable key, dependencies are declared
not imported, registrations are reversible effects (a disposer unwinds them), typed events
carry the seams. Two dispatch modes only: `emit` (observe) and `waterfall` (around-middleware).

## Process and build

- Repo `~/src/neon`, binary `neon`, MIT license, own `AGENTS.md`.
- Toolchain: rustup, rustc 1.99.1, installed under `~/.cargo` as `fangb` (apt needs root,
  unavailable here).
- Dev build: glibc dynamic on this box (Debian 13 aarch64 under PRoot).
- Release: static musl (`rustup target add aarch64-unknown-linux-musl`, plus x86_64).
  TLS is rustls + webpki-roots, so a static binary needs no system libraries at all.
- Crates (build-time only): `serde`, `serde_json`, `yaml-rust2`, `rustls`, `webpki-roots`,
  `wasm3`, `ratatui`, `crossterm`. HTTP/1.1 and SSE are hand-written on `TcpStream`.

## Layout (XDG)

```
~/.config/neon/config.yaml            # model, limits, key (mode 0600)
~/.config/neon/profiles/default.yaml  # ordered plugin rows
~/.config/neon/profiles/*.patch.yaml  # overrides by row id, last layer wins
~/.config/neon/plugins/<id>/plugin.yaml + plugin.wasm
~/.local/state/neon/sessions/<id>.jsonl
~/.cache/neon/spill/<call-id>.out     # full raw tool output
```

CLI: `neon` interactive, `neon -p "task"` print mode, `--resume <id>|last`,
`--model <name>`, `--profile <name>`, `--patch <file>`.

## Agent loop

A turn is zero or more steps. A step is one model request plus the tools it calls.

```
turn/start
  assemble prompt sections + step-admitted tool schemas
  stream request -> assistant text + function calls
  for each call: tools/pre-execute (waterfall) -> execute -> tools/post-execute -> result
  tool results or steering input owed -> next step
turn/end
```

- Steering: Enter while running injects a message after the current tool and drops the
  remaining calls of that assistant turn. Ctrl+C aborts the run and returns queued input.
- Prompt: built-in section of about 30 lines, plus `AGENTS.md` from the cwd and parent dirs.
  No plan mode, no sub-agents, no permission popups, no built-in to-dos.

## Gemini adapter

Native wire format, not the OpenAI-compatible one.

- `POST https://generativelanguage.googleapis.com/v1beta/models/<model>:streamGenerateContent?alt=sse`
- Header `x-goog-api-key`; body: `system` instruction, `contents` (role/content parts,
  `functionCall` and `functionResponse`), `tools` with `function_declarations`,
  optional `generationConfig.thinkingConfig`.
- Default model: `gemini-3.5-flash-lite`. Key comes from `~/.config/neon/config.yaml` only,
  never from the environment.
- Adapter is a trait: `fn stream(&[Message], &[ToolSchema], &ModelOpts) -> impl Stream<Item = Event>`.
  A second provider becomes a config row or plugin later.

## bash tool (the only tool in v0)

- Args: `command`, optional `cwd`, optional `timeout_secs` (default 120).
- Runs `/bin/bash -lc` in the session cwd, captures stdout and stderr separately.
- Model sees an excerpt: head plus tail with a marker naming the dropped byte count, plus
  the exit code and the spill path.
- Raw full output is always written to `~/.cache/neon/spill/<call-id>.out`.
- No gate. Tools run with the permissions of the `neon` process; isolate by container or trust.

## Plugins

- A plugin is a `plugin.wasm` plus a `plugin.yaml` manifest: `id`, `tools`, `capabilities`.
- Runtime: the `wasm3` interpreter inside the binary. No JIT, so it works under PRoot on
  Android, and the build stays small (RAM on this phone is tight).
- ABI, host-owned strings, four exports:
  - `neon_abi_version() -> i32`
  - `neon_tools() -> ptr` to a JSON array of `{name, description, parameters}`
  - `neon_handle(tool_index, args_ptr, args_len) -> ptr` to `{ok, content, error}`
  - `neon_dispose()`
  Host imports for memory: `alloc(len) -> ptr`, `free(ptr)`.
- Host services, gated per manifest grant: `fs.read`, `fs.write`, `bash.run`, `net.fetch`,
  `clock.now`, `log`. A plugin gets only the imports it was granted; an ungranted name is not
  wired at all, so the plugin cannot call it.
- Authoring: a small `neon-plugin` crate authors build with
  `cargo build --target wasm32-unknown-unknown`. It wraps the four exports and the import stubs.
- Hot swap at the turn boundary: `/plugin list|load|unload|reload`. Unload runs the plugin's
  disposers, so its tools and prompt sections unwind. A running request never sees a changing
  tool set. Optional directory poll (1 s) reloads changed files at the next turn boundary.
- Profiles and patches stack ordered layers onto an empty row list: profile bundles, profile
  patch, home patch, `--patch` overlays. A patch targets a row by `id` and replaces it, or
  inserts new rows, or sets `disabled`.

## Sessions and context

- Flat append-only JSONL: `user`, `assistant` (text + calls), `tool_result`, `model_change`,
  `compaction`, `meta`. `--resume` replays it.
- Context: estimated tokens over the budget (`context_budget_tokens`, default 200000) triggers
  model compaction: summarize the oldest span, insert a summary entry, keep the originals in the log.
- Spill and truncation keep tool payloads small; a call never returns more than the excerpt budget.

## TUI

`ratatui` + `crossterm`: streaming text area, tool cards (name, args, status, excerpt),
status bar (model, token estimate, loaded plugins), input line. Plain stdio mode stays as a
fallback for pipes and phones with a dumb terminal (`neon --plain`).

## Testing

- A fake `ctx.llm` adapter replays recorded JSONL fixtures, so loop tests run with no key and
  no network: `neon --profile test --fixture <file>`.
- Unit tests for the JSON codec, SSE parser, truncation and spill, capability gating, and a
  tiny fixture plugin built by the plugin crate.
- Real-API tests exist but self-skip when no key is present.

## Milestones

1. Loop + `bash` + Gemini streaming, plain stdio. Fixture replay test passes.
2. `ratatui` TUI, steering, abort, tool cards.
3. WASM plugin loader, capability grants, profiles and patches, hot swap, `/plugin` commands.
4. Session log, `--resume`, compaction.
