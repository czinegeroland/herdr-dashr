# Dashboard JSON for herdr-dashr

`apply_dashboard` takes standard Grafana dashboard JSON (schema 39, Grafana
12). You provide `title` and `panels`; dashr fills in the rest.

## What dashr sets for you

| Field | Behaviour |
|---|---|
| `uid` | Replaced with the session's uid. Do not choose one. |
| `id`, `version` | Removed. |
| `refresh` | Set to the session refresh (default `5s`). |
| `tags` | `dashr` is added. |
| `time` | Defaults to `now-1h` → `now` if you omit it. Set it yourself when the question needs another range. |
| `panels[].id` | Assigned when missing or duplicated. Use explicit ids when you plan to `watch_panel` or `panel_data_sample` them. |
| `panels[].gridPos` | Two-per-row flow layout when missing. Set it yourself for anything deliberate. |

Validation refuses: no title, panels not an array, a panel without `type`, a
datasource uid that is not provisioned, more than 60 panels.

## Skeleton

Every example below is validated by the repository's tests against the
datasource uids `dashr-testdata`, `prometheus` and `loki`. Replace uids with
what `list_datasources` returns.

```json dashr-example
{
  "title": "Checkout errors",
  "time": {"from": "now-3h", "to": "now"},
  "panels": [
    {
      "id": 1,
      "type": "stat",
      "title": "5xx rate (req/s)",
      "gridPos": {"x": 0, "y": 0, "w": 6, "h": 6},
      "datasource": {"type": "prometheus", "uid": "prometheus"},
      "targets": [{"refId": "A", "expr": "sum(rate(http_requests_total{service=\"checkout\",code=~\"5..\"}[5m]))"}],
      "fieldConfig": {
        "defaults": {
          "unit": "reqps",
          "decimals": 2,
          "thresholds": {"mode": "absolute", "steps": [
            {"color": "green", "value": null},
            {"color": "orange", "value": 0.1},
            {"color": "red", "value": 1}
          ]}
        }
      },
      "options": {"colorMode": "background", "reduceOptions": {"calcs": ["lastNotNull"]}}
    },
    {
      "id": 2,
      "type": "timeseries",
      "title": "Requests by status class",
      "gridPos": {"x": 6, "y": 0, "w": 18, "h": 6},
      "datasource": {"type": "prometheus", "uid": "prometheus"},
      "targets": [{"refId": "A", "expr": "sum by (code) (rate(http_requests_total{service=\"checkout\"}[5m]))", "legendFormat": "{{code}}"}],
      "fieldConfig": {"defaults": {"unit": "reqps"}}
    },
    {
      "id": 3,
      "type": "logs",
      "title": "Recent errors",
      "gridPos": {"x": 0, "y": 6, "w": 24, "h": 10},
      "datasource": {"type": "loki", "uid": "loki"},
      "targets": [{"refId": "A", "expr": "{service=\"checkout\"} |= \"error\"", "queryType": "range"}],
      "options": {"showTime": true, "wrapLogMessage": true, "sortOrder": "Descending"}
    }
  ]
}
```

A prototype on synthetic data, useful to check a layout before real data
exists (the end-to-end suite applies this one to a real Grafana and requires
every panel to be `ok`):

