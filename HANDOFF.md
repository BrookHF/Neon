# Handoff — Neon

Read this first, then `DESIGN.md` (the contract), then `AGENTS.md` (rules).

## What Neon is

A minimal coding agent in Rust, one binary, nothing installed to run it. One tool today
(`bash`). Pi for the minimal loop and minimal prompt; DeepSeek Harness / Cordis for
"everything is a plugin". Milestones 1 and 2 are built and verified.

## Environment (this machine)

- Debian 13 aarch64 under **PRoot-Distro on an Android phone**. RAM is tight: about 1.4 GB
  free. A large Rust build can take 5 minutes; run long builds with a background task.
- **No root.** `apt` cannot be used. `su root` asks for a password you do not have.
- Toolchain: rustup as user `fangb`, `~/.cargo/bin/cargo`, rustc 1.99.1. Nothing is on `PATH`,
  so call `~/.cargo/bin/cargo` directly.
- Network: `index.crates.io` works, `crates.io/api` returns 403 (fine, cargo uses the sparse
  index). DNS resolution fails occasionally — retry `cargo` if it reports
  "Could not resolve host".
- No Gemini key yet. `~/.config/neon/config.yaml` is mode 0600 with `api_key: ""`. Everything
  testable runs without a key.

## Current state

```
commits   ed6e474 milestone 1, c68f21b wire notes, 1c82cdf milestone 2
files     src/{lib,main,agent,llm,http,tools,config,prompt,messages,error,ui}.rs (2297 lines)
          fixtures/{demo,limit,steer}.jsonl   tests/loop.rs   scripts/tui-smoke.py
deps      serde, serde_json, rustls (ring provider), webpki-roots, yaml-rust2,
          ratatui 0.30.2, crossterm 0.29.0
built     turn/step loop, bash tool with timeout + spill, Gemini SSE adapter, fixture adapter,
          plain stdio REPL, ratatui TUI, steering, abort
verified  17 cargo tests pass, 9 pty checks pass, all with no key and no network
```

## Commands

```sh
cd ~/src/neon
~/.cargo/bin/cargo build                      # dev build (glibc)
~/.cargo/bin/cargo test                       # 17 tests, offline
python3 scripts/tui-smoke.py                  # drives the TUI on a pty, no key
~/.cargo/bin/cargo run -- --fixture fixtures/demo.jsonl -p "say hello"
~/.cargo/bin/cargo run                        # TUI
~/.cargo/bin/cargo run -- --plain             # plain REPL
```

Release target is still pending: `rustup target add aarch64-unknown-linux-musl`, then
`cargo build --release --target aarch64-unknown-linux-musl`. Do it before claiming the
"runs anywhere" goal is met.

## Architecture you must keep

- Core holds only: loop (`agent.rs`), session log, tool registry (`tools.rs`), LLM seam
  (`llm.rs` trait), thin TUI (`ui.rs`). Everything else becomes a profile row or a plugin.
- Service keys from the design: `ctx.llm`, `ctx.tools`, `ctx.sessions`, `ctx.systemPrompt`,
  `ctx.agentLoop`, `ctx.plugins`. Registrations are reversible effects: a disposer unwinds them.
- Two event dispatch modes only: `emit` (observe) and `waterfall` (around-middleware). The
  tool pipeline already takes waterfall interceptors: `Registry::add_interceptor`.
- Steering: Enter (carriage return) lands after the tool in flight; calls still owed are
  dropped, so no call goes unanswered. Controls reset at the **end** of a turn, not the start.
- Plugin swaps take effect at the turn boundary, never mid-request.

## Milestone 3: WASM plugins — done

- `src/plugin.rs` loads `plugin.wasm` with `wasmi` (pure Rust interpreter). The pinned
  `wasm3` crate cannot build or run here: it needs clang+bindgen, ships x86-only bindings,
  and emits native code that PRoot blocks. Fang chose `wasmi`; the ABI stayed as designed.
