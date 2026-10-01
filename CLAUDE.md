# Working rules for this repository

- `docs/PRD.md` is the authoritative specification. Every pull request
  updates it: requirement rows it touches, the delivery ledger (section 14)
  and the `Last updated` timestamp. CI (`PRD traceability`) enforces this.
- Run `cargo fmt --all` and `cargo clippy --workspace --all-targets --locked`
  before committing. Run focused tests when behaviour is in doubt; CI runs the
  full suite on Linux, macOS and Windows, plus the end-to-end suite against
  a real Herdr, a real Jaeger and services using the real OpenTelemetry SDK.
- The privacy boundary is part of the product. Anything returned to the
  agent that came from a span (or a source's output) must pass through
  `dashr_core::privacy::Masker`. Never add a command, route or log line that
  bypasses it; raw values go only to the human's viewer (`/v/...` routes).
- `herdr-plugin.toml` is generated: `cargo run -q --bin dashr -- herdr manifest > herdr-plugin.toml`.
