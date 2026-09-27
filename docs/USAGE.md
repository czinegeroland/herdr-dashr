# Using herdr-dashr

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

## Configure

`dashr config example` prints a commented `dashr.toml`; put it in the
directory `herdr plugin config-dir herdr-dashr` prints. Datasources:
Prometheus, Loki, Tempo, CloudWatch, SQL Server, Azure Monitor, Zabbix and Seq
(the last two need `dashr image build`). Secrets are named by environment
variable, never written into the file. Every datasource is personal unless
you say `personal = false`.

## The agent skill

The plugin installs a **herdr-dashr skill** for the coding agents on your
machine (Claude Code, Codex, Cursor and others)
that teaches the agent to build dashboards
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
