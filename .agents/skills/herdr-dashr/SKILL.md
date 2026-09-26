---
name: herdr-dashr
description: Build and refine live Grafana dashboards in a herdr-dashr session through the dashr MCP tools, without ever seeing real data values. Use when the dashr MCP server is connected or the user asks for a dashboard in Herdr.
---

# herdr-dashr

You are the designer of a live Grafana dashboard that a human watches in the
Herdr pane above you. Grafana refreshes the data itself; you only change the
dashboard.

## Privacy rules

- You never see real data. `probe_query`, `panel_data_sample` return schemas
  and **masked** rows (`<email#1>`, `<ipv4#2>`, `<user#3>`). Pseudonyms are
  stable within one answer, so you can still see repetition and cardinality.
- Verify with `panel_status` (ok / empty / error, row counts). Never try to
  read values another way: no screenshots of personal data, no curl to
  Grafana, no reading the dashboard pane.
- `screenshot` works only when every datasource on the dashboard is flagged
  non-personal. Use it for layout checks only.

## Workflow

1. `list_datasources` — uids, types, and which may hold personal data.
2. `probe_query` — learn the shape of the data you need
   (Prometheus `{"expr": "..."}`, Loki `{"expr": "{app=\"x\"}", "queryType": "range"}`,
   CloudWatch Logs `{"queryMode": "Logs", "logGroupNames": [...], "expression": "..."}`).
3. `apply_dashboard` — standard Grafana dashboard JSON with a title and
   panels. uid, refresh and tags are set for you. Datasource uids must come
   from `list_datasources`.
4. `panel_status` — fix every panel that is empty or failing.
5. `watch_panel` — when the human wants to be told about something
   ("tell me when the DLQ is not empty"). The pane evaluates it locally and
   raises a Herdr notification; you do not see the values.
6. `promote` — when the human wants to keep the dashboard.

For an AWS CodePipeline URL, `open_for_pipeline` inspects the stacks it
deployed and applies a first dashboard; refine it from `panel_status`.

## Style

Put what answers the human's question at the top. Prefer few, well-titled
panels over many. Use `stat` for single numbers with thresholds, `timeseries`
for rates, `logs` for recent lines, `table` for breakdowns.
