# Debugging recipes

Starting points for common questions. Adapt names and uids, apply, then fix
what `panel_status` reports. Each recipe says what to watch.

## "Did the deploy break anything?"

1. Stats across the top: error rate now, p95 latency now, requests/s.
2. Timeseries of error rate and latency over `now-3h`, so the deploy moment
   is visible.
3. Logs panel: errors since the deploy.
4. Offer a watch: error rate `> ` the pre-deploy level.

## "Why is the queue backing up?" (SQS)

1. Stat: DLQ `ApproximateNumberOfMessagesVisible` (`Maximum`), red at ≥ 1.
2. Timeseries: main queue depth and `ApproximateAgeOfOldestMessage`.
3. Timeseries: consumer Lambda `Errors`, `Throttles`, `Duration`.
4. Logs: consumer log group filtered on errors/timeouts.
5. Watch: DLQ `max > 0` with label "DLQ not empty".

## "Step Functions executions are failing"

1. Stat: `ExecutionsFailed` sum over the range.
2. Timeseries: `ExecutionsStarted` vs `ExecutionsFailed` vs `ExecutionsTimedOut`.
3. Logs: the Lambdas the state machine calls, filtered on errors.
4. Watch: `ExecutionsFailed` `sum > 0`.

## "Error spike — where from?"

1. Timeseries: errors `sum by (service)` (Prometheus) or
   `sum by (service_name) (count_over_time({level="error"}[5m]))` (Loki).
2. Bar gauge / table: top 10 routes or services by error count over the
   spike window (set the dashboard `time` to cover it).
3. Logs panel for the worst one. Drill down by adding a panel, not by
   replacing the overview.

## "Is it slow?"

1. Timeseries: p50/p95/p99 with `histogram_quantile` over `le`.
2. Table: p95 by route, sorted descending.
3. Timeseries: saturation (CPU, connections, queue depth) next to latency.
4. Watch: p95 `last > <SLO>`.

## "Show me the logs for X"

A `logs` panel full width, query narrowed by service/label, plus an error
count timeseries above it. Do not try to summarise the log content — you only
see masked lines; the human reads them.

## CodePipeline / ephemeral environment

Call `open_for_pipeline` with the console URL. It inspects the deployed
stacks and applies panels for log groups, queues/DLQs, state machines and
Lambdas. Then:

1. `panel_status` — CloudWatch credential errors mean the human must refresh
   AWS credentials (`aws sso login`) and reopen the pane.
2. Remove panels for resources that do not matter to the question.
3. Add the watch that matches the question (DLQ not empty, failed executions).

## Watches

`watch_panel` evaluates a panel's numeric series every few seconds inside the
pane:

| Want | `reducer` | `op` | `threshold` |
|---|---|---|---|
| Any message in the DLQ | `max` | `>` | `0` |
| Error rate above 1/s | `last` | `>` | `1` |
| Any failed execution in range | `sum` | `>` | `0` |
| Log panel shows anything | `count` | `>` | `0` |
| Heartbeat stopped | `count` | `==` | `0` |

Give it a short `label`; it becomes the Herdr notification. The pane turns
blocked on a new breach and returns to idle when every watch clears.
