# Contributing

```bash
cargo test --workspace
cargo build && mkdir -p bin && cp target/debug/dashr bin/ && herdr plugin link "$PWD"
scripts/e2e/run.sh            # real Herdr + real Grafana; needs Docker
```

Releases: dispatch the **Release** workflow from `main`; it builds the four
platform archives with checksums and creates the `v<version>` tag. **Publish to
npm** then runs automatically: it verifies every archive, builds the packages
(`scripts/build-npm-packages.mjs`) and publishes them. It needs an
`NPM_TOKEN` repository secret (an npm automation token); it can be re-run for
any tag with a dry-run option.

`docs/PRD.md` is the specification; every pull request updates it (CI checks this).