- Capability gating: only granted names are wired; a plugin that declares an ungranted
  import is refused at load with the capability named.
- `src/profile.rs`: profile bundle, profile patch, home patch, `--patch` overlays stack
  onto an empty row list; a patch overrides only the fields it names; last layer wins.
- Hot swap: `/plugin list|load|unload|reload` (TUI), `:plugin` (plain). Changes queue and
  apply at the turn boundary; unload runs `neon_dispose` and drops the instance.
- Author crate: sibling repo `~/src/neon-plugin` (Fang's choice); fixture plugin builds to
  `fixtures/plugins/echo/`. `sh ~/src/neon-plugin/scripts/build.sh` rebuilds the wasm.

ABI v1: plugin exports `neon_abi_version/neon_tools/neon_handle/neon_dispose/neon_scratch_base`
plus `memory` and `alloc/free` wrappers. Host imports live in module `neon` named
`neon_alloc`, `neon_free`, and `neon_<capability with dots turned to underscores>`.
Strings cross with a 4-byte little-endian length just before the payload; the bump
allocator lives inside the plugin's own scratch array (`neon_scratch_base()`).

The debug wasmi interpreter needs a deep host stack: `.cargo/config.toml [env]
RUST_MIN_STACK = "8000000"` is committed for tests and UI worker threads.

## Milestone 4: sessions and compaction (next work)

- Flat append-only JSONL at `~/.local/state/neon/sessions/<id>.jsonl`, entries `user`,
  `assistant` (text + calls), `tool_result`, `model_change`, `compaction`, `meta`.
- `neon --resume <id>|last` replays it.
- Compaction: when the token estimate passes `context_budget_tokens` (default 200000), ask the
  model to summarize the oldest span, insert a summary entry, keep originals in the log. The
  loop currently errors at the budget with a message pointing here.

## Things already learned (do not rediscover)

- Gemini v1beta: the prompt field is `systemInstruction`, not `system`; `additionalProperties`
  is rejected in a function declaration schema; schema `type` values must be upper case.
- Gemini can close a stream without a TLS `close_notify`. Treat a bad close as end of body:
  `http::read_all` and `http::each_line` keep whatever arrived.
- crossterm 0.29 has no `terminal::execute`; use the `execute!` macro. Enter arrives as
  carriage return, not newline.
- ratatui diff frames make ANSI-stripped screen captures lose spaces. The smoke script compares
  whitespace-free strings on purpose.
- The UI thread must never take the session lock: the worker owns it for the whole turn. The
  status bar reads `Shared.tokens`.
- Fixture JSON is easy to get wrong by brace counting. Generate fixtures with `json.dumps`, not
  by hand.
- Keep rustls on the ring provider, not the default `aws-lc-rs`, so a static musl build needs
  no cmake.

## Rules

- Never log or echo the Gemini key. `config.yaml` stays mode 0600.
- Add a crate only when it replaces hand-written code you would otherwise own, and say why.
- No `apt`, no sudo. rustup only.
- Keep the plugin ABI stable; a change needs a rebuilt fixture plugin and a bumped
  `neon_abi_version()`.
- Run `cargo test` and `scripts/tui-smoke.py` before claiming a milestone done.

## Project memory (OKF)

Use the okf tools; do not read files under `knowledge/`.
- `decisions/neon-agent-harness` — every locked choice, milestones built, wire facts.
- `architecture/neon-plugin-abi` — the plugin ABI and capability grants.
After `okf_write` with `action=update`, re-run `okf_relate` (an update wipes links), then
`okf_validate` with `strict` and `scorecard` and fix every finding. Never write a `verified:`
line; only a human verifies.

## Open questions for Fang

1. Confirm the exact Gemini model id. `gemini-3.5-flash-lite` is Fang's naming and has not been
   verified against the model list, because there is no key yet.
2. Whether milestone 3 ships the `neon-plugin` author crate in the same repo or a sibling repo.
3. When the static musl release should be built: before milestone 3 or after milestone 4.
