# herdr-dashr

A [Herdr](https://github.com/herdrdev/herdr) plugin that opens an
**agent-built, live Grafana dashboard inside a Herdr pane**, with a coding
agent underneath to reshape it.

- **Real Grafana, in the terminal.** The top pane shows Grafana in kiosk mode
  through [terminal-browser](https://github.com/zenbu-labs/terminal-browser);
  without it, a text status view.
- **No model in the data path.** Grafana refreshes the data. The agent only
  changes the dashboard, through an MCP server.
- **Privacy by construction.** The agent designs from schemas and masked
  samples (`<email#1>`, `<ipv4#2>`); only you see real values.
- **Disposable.** A pane-owned Grafana container with a read-only root and
  tmpfs storage on a loopback port, removed when the pane closes.
- **CodePipeline bootstrap.** Ctrl-click a CodePipeline URL: the plugin finds
  the stacks it deployed and builds a first dashboard of their log groups,
  queues and DLQs, state machines and Lambdas before the agent says a word.
- **Saved dashboards.** Name a dashboard you like and reopen it in any later
  pane.
- **OpenTelemetry and live log checks.** One pane-owned container can also
  receive OTLP logs, traces and metrics. Tell the agent which log messages
  should fire (and which must not): the dashboard shows a tile per message
  that turns green when it is logged, and a live log trail with the matches
  highlighted.

`docs/PRD.md` is the authoritative specification and delivery ledger.

**Status:** v0.1.0, the first public release. All but one PRD requirement verified (the first npm publish comes with this release), most by an end-to-end
suite that runs a real Herdr, a real Grafana and terminal-browser in CI.

## Install

```bash
herdr plugin install czinegeroland/herdr-dashr
```

The build step installs the prebuilt `dashr` binary for your platform — from
npm (`herdr-dashr`), whose platform packages were verified against the
release's SHA-256 before they were packed; failing that from the GitHub
release, verified the same way; and only as a last resort with `cargo`. No
Rust toolchain is needed. `DASHR_INSTALL_SOURCE=npm|github|source` forces one. You need
Docker. terminal-browser (for the real Grafana UI; it refuses to run as root), Claude Code (the chat
pane) and the AWS CLI (CodePipeline, CloudWatch) are optional — run the
**Check dashr prerequisites** action to see what is missing.

### The CLI on its own

```bash
npx herdr-dashr@latest --version
npm install -g herdr-dashr    # installs the `dashr` command
```

One npm package per platform carries the executable; npm installs only the
one that runs on your machine, with no postinstall script and no download
from GitHub.

## Use

| Action | What it does |
|---|---|
| **Open live dashboard** | New tab: Grafana on top, the agent underneath. |
| **Open dashboard for a CodePipeline** | Ctrl-click a CodePipeline console URL. |
| **Open live logs and traces (OpenTelemetry)** | Same tab, with an OTLP endpoint and a live log trail. |
| **Promote dashboard to persistent Grafana** | Copies the focused session's dashboard (needs `[promote]`). |
| **Check dashr prerequisites** | Doctor popup. |

Close the dashboard pane to stop Grafana; the container, runtime files and
browser profile are deleted. The sidebar token `$dashr` shows panel health
(`6 ok · 1 err · 1 alert`).

### Saved dashboards

Like a dashboard? Tell the agent *"save this as checkout debug"*. It is kept
on this machine (panels, queries and layout — never data). In any later pane,
*"load checkout debug"* brings it back; the agent is told which dashboards are
saved when it starts. From the command line:

```bash
dashr dashboards save "checkout debug"      # from the running session
dashr dashboards list
dashr dashboards load "checkout debug"      # into the running session
dashr session start --load "checkout debug" # a new session showing it
```

A dashboard that uses a datasource the session lacks (say, the pipeline's
CloudWatch in an OpenTelemetry pane) is refused, naming what is missing.

### Live logs and log checks

The OpenTelemetry action (or `[otel] enabled = true`) runs
`grafana/otel-lgtm` instead of plain Grafana: still one container per pane,
with Loki, Tempo and Prometheus behind an OTLP endpoint on loopback. What you
send is queryable within about five seconds (traces in about two). Programs
started from the chat pane find it in `OTEL_EXPORTER_OTLP_ENDPOINT`; anything
else can be wrapped:

```bash
dashr tail -- cargo run --bin checkout     # output still on your terminal, exit code kept
./app 2>&1 | dashr tail --service app
```

Then ask the agent: *"check that `order 42 created` and `payment captured` are
logged and no exception is"*. It arms them (`expect_logs`), the dashboard gains
a tile per message and a highlighted live trail, the pane notifies you as each
arrives and turns blocked on a forbidden one, and the agent reads back a
counts-only verdict. Without the agent:

```bash
dashr expect -p 'order created = order \d+ created' -p 'payment captured' -a 'exception'
dashr expect --check    # exit status 0 when every expectation holds
dashr expect --clear
```

The agent gets these MCP tools: `list_datasources`, `probe_query`,
`apply_dashboard`, `panel_status`, `panel_data_sample`, `get_dashboard`,
`watch_panel`, `list_watches`, `remove_watch`, `screenshot` (non-personal
dashboards only), `open_for_pipeline`, `promote`.

## The agent skill

The plugin installs a **herdr-dashr skill** for Claude Code
(`~/.claude/skills/herdr-dashr/`) that teaches the agent to build dashboards
dynamically: turn the human's question into panels, probe the data, apply,
verify with `panel_status`, and keep evolving the dashboard as the
conversation goes. It carries dashboard-JSON rules, query models for every
supported datasource (Prometheus, Loki, Tempo, CloudWatch, SQL Server, Azure
Monitor, Zabbix, Seq, TestData) and debugging recipes (deploys, queue
backlogs, Step Functions failures, error spikes, latency).

- Installed by the plugin's build step, refreshed by the dashboard pane when
  the binary is newer. A skill of the same name that you wrote is never
  replaced (`--force` to override).
- Also served as MCP resources (`dashr://guide/...`), so any MCP agent gets it.
- `dashr skill files`, `dashr skill print reference/recipes.md`,
  `dashr skill install --dir <skills-dir>`, `dashr skill uninstall`.
- Configure with `agent.install_skill` and `agent.skill_dirs` in `dashr.toml`.

## Configure

`dashr config example` prints a commented `dashr.toml`; put it in the
directory `herdr plugin config-dir herdr-dashr` prints. Datasources:
Prometheus, Loki, Tempo, CloudWatch, SQL Server, Azure Monitor, Zabbix and Seq
(the last two need `dashr image build`). Secrets are named by environment
variable, never written into the file. Every datasource is personal unless
you say `personal = false`.

## Privacy

- Every value an MCP tool returns passes through the masker: personal
  field names are redacted; emails, IBANs, cards, IPs, phones, JWTs, AWS keys
  and credentials inside other fields are replaced with stable pseudonyms.
- `panel_status` reports rows and errors, never values. Watches are evaluated
  by the pane; the agent never sees what it alerts on.
- Grafana runs with a read-only root, tmpfs storage, no logs, no swap and no
  capabilities, on loopback only. The browser profile lives in memory. In
  OpenTelemetry mode, logs, traces and metrics are kept on disk in anonymous
  Docker volumes that are deleted with the container.

## Develop

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

## License

Apache-2.0
