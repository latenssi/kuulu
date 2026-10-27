# Kuulu

Open-source, low-latency voice chat for gamers: a native desktop client
(Tauri 2 + Rust) and a self-hosted server. The full plan is in docs/PLAN.md.
Dev environment: Nix flake via direnv (`flake.nix`).
Read it before starting any work.

## Ground rules
- Write all code, comments, commit messages and docs in English.
- Work one milestone at a time (M0-M6 in docs/PLAN.md). Propose a plan
  before writing code. Check off tasks in PLAN.md when they are done.
- Latency is the product. Never add buffering to the audio path without
  measuring it and updating the latency budget in PLAN.md.
- Audio callbacks and the audio thread: no allocation, no locks, no I/O,
  no logging. Pass data between threads only through rtrb ring buffers.
- Stale audio is worse than lost audio: never queue voice datagrams,
  drop old frames instead.
- Ask before changing the wire protocol (kuulu-proto), the latency budget,
  or adding a dependency.
- Never commit secrets or signing keys.

## Commands
- Build: cargo build --workspace
- Test: cargo test --workspace
- Lint: cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
- Desktop app: cd apps/desktop && npm install && npm run tauri dev
- Latency check: cargo run -p kuulu-bench -- latency

## Layout
- crates/kuulu-proto: messages, packet header, protocol versions
- crates/kuulu-audio: capture, Opus, jitter buffer, mixing, playback
- crates/kuulu-client: QUIC session, client side
- crates/kuulu-server: server binary
- crates/kuulu-bench: latency measurement and network emulator
- apps/desktop: Tauri app with Svelte UI
- docs/mockups: UI mockups (voice channel, join, settings)
