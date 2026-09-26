---
name: herdr-dashr
description: Build, verify and evolve live Grafana dashboards in a herdr-dashr session through the dashr MCP tools, from the human's debugging question, without ever seeing real data values. Use whenever the dashr MCP server is connected, the human asks for a dashboard, graph, panel, log view, queue/alarm view or "show me" in Herdr, or opens a CodePipeline for debugging.
metadata:
  generated-by: herdr-dashr
---

# Building dashboards with herdr-dashr

You design a live Grafana dashboard that the human watches in the Herdr pane
above you. Grafana fetches and refreshes the data itself; **you only change
the dashboard**. You never see data values — only schemas, masked samples,
row counts and errors. Treat that as the physics of this environment, not an
obstacle.

## The loop

Work in short iterations. A useful dashboard in two minutes beats a perfect
one in twenty.

1. **Understand the question.** What is the human debugging? A deploy, an
   error spike, a queue backing up, slow requests? Ask one short question if
   it is unclear; otherwise start.
2. **Discover.** `list_datasources` (uids, types, which are personal). Then
   `probe_query` the data you think answers the question — it returns field
   names, types, row counts and masked samples. Two or three probes are
   usually enough. See `reference/datasources.md` for each query model.
3. **Design.** Pick three to six panels that answer the question, most
   important first (top-left). Start from a recipe in
   `reference/recipes.md`; panel JSON rules are in
   `reference/dashboard-json.md`.
4. **Apply.** `apply_dashboard` with the whole dashboard. dashr validates
   datasource references, assigns ids and layout where missing, pins the uid,
   refresh and tags, and reloads the browser pane.
5. **Verify.** `panel_status`. Every panel should be `ok`. For each `empty` or
   `error` panel: read the (masked) error, `probe_query` a simpler version,
   fix, re-apply. Never leave a broken panel on the dashboard; remove it if
   you cannot fix it and say why.
6. **Evolve.** When the human reacts ("what about the DLQ?", "only prod",
   "why did that spike?"), change the dashboard: add a drill-down panel,
   narrow a query, split by a label, change the time range. `get_dashboard`
   returns the current JSON to edit — modify it and re-apply rather than
   rebuilding from scratch.
7. **Watch and keep.** If the human wants to be told when something happens,
   `watch_panel` (evaluated locally; you never see the values). If the
   dashboard is worth keeping, `save_dashboard` it under a name the human
   picks (it stays on this machine and `load_dashboard` reopens it in a
   later pane), or `promote` it to the team's Grafana. When a session starts
   and the human names a saved dashboard, load it before building anything.

Report briefly after each apply: what the dashboard now shows and what is
still empty or failing. Do not describe values — you do not know them; the
human can see them.

## Privacy rules (enforced — do not work around them)

- Samples are masked: `<email#1>`, `<ipv4#2>`, `<user#3>`, `<card#1>`. The same
  value gets the same pseudonym within one answer, so repetition and
  cardinality are visible. Use that: "12 rows, 3 distinct users" is fine.
- Never try to read values another way: no `curl` to the Grafana port, no
  reading the dashboard pane, no screenshots of dashboards with personal
  datasources (the `screenshot` tool refuses them; it exists for layout
  checks on non-personal data).
- Do not put personal data into queries or titles. Filter by labels and
  services, not by a person's email.
- Error messages are masked too; a `<email#1>` in an error is a hint that the
  query matched personal data.

## Tools at a glance

| Tool | Use it to |
|---|---|
| `list_datasources` | Learn uids, types and the personal flag. |
| `probe_query` | Test one query and see its schema and masked rows before building a panel. |
| `apply_dashboard` | Push the whole dashboard JSON. |
| `get_dashboard` | Fetch the current JSON to modify. |
| `panel_status` | Check every panel: ok / empty / error, rows, fields, masked errors. |
| `panel_data_sample` | Masked rows of one existing panel, to refine it. |
| `watch_panel` / `list_watches` / `remove_watch` | Alert the human on a threshold (Herdr notification, pane turns blocked). |
| `screenshot` | Layout check — non-personal dashboards only. |
| `open_for_pipeline` | Inspect an AWS CodePipeline and apply a first dashboard. |
| `promote` | Copy the dashboard to the team's persistent Grafana. |
| `session_info` | Mode and, in OpenTelemetry mode, the OTLP endpoint programs export to. |
| `save_dashboard` / `list_saved_dashboards` / `load_dashboard` / `delete_saved_dashboard` | Keep a dashboard on this machine by name and reopen it in a later session. |
| `expect_logs` / `log_expectations` / `clear_log_expectations` | "Did the right log messages fire?" — tiles per expected (or forbidden) message, a highlighted live trail, a counts-only verdict. |

The same references are available as MCP resources (`dashr://guide/...`) if
this skill's files are not on disk.

## OpenTelemetry sessions and log checks

When `session_info` says `opentelemetry`, the session's own Loki, Tempo and
Prometheus receive whatever is sent to its OTLP endpoint, and `dashr tail --
<command>` ships any command's output as logs. The human's typical ask is
"run this and show me the right messages are logged": arm them with
`expect_logs`, let the code run, then answer from `log_expectations`. Details,
LogQL/TraceQL/PromQL for OTel data and patterns that work are in
`reference/otel-and-logs.md`.

## Choosing panels

| Question | Panel |
|---|---|
| Is it happening right now? One number | `stat` with thresholds |
| How is it trending? Rates, latency, depth | `timeseries` |
| What exactly failed? | `logs` (recent lines) or `table` |
| Which one is worst? Breakdown by service/queue/route | `bargauge` or `table` sorted |
| Is it within limits? | `gauge` with min/max |
| When did state change? | `state-timeline` |
| Context the human needs | `text` (markdown) — short |

## When things go wrong

- **`unknown datasource`**: use a uid from `list_datasources`, never a name.
- **`empty` panel**: the query ran but matched nothing. Widen the time range
  (`"time": {"from": "now-6h", "to": "now"}`), loosen label matchers, check
  the metric/log group name with `probe_query`.
- **`error` panel**: read the masked error. Syntax errors — fix the query
  language. Auth/credentials errors (CloudWatch, SQL) — tell the human; you
  cannot fix credentials. Timeouts — shorten the range or aggregate more.
- **CodePipeline**: if `open_for_pipeline` opened a new tab, a different
  session owns that pipeline; continue there.
