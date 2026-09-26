# herdr-dashr — design handoff

A herdr plugin that opens an agent-built, live Grafana dashboard inside a herdr pane, for debugging deployed work (e.g. an ephemeral environment deployed from a CodePipeline run).

## Origin / use case

- Today: deploy a branch to an ephemeral AWS environment, give Claude Code the CodePipeline link inside herdr, ask it to open a pane and debug.
- Typical tasks: live log tailing, SQS queue trailing, Step Functions execution trailing.
- Goal: one delegation opens a pane with a dashboard **built for the problem at hand**, plus a chat (Claude Code) underneath to reshape it.

## Research findings (Sept 2026)

- herdr: Rust, Apache-2.0, ~40k stars. **Does not accept unsolicited PRs** → plugins are the only contribution path.
- Plugin API = the whole herdr CLI/socket API. Manifest (`herdr-plugin.toml`) declares actions, event hooks (`[[events]]`), `[[startup]]` hooks, panes (overlay/popup/split/tab/zoomed), link handlers (regex on Ctrl-clicked URLs). Env: `HERDR_BIN_PATH`, `HERDR_PLUGIN_STATE_DIR`, `HERDR_PLUGIN_CONTEXT_JSON`, etc.
- Useful APIs: `events.subscribe`, `pane report-agent` (mark pane blocked → notifications), `pane report-metadata` (sidebar tokens), `agent prompt`, `pane.graphics.*`.
- Plugin API gaps (don't depend on them): plugin items in right-click menus (#1722, #906, #3183), plugin-owned sidebar sections (#1609), triggering built-in actions over socket (#1624), theme passed to plugin panes (#1796).
- Marketplace: ~1,300 plugins; **no AWS/observability dashboard plugin exists**. Only herdr-aws-ssm, an S3 clipboard tool, grafterm-style tools elsewhere.
- `zenbu-labs/terminal-browser` (3.4k★): real Chromium rendered in the terminal via kitty graphics; ships a herdr plugin and an agent-browser-compatible CLI (`terminal-browser action`). Lets the pane show the **real Grafana UI**.

## Architecture

```
┌─ top pane: terminal-browser → Grafana kiosk URL ─────┐
│  http://127.0.0.1:<port>/d/<uid>?kiosk&refresh=5s    │
├─ bottom pane: Claude Code + MCP servers ─────────────┤
│  mcp-grafana (datasource discovery, queries)         │
│  plugin MCP server (dashboard + masking tools)       │
└──────────────────────────────────────────────────────┘
```

1. Plugin action (or Ctrl-click on a CodePipeline URL via link handler) opens a new tab with the two panes.
2. Pane process starts a **pane-owned, in-memory Grafana** in Docker.
3. Agent discovers data (datasources, metrics, labels, log streams, schemas) and writes **standard Grafana dashboard JSON**, pushed via Grafana HTTP API.
4. Grafana refreshes data itself. **No LLM in the data path.** The agent's loop only changes the dashboard (add drill-downs, fix queries, react to anomalies).
5. "Promote" command pushes a useful dashboard to a real, persistent Grafana.

### Data sources (via Grafana)

Prometheus, Loki, SQL Server, CloudWatch, Azure Monitor: built in. OpenTelemetry: Tempo (traces) + Prometheus/Mimir + Loki. Zabbix: community plugin (alexanderzobnin). Seq: no official datasource known — Infinity plugin against Seq HTTP API (verify).
Streaming gaps: CloudWatch Live Tail and Loki tail are not Grafana-native streams → may need small native sources later.

### CodePipeline bootstrap (AWS case)

`GetPipelineState` → deploy stage → CloudFormation stack → stack resources (log groups, queues + DLQs, state machines, Lambdas) → agent proposes a first dashboard.

## Grafana container: pane-owned, stores nothing

```bash
docker run --rm -d \
  --name herdr-grafana-<pane-id> --label herdr.pane=<pane-id> \
  -p 127.0.0.1::3000 \
  --read-only --tmpfs /var/lib/grafana:uid=472 --tmpfs /tmp \
  --log-driver none \
  -v <runtime-dir>/provisioning:/etc/grafana/provisioning:ro \
  -e GF_AUTH_ANONYMOUS_ENABLED=true -e GF_AUTH_ANONYMOUS_ORG_ROLE=Admin \
  -e GF_AUTH_DISABLE_LOGIN_FORM=true \
  -e GF_ANALYTICS_REPORTING_ENABLED=false -e GF_ANALYTICS_CHECK_FOR_UPDATES=false \
  herdr-grafana:<pinned>
```

- Custom image with plugins (Zabbix, Infinity) baked in.
- Credentials via `$__env{}` references / short-lived STS creds; never written to files.
- Consider `--memory-swap` = `--memory` to avoid tmpfs swapping to disk.

Lifecycle cleanup, three layers:
1. Pane process stops the container on normal exit.
2. `[[events]] on = "pane.closed"` hook stops containers with that pane label.
3. `[[startup]]` hook reaps containers whose `herdr.pane` label no longer matches `herdr pane list`.

## Privacy (data may contain personal/user data)

- The **agent designs from schemas and masked samples only**; only the human sees real values.
- Masking runs in the plugin process between Grafana and the agent: per-field rules (name, regex for email/phone/IBAN, per-datasource allow-lists).
- Agent verification uses panel status (row counts, query errors), **not screenshots** of real data. Allow screenshots only for datasources flagged as non-personal.
- terminal-browser/Chromium profile: use a throwaway profile in tmpfs, delete on close (verify incognito support).
- Check whether herdr persists pane content on disk; note socket has no caller auth (#514).

## MCP tools (plugin server)

- `apply_dashboard(json)` — schema-validate, push to Grafana, reload browser pane.
- `panel_status()` — per panel: rows, errors, empty; no values.
- `panel_data_sample(panel)` — masked sample rows with real field names/types.
- `open_for_pipeline(url)` — CodePipeline bootstrap.
- `promote(dashboard)` — export to persistent Grafana.

## Risks to test first

- **Spike 0:** terminal-browser in a herdr pane on my terminal, loading any Grafana dashboard. Known herdr issues: #3018 (WezTerm abort with kitty-graphics browser pane), #3941 (iTerm2), #3697/#3676 (missing/flickering images). Fallback: Ctrl-click link to a normal browser.
- macOS: terminal-browser's input helper may need accessibility permissions (corporate machine policy).
- Grafana image + Chromium per pane: memory footprint.

## Build order

1. Spike 0 (rendering).
2. Pane-owned Grafana lifecycle + provisioning (Prometheus, Loki, CloudWatch).
3. Plugin MCP server: `apply_dashboard`, `panel_status`, `panel_data_sample` + masking.
4. Two-pane layout wiring (terminal-browser top, Claude Code + mcp-grafana bottom).
5. CodePipeline bootstrap + link handler.
6. Blocked-state/sidebar alerts, promote command, more datasources.

## Stack

.NET (Native AOT binary for the plugin/MCP server; C# MCP SDK; AWS SDK for .NET), Docker, terminal-browser, mcp-grafana. Herdr calls via `HERDR_BIN_PATH`.
