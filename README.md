# herdr-dashr

A [Herdr](https://github.com/herdrdev/herdr) plugin that opens an
**agent-built, live Grafana dashboard inside a Herdr pane**, built and
reshaped by the AI session you are already talking to.

You only talk to your AI session. Paste a CodePipeline link and say
*"visualize it on a dashboard"*: its herdr-dashr skill opens the dashboard
pane beside you and builds the dashboard, then keeps changing it as you ask.

- **Real Grafana, in your browser.** The dashr pane is a narrow column on the
  right with the dashboard's link and its health; Ctrl-click the link to open
  Grafana in Chrome (or any browser). It follows every change the agent makes
  by itself.
- **No model in the data path.** Grafana refreshes the data. The agent only
  changes the dashboard, through the `dashr` tools (a command line, or MCP).
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

**Status:** v0.1.4, on GitHub and npm, for Linux, macOS and Windows. Windows support and the AI-session flow are new; everything else is verified, most by an end-to-end
suite that runs a real Herdr, a real Grafana and a real Chrome in CI.

## Install

Linux, macOS and Windows:

```bash
herdr plugin install czinegeroland/herdr-dashr
```

The build step runs `npm install herdr-dashr@<version>` in the plugin
directory, as herdr-remote-channel does: npm installs the one platform
package whose prebuilt `dashr` matches your machine (verified against the
release's SHA-256 before it was packed), and every Herdr entry point runs it
through `node node_modules/herdr-dashr/bin.js`. You need Node.js 18+ and
Docker; no Rust toolchain. Claude Code and the AWS CLI (CodePipeline,
CloudWatch) are optional — run the **Check dashr
prerequisites** action to see what is missing.

Another build step installs the **herdr-dashr skill** for Claude Code with
`npx skills add czinegeroland/herdr-dashr --skill herdr-dashr --agent
claude-code --global`, as herdr-remote-channel installs its skill. Your AI
session also needs the `dashr` command: `npm install -g herdr-dashr` (the
skill runs it for you when `dashr` is missing).

On Windows, Docker Desktop must be running, and the chat pane's shell is
PowerShell.

### The CLI on its own

```bash
npx herdr-dashr@latest --version
npm install -g herdr-dashr    # installs the `dashr` command
```

One npm package per platform carries the executable; npm installs only the
one that runs on your machine, with no postinstall script and no download
from GitHub.

## Use

Ask your AI session, in Herdr:

- *"Visualize this on a dashboard: https://eu-west-1.console.aws.amazon.com/codesuite/codepipeline/pipelines/api/view"*
- *"Show me error rates and latency for checkout."*
- *"I'm about to run the importer — show me its logs and check that `import done` is logged."*

The skill opens the dashboard pane beside the conversation (`herdr plugin
pane open --plugin herdr-dashr --entrypoint dashboard`), waits for Grafana
(`dashr wait`) and builds with `dashr tool <name>`: the same tools, masking
and privacy rules as the MCP server. You never have to leave the
conversation.

The Herdr actions still work without an AI session of your own; they open a
tab with Grafana on top and a new agent underneath:

| Action | What it does |
|---|---|
| **Open live dashboard** | New tab: Grafana on top, the agent underneath. |
| **Open dashboard for a CodePipeline** | Ctrl-click a CodePipeline console URL. |
| **Open live logs and traces (OpenTelemetry)** | Same tab, with an OTLP endpoint and a live log trail. |
| **Promote dashboard to persistent Grafana** | Copies the focused session's dashboard (needs `[promote]`). |
| **Check dashr prerequisites** | Doctor popup. |

Close the dashboard pane to stop Grafana; the container and runtime files are
deleted. The sidebar token `$dashr` shows panel health
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

- Installed by the plugin's build step with `npx skills add` from this
  repository, as herdr-remote-channel does. It teaches your own AI session
  to open the dashboard pane and drive it with `dashr wait` and `dashr tool`.
- `dashr skill install --dir <skills-dir>` installs the copy embedded in the
  binary, for other agents; a skill of the same name that you wrote is never
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
  capabilities, on loopback only. In
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
