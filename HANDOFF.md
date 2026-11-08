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
- Gemini key: filled in by Fang 2026-11-08, only in `~/.config/neon/config.yaml` (mode 0600).
  Never log or echo it. First real-API turn succeeded: `gemini-3.5-flash-lite` is accepted.

## Current state

```
commits   ed6e474 m1, c68f21b wire notes, 1c82cdf m2, ddf4e6b m3, 5242ae8 m4
files     src/{lib,main,agent,llm,http,tools,config,prompt,messages,error,ui,plugin,profile,session}.rs (4081 lines)
          fixtures/{demo,limit,steer,compact}.jsonl, fixtures/plugins/echo
          tests/loop.rs   scripts/tui-smoke.py
deps      serde, serde_json, rustls (ring provider), webpki-roots, yaml-rust2,
          wasmi, ratatui 0.30.2, crossterm 0.29.0
built     turn/step loop, bash tool with timeout + spill, Gemini SSE adapter, fixture adapter,
          plain stdio REPL, ratatui TUI, steering, abort, wasmi plugin loader + grants,
          profiles/patches, hot swap, session log, --resume, compaction
verified  31 cargo tests pass, 13 pty checks pass, all with no key and no network
```

## Commands

```sh
cd ~/src/neon
~/.cargo/bin/cargo build                      # dev build (glibc)
~/.cargo/bin/cargo test                       # 31 tests, offline
python3 scripts/tui-smoke.py                  # drives the TUI on a pty, no key
~/.cargo/bin/cargo run -- --fixture fixtures/demo.jsonl -p "say hello"
~/.cargo/bin/cargo run -- --fixture fixtures/demo.jsonl --resume last -p "more"
~/.cargo/bin/cargo run                        # TUI
~/.cargo/bin/cargo run -- --plain             # plain REPL
```

## Static musl release — done

No musl-gcc exists here and apt cannot be used. ring's assembler needs no libc,
so the host gcc works as the cross compiler:

```sh
~/.cargo/bin/rustup target add aarch64-unknown-linux-musl
CC_aarch64_unknown_linux_musl=$(command -v gcc) \
  ~/.cargo/bin/cargo build --release --target aarch64-unknown-linux-musl
```

Result: a statically linked 5.7 MB aarch64 binary (`file` reports "statically
linked", `ldd` reports "not a dynamic executable"). It runs the fixture turn
and the plain REPL on this phone with nothing installed.

## Architecture you must keep

- Core holds only: loop (`agent.rs`), session log, tool registry (`tools.rs`), LLM seam
  (`llm.rs` trait), thin TUI (`ui.rs`). Everything else becomes a profile row or a plugin.
- Service keys from the design: `ctx.llm`, `ctx.tools`, `ctx.sessions`, `ctx.systemPrompt`,
  `ctx.agentLoop`, `ctx.plugins`. Registrations are reversible effects: a disposer unwinds them.
- Two event dispatch modes only: `emit` (observe) and `waterfall` (around-middleware). The
  tool pipeline already takes waterfall interceptors: `Registry::add_interceptor`.
- Steering: Enter (carriage return) lands after the tool in flight; calls still owed are
  dropped, so no call goes unanswered. Ctrl+C aborts the run and kills the in-flight bash
  child (exit -1, `killed by user`; `ToolCtx.abort` shares the switch with the tool).
  Controls reset at the **end** of a turn, not the start.
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

## Milestone 4: sessions and compaction — done

- `src/session.rs`: append-only JSONL at `~/.local/state/neon/sessions/<id>.jsonl`.
  Entry kinds `meta`, `user`, `assistant` (text + calls), `tool_result`, `model_change`,
  `compaction`. Session id is hex-epoch-secs + pid + nonce.
- `neon --resume <id>|last` replays the log and continues appending to the same file.
  A resumed session keeps its `meta` model unless `--model` overrides it; an override
  is recorded as a `model_change` entry.
- Compaction: when `estimate_tokens` passes `context_budget_tokens`, keep the newest whole
  messages that fit in 60% of the budget, ask `ctx.llm` to summarize the rest, replace the
  span with one summary message, append a `compaction` entry; originals stay in the log.
  Results whose call fell into the summarized span are stripped from the tail's first
  message (they would be orphaned `functionResponse`s on the wire). Still over budget
  after one compaction is an error.
- Replay rebuilds the model-facing history: a `compaction` entry drops everything before
  it and inserts the summary message.
- `estimate_tokens` now counts tool results and call args, not just text.
- Recording split: the CLI/REPL records the typed input message; the loop records every
  message it pushes (`Agent::push`). `Agent.session` is `Option<Arc<Session>>`.
- Fixed a milestone 2 bug: the TUI worker never pushed the typed message into history
  (fixtures hid it because the fake adapter ignores the request). The worker now owns the
  input string and pushes it under the history lock.

## Things already learned (do not rediscover)

- Gemini v1beta: the prompt field is `systemInstruction`, not `system`; `additionalProperties`
  is rejected in a function declaration schema; schema `type` values must be upper case.
- Gemini can close a stream without a TLS `close_notify`. Treat a bad close as end of body:
  `http::read_all` and `http::each_line` keep whatever arrived.
- Gemini thinking models attach `thoughtSignature` to functionCall parts (a sibling of
  `functionCall` on the part). The next request must echo it back as `thought_signature`, or
  the follow-up request is rejected with HTTP 400 "Function call is missing a
  thought_signature". It rides on `ToolCall.thought_sig`, is written to the session log as
  `sig` in call entries, and parts with `"thought": true` are dropped (reasoning summaries
  are not conversation text).
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

1. ~~Confirm the exact Gemini model id~~ — `gemini-3.5-flash-lite` is accepted by the real
   API (first turn ran a bash call on 2026-11-08).
2. Whether milestone 3 ships the `neon-plugin` author crate in the same repo or a sibling repo.
3. ~~When the static musl release should be built~~ — built after milestone 4; the
   outcome is recorded under "Static musl release".
