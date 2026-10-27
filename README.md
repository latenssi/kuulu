# Kuulu

Open-source voice chat for gamers: Mumble's latency with Discord's ease, no
accounts and no central service. Target: ~45 ms mouth-to-ear plus one-way
network delay, measured and reported publicly.

Status: early development. See [docs/PLAN.md](docs/PLAN.md) (in Finnish) for
the full plan and milestones.

## Layout

- `crates/kuulu-proto` – control messages, packet header, protocol versions
- `crates/kuulu-audio` – capture, Opus, jitter buffer, mixing, playback
- `crates/kuulu-client` – client-side QUIC session
- `crates/kuulu-server` – self-hosted server binary
- `crates/kuulu-bench` – latency measurement and network emulator
- `apps/desktop` – Tauri 2 + Svelte desktop app

Running a server: [docs/SERVER.md](docs/SERVER.md). Releases and
auto-update: [docs/RELEASING.md](docs/RELEASING.md).

## Development

With Nix and direnv, `direnv allow` sets up the toolchain. Otherwise install
stable Rust, then:

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
cargo deny check && cargo audit
```

## License

[AGPL-3.0-only](LICENSE)
