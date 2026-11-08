# AGENTS.md — Neon

Read `DESIGN.md` before changing anything. It is the contract: service keys, plugin ABI,
capability grants, turn/step flow, XDG layout, milestone order.

## Build

```sh
cargo build --release            # dev/host build on glibc
cargo test                       # fixture replay, no key, no network
python3 scripts/tui-smoke.py     # drive the TUI on a pty, no key
cargo run                        # interactive TUI
sh ~/src/neon-plugin/scripts/build.sh  # rebuild fixtures/plugins wasm (wasm32 target)
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl   # portable static release
# no musl-gcc here (no apt); ring's asm needs no libc, so host gcc works as cross CC:
# CC_aarch64_unknown_linux_musl=$(command -v gcc) cargo build --release --target aarch64-unknown-linux-musl
```

No root is available in this environment. Never use `apt`; rustup lives in `~/.cargo`.

## Rules

- The runtime needs nothing installed: no OpenSSL, no Node, no dynamic plugin loading of
  system libraries. TLS is rustls plus webpki-roots.
- Add a crate only when it replaces hand-written code we would otherwise own, and say why.
- Core holds only: loop, session log, tool registry, LLM seam, thin TUI. Anything else is a
  profile row or a plugin.
- Keep the plugin ABI stable. Changing it needs a rebuilt fixture plugin and a bumped
  `neon_abi_version()`.
- Plugin runtime is `wasmi` (pure Rust interpreter). Do not switch back to the `wasm3`
  crate: it needs clang+bindgen at build and emits native code, which PRoot blocks.
- A plugin gets only the host imports its manifest grants. Ungranted names are never wired.
- Plugin swaps take effect at the turn boundary, never mid-request.
- The UI thread never takes the session lock. The worker owns it for the whole turn, and the
  status bar reads a cached token count from `Shared.tokens`.
- Enter arrives as carriage return. Steering lands after the tool that was in flight, and the
  calls still owed are dropped so no call goes unanswered.
- Never log or echo the Gemini key. `config.yaml` stays mode 0600.
- Tests run against the fake `ctx.llm` adapter with recorded fixtures. Real-API tests
  self-skip when no key is present.
