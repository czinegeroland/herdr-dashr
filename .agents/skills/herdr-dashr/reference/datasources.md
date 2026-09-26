# Query models per datasource

`probe_query` and every panel target use the datasource's own query model.
dashr adds `refId` and `datasource` for you in `probe_query`; in dashboard
targets set `"datasource": {"type": ..., "uid": ...}` (or inherit the panel's).
Get uids from `list_datasources`.

## TestData (`grafana-testdata-datasource`, uid `dashr-testdata`)

Always present, synthetic, non-personal. Use it to prototype layouts.

```json
{"scenarioId": "random_walk", "seriesCount": 2}
{"scenarioId": "csv_content", "csvContent": "service,errors\napi,3\nworker,7"}
{"scenarioId": "logs"}
```

Other scenarios: `predictable_pulse`, `table_static`, `no_data_points`,
`random_walk_table`.

## Prometheus / Mimir (`prometheus`)

```json
{"expr": "sum by (service) (rate(http_requests_total{code=~\"5..\"}[5m]))", "legendFormat": "{{service}}"}
{"expr": "histogram_quantile(0.95, sum by (le, route) (rate(http_request_duration_seconds_bucket[5m])))", "legendFormat": "p95 {{route}}"}
{"expr": "up{job=\"api\"}", "instant": true, "range": false}
```

- Discover metric names with `{"expr": "count by (__name__) ({job=\"api\"})", "instant": true, "range": false}` —
  the `__name__` field passes the masker.
- Labels of one metric: `{"expr": "count by (service, namespace) (http_requests_total)", "instant": true, "range": false}`.
- Counters need `rate()`/`increase()`; use `[5m]` or `$__rate_interval`.

## Loki (`loki`)

```json
{"expr": "{service=\"api\"} |= \"error\"", "queryType": "range"}
{"expr": "sum by (level) (count_over_time({service=\"api\"} | json [5m]))", "queryType": "range", "legendFormat": "{{level}}"}
{"expr": "topk(10, sum by (route) (count_over_time({service=\"api\"} |= \"timeout\" [15m])))", "queryType": "instant"}
```

- Log lines in samples are masked; labels such as `level`, `service_name`
  pass when allow-listed.
- Metric queries (`count_over_time`, `rate`) are better for stats and
  timeseries; plain selectors for `logs` panels.

## Tempo (`tempo`)

```json
{"queryType": "traceql", "query": "{ resource.service.name = \"api\" && status = error }", "limit": 20}
{"queryType": "traceqlSearch", "filters": [{"id": "service-name", "tag": "service.name", "operator": "=", "value": ["api"], "scope": "resource"}]}
```

Use a `table` panel for search results and a `traces` panel for one trace.

## CloudWatch (`cloudwatch`)

Metrics:

```json
{"queryMode": "Metrics", "metricQueryType": 0, "metricEditorMode": 0, "region": "eu-west-1",
 "namespace": "AWS/SQS", "metricName": "ApproximateNumberOfMessagesVisible",
 "dimensions": {"QueueName": ["orders-dlq"]}, "statistic": "Maximum", "period": "60",
 "matchExact": true, "id": "", "expression": ""}
```

Common metrics: `AWS/Lambda` `Errors`, `Throttles`, `Duration`, `Invocations`
(dimension `FunctionName`); `AWS/SQS` `ApproximateNumberOfMessagesVisible`,
`ApproximateAgeOfOldestMessage` (`QueueName`); `AWS/States`
`ExecutionsFailed`, `ExecutionsStarted`, `ExecutionsTimedOut`
(`StateMachineArn`); `AWS/ApplicationELB` `HTTPCode_Target_5XX_Count`,
`TargetResponseTime` (`LoadBalancer`); `AWS/ECS` `CPUUtilization`
(`ClusterName`, `ServiceName`).

Logs Insights:

```json
{"queryMode": "Logs", "region": "eu-west-1", "logGroupNames": ["/aws/lambda/api-handler"],
 "expression": "fields @timestamp, @message | filter @message like /(?i)error/ | sort @timestamp desc | limit 100"}
{"queryMode": "Logs", "region": "eu-west-1", "logGroupNames": ["/aws/lambda/api-handler"],
 "expression": "filter @message like /(?i)error/ | stats count(*) as errors by bin(1m)", "statsGroups": ["bin(1m)"]}
```

`@timestamp`, `@log`, `@logStream` pass the masker; `@message` is scanned.
Credential errors mean the human must refresh AWS credentials and reopen the
pane.

## SQL Server (`mssql`)

```json
{"format": "time_series", "rawSql": "SELECT $__timeGroupAlias(created_at, 5m), COUNT(*) AS orders FROM orders WHERE $__timeFilter(created_at) GROUP BY $__timeGroup(created_at, 5m) ORDER BY 1"}
{"format": "table", "rawSql": "SELECT TOP 20 status, COUNT(*) AS n FROM orders WHERE $__timeFilter(created_at) GROUP BY status ORDER BY n DESC"}
```

Select aggregates, not rows of people. Columns named like `email`, `name`,
`customer` are redacted in samples anyway.

## Azure Monitor (`grafana-azure-monitor-datasource`)

```json
{"queryType": "Azure Log Analytics", "azureLogAnalytics": {"query": "AppRequests | where Success == false | summarize count() by bin(TimeGenerated, 5m), OperationName", "resources": ["/subscriptions/<sub>/resourceGroups/<rg>/providers/Microsoft.OperationalInsights/workspaces/<ws>"], "resultFormat": "time_series"}}
{"queryType": "Azure Monitor", "azureMonitor": {"resourceUri": "/subscriptions/<sub>/resourceGroups/<rg>/providers/Microsoft.Web/sites/<app>", "metricNamespace": "microsoft.web/sites", "metricName": "Http5xx", "aggregation": "Total", "timeGrain": "auto"}}
```

## Zabbix (`alexanderzobnin-zabbix-datasource`)

```json
{"queryType": "0", "group": {"filter": "Production"}, "host": {"filter": "/api-.*/"}, "application": {"filter": ""}, "item": {"filter": "CPU utilization"}, "functions": [], "options": {"showDisabledItems": false}}
```

Filters accept exact names or `/regex/`. Needs the custom image
(`dashr image build`).

## Seq through Infinity (`yesoreyeram-infinity-datasource`)

```json
{"type": "json", "source": "url", "parser": "backend", "format": "table",
 "url": "/api/events?count=100&filter=@Level%20%3D%20'Error'&render=true",
 "url_options": {"method": "GET"}, "root_selector": "",
 "columns": [
   {"selector": "Timestamp", "text": "time", "type": "timestamp"},
   {"selector": "Level", "text": "level", "type": "string"},
   {"selector": "RenderedMessage", "text": "message", "type": "string"}
 ]}
```

The URL is relative to the Seq base URL configured on the datasource; the API
key header is added by dashr's provisioning. Needs the custom image.