```json dashr-example
{
  "title": "Layout prototype",
  "panels": [
    {"id": 1, "type": "stat", "title": "Errors now", "gridPos": {"x": 0, "y": 0, "w": 6, "h": 5},
     "datasource": {"type": "grafana-testdata-datasource", "uid": "dashr-testdata"},
     "targets": [{"refId": "A", "scenarioId": "random_walk"}],
     "options": {"reduceOptions": {"calcs": ["lastNotNull"]}}},
    {"id": 2, "type": "timeseries", "title": "Error rate", "gridPos": {"x": 6, "y": 0, "w": 18, "h": 5},
     "datasource": {"type": "grafana-testdata-datasource", "uid": "dashr-testdata"},
     "targets": [{"refId": "A", "scenarioId": "random_walk", "seriesCount": 2}]},
    {"id": 3, "type": "bargauge", "title": "Errors by service", "gridPos": {"x": 0, "y": 5, "w": 12, "h": 6},
     "datasource": {"type": "grafana-testdata-datasource", "uid": "dashr-testdata"},
     "targets": [{"refId": "A", "scenarioId": "csv_content", "csvContent": "service,errors\napi,12\nworker,4\nbilling,1"}]},
    {"id": 4, "type": "logs", "title": "Recent lines", "gridPos": {"x": 12, "y": 5, "w": 12, "h": 6},
     "datasource": {"type": "grafana-testdata-datasource", "uid": "dashr-testdata"},
     "targets": [{"refId": "A", "scenarioId": "logs"}]}
  ]
}
```

## Layout

- The grid is **24 columns** wide; `h` is in rows of ~30 px. `gridPos` is
  `{"x", "y", "w", "h"}`.
- Typical sizes: stat `w: 4–6, h: 4–6`; timeseries `w: 12–24, h: 8`; logs
  `w: 24, h: 10–14`; table `w: 12–24, h: 8–12`.
- Put the answer top-left. A row of 3–4 stats across the top, charts below,
  logs at the bottom works for most debugging views.
- Rows: `{"type": "row", "title": "Queues", "collapsed": false, "gridPos": {"x": 0, "y": 20, "w": 24, "h": 1}, "panels": []}`.
  Collapsed rows keep their children in `panels`.

## Panel types

| `type` | Notes |
|---|---|
| `timeseries` | Default for anything over time. `fieldConfig.defaults.custom.drawStyle`: `line`, `bars`, `points`. |
| `stat` | One number. `options.reduceOptions.calcs`: `lastNotNull`, `max`, `sum`, `mean`. `options.colorMode`: `value` or `background`. |
| `gauge`, `bargauge` | Bounded values; set `fieldConfig.defaults.min`/`max`. |
| `table` | Breakdown. Sort with `options.sortBy: [{"displayName": "Value", "desc": true}]`. |
| `logs` | Loki, CloudWatch Logs, Seq. `options.showTime`, `wrapLogMessage`, `sortOrder`. |
| `state-timeline` | Discrete states over time (up/down, pipeline stage status). |
| `text` | `options: {"mode": "markdown", "content": "..."}`. No datasource. |
| `traces` | Tempo trace view. |

## Field config essentials

```text
fieldConfig.defaults.unit        reqps, ops, percent, percentunit, s, ms, bytes, short, none
fieldConfig.defaults.decimals    2
fieldConfig.defaults.min / max   for gauges and percentages
fieldConfig.defaults.thresholds  {"mode": "absolute", "steps": [{"color": "green", "value": null}, {"color": "red", "value": 80}]}
fieldConfig.defaults.color       {"mode": "thresholds"} or {"mode": "palette-classic"}
fieldConfig.overrides            [{"matcher": {"id": "byName", "options": "errors"}, "properties": [{"id": "color", "value": {"mode": "fixed", "fixedColor": "red"}}]}]
```

## Transformations (optional)

`"transformations": [{"id": "organize", "options": {"excludeByName": {"id": true}}}]`,
`{"id": "reduce", "options": {"reducers": ["max"]}}`,
`{"id": "filterByValue", ...}`. Keep them simple; prefer doing the work in the
query.

## Mixed datasources

Set the panel datasource to `{"uid": "-- Mixed --"}` and give every target its
own `datasource`. Useful to overlay a deploy marker series on a metric.

## Things to avoid

- Dashboard variables (`templating`) with datasource variables: dashr cannot
  check or monitor `${ds}` references. Hard-code uids; create more panels
  instead of variables.
- Hundreds of series: aggregate (`sum by (service)`) or `topk(10, ...)`.
- Very long ranges with fine resolution; let Grafana pick the interval.
