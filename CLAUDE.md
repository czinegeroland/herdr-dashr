# Working rules for this repository

- `docs/PRD.md` is the authoritative specification. Every pull request
  updates it: requirement rows it touches, the delivery ledger (section 14)
  and the `Last updated` timestamp. CI (`PRD traceability`) enforces this.
- Run `cargo fmt --all` and `cargo clippy --workspace --all-targets --locked`
  before committing. Run focused tests when behaviour is in doubt; CI runs the
  full suite on Linux and macOS, plus the end-to-end suite against a real
  Herdr and a real Grafana.
- The privacy boundary is the product. Anything returned to the agent that
  came from a datasource must pass through `dashr_core::masking::Masker`.
  Never add a tool or log line that bypasses it.
- `herdr-plugin.toml` is generated: `cargo run -q --bin dashr -- herdr manifest > herdr-plugin.toml`.
