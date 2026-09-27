# OpenTelemetry sessions and log checks

An OpenTelemetry session runs one container per pane that holds Grafana plus
an OpenTelemetry collector, Loki (logs), Tempo (traces) and Prometheus
(metrics). Telemetry is kept on disk inside the container and deleted with it
when the pane closes.

`session_info` returns the endpoint. Programs export to it with the standard
variables (already set in the chat pane's shell):

```sh
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:<port>   # OTLP/HTTP
OTEL_SERVICE_NAME=checkout                             # how it shows up
```

Programs without OpenTelemetry need no change — wrap them:

```sh
dashr tail -- cargo run --bin checkout      # service.name = cargo
dashr tail --service checkout -- ./run.sh   # choose the name
./app 2>&1 | dashr tail --service app       # or pipe
```

`dashr tail` still prints everything and exits with the command's status.
Each line becomes a log record with `service_name`, a severity guessed from
the line (`ERROR`, `WARN`, ...) and the stream (`log_iostream`: stdout or
stderr).

## Datasources

| uid | type | holds | personal |
|---|---|---|---|
| `loki` | loki | log lines | yes — lines are masked in samples |
| `tempo` | tempo | traces and spans | yes |
| `prometheus` | prometheus | OTLP metrics | no |

## "Did the right log messages fire?"

The human's usual ask: *I am about to run X; show me that these messages are
logged (and that those are not).* Use the expectation tools — they build the
panels for you and you never need to read a line.

```json
expect_logs {
  "expectations": [
    {"name": "order created",  "pattern": "order \\d+ created"},
    {"name": "payment captured", "pattern": "payment .* captured"},
    {"name": "no exceptions", "pattern": "exception|panicked", "expect": "absent"}
  ],
  "selector": "{service_name=\"checkout\"}"
}
```

The dashboard gains, at the top:

- one tile per expectation, counting matching lines **since arming**: grey
  `waiting` → green for an expected message; green `none` → red for a
  forbidden one;
- **Live log trail**: every line, newest first, expected matches on green,
  forbidden ones on red.

The tiles count from the dashboard's time range, which arming sets to the
moment of arming; the human's page reloads the dashboard after every change,
so it counts from arming too.

The pane notifies the human as each expected message arrives, and marks
itself blocked when a forbidden one does. Afterwards `log_expectations`
answers with counts per expectation and `passed`. Report that, e.g. "2 of 3
seen; *payment captured* not logged yet; no exceptions". Arming again
replaces the set and restarts the count; `clear_log_expectations` removes the
section and restores the dashboard below it.

Patterns:

- Case-insensitive regular expressions, matched anywhere in the line. Keep
  them short and literal: `order \d+ created`, `retry(ing)? in \d+ms`.
- Not allowed: `(?` constructs (inline flags, named groups, look-arounds) and
  backticks — the same pattern must mean the same to Loki and to the browser.
- Match the words the human cares about, not timestamps or ids.
- Never put personal data (an email, a name) in a pattern: it is shown on the
  dashboard and in notifications.

Up to 12 expectations. The section uses panel ids 9000–9199; do not reuse
them for your own panels.

## Queries for OpenTelemetry data

**Logs (LogQL).** Streams are labelled by `service_name`; `detected_level`
holds the severity.

```text
{service_name="checkout"}                                   # all lines
{service_name="checkout"} |~ `(?i)timeout`                  # matching
sum by (detected_level) (count_over_time({service_name="checkout"}[1m]))
sum(count_over_time({service_name=~".+"} |~ `(?i)error` [$__range])) or vector(0)
```

**Traces (TraceQL, Tempo).** Table of recent traces:

```json
{"refId": "A", "datasource": {"type": "tempo", "uid": "tempo"},
 "queryType": "traceql", "query": "{resource.service.name=\"checkout\"}",
 "limit": 20, "tableType": "traces"}
```

Slow spans: `{duration > 500ms}`; failed: `{status = error}`.

**Metrics (PromQL).** OTLP metric names are translated: dots become
underscores, counters gain `_total`, units are suffixed
(`http.server.duration` in ms → `http_server_duration_milliseconds_*`).
Resource attributes land on `target_info`; `job` is the service name.

```text
sum by (job) (rate(http_server_request_duration_seconds_count[1m]))
histogram_quantile(0.95, sum by (le) (rate(http_server_request_duration_seconds_bucket[5m])))
```

A range query is evaluated at step-aligned times, so a point sent a moment
ago appears in a time series only after the next step (up to 15 s over a few
minutes). To check that a metric has arrived, `probe_query` it as an instant
query: `{"expr": "dashr_e2e_orders_total", "instant": true, "range": false}`.

`probe_query` the metric name first — Prometheus returns `{__name__=~"http.*"}`
matches with their real names.

## How fast data appears

Logs, traces and metrics are queryable within about five seconds of being
sent (traces in about two). If a panel is still empty ten seconds after the
human ran the code, the data did not arrive: check the service name and the
endpoint with `session_info` rather than waiting longer.
