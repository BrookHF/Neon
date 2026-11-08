# AGENTS.md — Neon

Read `DESIGN.md` before changing anything. It is the contract: service keys, plugin ABI,
capability grants, turn/step flow, XDG layout, milestone order.

## Build

```sh
cargo build --release            # dev/host build on glibc
cargo test                       # fixture replay, no key, no network
cargo run                        # interactive TUI
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl   # portable static release
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
- A plugin gets only the host imports its manifest grants. Ungranted names are never wired.
- Plugin swaps take effect at the turn boundary, never mid-request.
- Never log or echo the Gemini key. `config.yaml` stays mode 0600.
- Tests run against the fake `ctx.llm` adapter with recorded fixtures. Real-API tests
  self-skip when no key is present.
