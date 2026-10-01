# Pull sources: traces from wherever the services run

A deployed environment does not send spans to the human's laptop: each
service reports to its own backend (X-Ray, Application Insights, Cloud
Trace, a team Jaeger). A pull source is a command dashr runs every few
seconds that prints recent traces; dashr converts them, joins them with
everything else by trace id, and sends them on to the session's Jaeger.

```bash
dashr source add <name> --format <format> --every 10 [--lookback 15] -- <command> [args...]
dashr source list
dashr source rm <name>
```

The command gets the window to read in its environment:

| Variable | Value |
|---|---|
| `DASHR_SINCE`, `DASHR_UNTIL` | Unix seconds |
| `DASHR_SINCE_MS`, `DASHR_UNTIL_MS` | Unix milliseconds |
| `DASHR_SINCE_ISO`, `DASHR_UNTIL_ISO` | RFC 3339 UTC |
| `DASHR_SOURCE` | the source's name |

Each run re-reads two minutes before the previous one (late spans) and
dashr keeps one copy per span. Printing nothing is fine (no traffic).
Several JSON documents in a row, or JSON lines, are fine. On Windows the
command runs through `cmd /c`; use `pwsh -NoProfile -Command "..."` with
`$env:DASHR_SINCE` for the recipes below.

Formats: `xray`, `appinsights`, `cloudtrace`, `jaeger`, `zipkin`, `otlp`
(OTLP/JSON, also Tempo), `otlp-proto`, or `auto` (recognised by shape). For
anything else, convert to OTLP/JSON with `jq` or a few lines of Python and
use `--format otlp`.

One source per account, region, project or workspace. The human's CLI
credentials are used as they are: ask them to log in when a trial fails
with an authentication error.

## AWS X-Ray (Lambda, Step Functions, ECS, API Gateway, ...)

Prerequisites, checked before adding: `aws sts get-caller-identity` works;
active tracing is on (`aws lambda get-function-configuration --function-name
F --query TracingConfig.Mode` is `Active`; `aws stepfunctions
describe-state-machine --state-machine-arn A --query
tracingConfiguration.enabled` is `true`); the ECS task runs the ADOT
collector or X-Ray daemon sidecar. If not, tell the human what to switch on.

```bash
dashr source add aws --format xray --every 10 -- sh -c '
  ids=$(aws xray get-trace-summaries --start-time "$DASHR_SINCE" --end-time "$DASHR_UNTIL" \
        --filter-expression "annotation.test_run = \"r-42\"" \
        --query "TraceSummaries[].Id" --output text)
  [ -z "$ids" ] || [ "$ids" = None ] || echo $ids | xargs -n 5 aws xray batch-get-traces --output json --trace-ids'
```

- `batch-get-traces` takes at most 5 ids per call: `xargs -n 5`.
- Narrow with a filter expression: `service("checkout-machine")`,
  `annotation.test_run = "r-42"`, `fault = true`, `responsetime > 2`.
- Add `--profile P --region R` to both calls for other accounts and regions
  (one source each).
- X-Ray indexes traces within seconds to a minute: give flows that use it
  `"settle_secs": 30` or more.
- Step Functions passes the trace header to Lambda tasks; check that the
  ECS task and SQS/EventBridge consumers continue the trace (a span from
  `?` in the sequence means they did not).

## Azure Application Insights

Classic resource (`az monitor app-insights query`, the app id from
`az monitor app-insights component show`):

```bash
dashr source add azure --format appinsights --every 15 -- sh -c '
  az monitor app-insights query --app "$APP_ID" --offset 1h --output json --analytics-query "
    union requests, dependencies
    | where timestamp between (datetime($DASHR_SINCE_ISO) .. datetime($DASHR_UNTIL_ISO))
    | project itemType, operation_Id, id, operation_ParentId, name, timestamp, duration,
              success, resultCode, cloud_RoleName, target, type, customDimensions"'
```

Workspace-based (`az monitor log-analytics query`, the workspace's
customer id):

```bash
dashr source add azure --format appinsights --every 15 -- sh -c '
  az monitor log-analytics query -w "$WORKSPACE_ID" --output json --analytics-query "
    union AppRequests, AppDependencies
    | where TimeGenerated between (datetime($DASHR_SINCE_ISO) .. datetime($DASHR_UNTIL_ISO))
    | project Type, OperationId, Id, ParentId, Name, TimeGenerated, DurationMs, Success,
              ResultCode, AppRoleName, Target, DependencyType, Properties"'
```

Requests become server spans, dependencies client spans; custom dimensions
become attributes. Ingestion delay is typically 1-3 minutes: use a large
`settle_secs`.

## Google Cloud Trace

```bash
dashr source add gcp --format cloudtrace --every 15 -- sh -c '
  curl -sf -H "Authorization: Bearer $(gcloud auth print-access-token)" \
    "https://cloudtrace.googleapis.com/v1/projects/$PROJECT/traces?view=COMPLETE&pageSize=100&startTime=$DASHR_SINCE_ISO&endTime=$DASHR_UNTIL_ISO"'
```

## A remote Jaeger, Tempo or Zipkin

```bash
# Jaeger query API (also what Tempo's Jaeger endpoints answer)
dashr source add staging --format jaeger --every 10 -- sh -c '
  curl -sf "$JAEGER/api/traces?service=orders-api&limit=50&start=${DASHR_SINCE_MS}000&end=${DASHR_UNTIL_MS}000"'

# Grafana Tempo: search, then fetch each trace as OTLP/JSON
dashr source add tempo --format otlp --every 10 -- sh -c '
  curl -sf "$TEMPO/api/search?start=$DASHR_SINCE&end=$DASHR_UNTIL&limit=50" |
    python3 -c "import json,sys; print(\"\n\".join(t[\"traceID\"] for t in json.load(sys.stdin).get(\"traces\", [])))" |
    while read id; do curl -sf -H "Accept: application/json" "$TEMPO/api/traces/$id"; done'

# Zipkin
dashr source add zipkin --format zipkin --every 10 -- sh -c '
  curl -sf "$ZIPKIN/api/v2/traces?limit=50&endTs=$DASHR_UNTIL_MS&lookback=$((DASHR_UNTIL_MS - DASHR_SINCE_MS))"'
```

A service only reachable inside a cluster: put `kubectl port-forward` (or
an SSM/ssh tunnel) in front, started by the human or kept alive in the
command.

## One-off imports

```bash
dashr ingest trace-export.json --format auto --source export
aws xray batch-get-traces --trace-ids 1-... | dashr ingest - --format xray --source aws
```
