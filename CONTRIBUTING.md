# Contributing

- Rust, stable toolchain. `cargo fmt`, zero clippy warnings, `cargo test` green
  before you push — CI enforces all three.
- TigerStyle: bound every buffer, no panics on hot paths, explicit errors,
  deterministic outputs. No TODO survives a merge.
- Politeness is the product: any change to request behavior must respect the
  global rate ceiling and backoff design, and must state its request-cost.
- Keep a Changelog: every user-visible change gets a CHANGELOG entry.
