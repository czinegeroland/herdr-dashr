# Collectors and the system dashboard

The dashboard pane runs collectors for as long as it lives, every few
seconds, into the session's Prometheus (`prometheus` datasource) and Loki
(`loki`). You add them; the pane keeps them running; you never poll. Every
`dashr collect ...` samples once first and prints the metric names and label
keys it produced (never values) — build panels from exactly those names.

```bash
dashr collect list                 # each collector and whether it works
dashr collect remove <id>          # stop one
```

All collector metrics are gauges that are already rates or current values:
chart them directly, no `rate()` needed.

## Local collectors

| Command | Metrics (labels) |
|---|---|
| `dashr collect docker [NAME...]` | `dashr_container_up`, `_cpu_percent`, `_memory_bytes`, `_memory_limit_bytes`, `_memory_percent`, `_pids`, `_network_rx_bytes_per_second`, `_network_tx_bytes_per_second`, `_disk_read_bytes_per_second`, `_disk_write_bytes_per_second` (`container`) — all prefixed `dashr_container_` |
| `dashr collect host` | `dashr_host_cpu_percent`, `_cpu_count`, `_load1`, `_memory_used_bytes`, `_memory_total_bytes`, `_swap_used_bytes`, `_disk_used_percent` (`mount`), `_disk_read_bytes_per_second`, `_disk_write_bytes_per_second`, `_network_rx_bytes_per_second`, `_network_tx_bytes_per_second` |
| `dashr collect process NAME` | `dashr_process_count`, `_cpu_percent` (100 = one core), `_memory_bytes`, `_disk_read_bytes_per_second`, `_disk_write_bytes_per_second` (`process`) — for apps run without Docker (`dotnet`, `node`, `java`, `python`) |
| `dashr collect logs CONTAINER [--service S]` | the container's log lines in Loki (`{service_name="S"}`), plus request metrics read from them (below) |
| `dashr collect postgres CONTAINER` | `dashr_pg_connections`, `_connections_by_state` (`state`), `_commits_per_second`, `_rollbacks_per_second`, `_cache_hit_ratio`, `_rows_{returned,fetched,inserted,updated,deleted}_per_second`, `_deadlocks_per_second`, `_temp_bytes_per_second`, `_longest_query_seconds`, `_database_size_bytes`; with `pg_stat_statements`: `_query_mean_ms`, `_query_calls_per_second`, `_query_time_ms_per_second` (`query`, normalized) — all `database`-labelled |
| `dashr collect mysql CONTAINER` | `dashr_mysql_connections`, `_threads_running`, `_queries_per_second`, `_slow_queries_per_second`, `_buffer_pool_hit_ratio`, `_received_bytes_per_second`, `_sent_bytes_per_second`, `_aborted_connects_per_second`, `_row_lock_waits_per_second` |
| `dashr collect redis CONTAINER` | `dashr_redis_clients`, `_blocked_clients`, `_memory_bytes`, `_max_memory_bytes`, `_ops_per_second`, `_hit_ratio`, `_evicted_keys_per_second`, `_expired_keys_per_second` |
| `dashr collect scrape URL --service S` | whatever the `/metrics` endpoint exposes, labelled `service` (counters stay counters: use `rate(x[1m])`) |

Database collectors run the database's own client inside its container
(`docker exec`) with the container's credentials; nothing is passed through
you.

## Request metrics from logs

`logs` and `stream` read requests from ASP.NET Core ("Request finished"),
Gin (Go, Ollama), nginx/Apache access logs, JSON logs with a status field, and
generic `GET /x 200 12ms` lines — method, status and duration only, never the
path. `dashr discover` says per container whether its log has such lines
(`request_log_formats`).

`dashr_http_requests_per_second`, `dashr_http_client_errors_per_second` (4xx),
`dashr_http_server_errors_per_second` (5xx), `dashr_http_error_ratio`
(5xx / all), `dashr_http_latency_p50_ms`, `_p95_ms`, `_p99_ms`,
`dashr_http_requests_total` — labelled `service`.

## Adapters for everything else

**`exec`** runs a command every `--every` seconds (default 60, minimum 10)
and reads Prometheus text from its stdout — one sample per line:

```text
cpu_utilization{resource="api-service"} 37.5
http_5xx{resource="alb/app/web/123"} 2
```

```bash
dashr collect exec --service cloud --every 60 -- sh /path/to/adapter.sh
```

Write the adapter as a small script around the environment's CLI or API
(examples in `reference/environments.md`), run it once yourself to check its
output, then hand it to `exec`. The trial refuses a command that prints no
samples. On Windows the command runs through `cmd /c`; write adapters in
PowerShell or Python there.

**`stream`** runs a long-running command and treats each output line as a log
line (to Loki, request metrics as above); it is restarted if it ends:

```bash
dashr collect stream --service api -- kubectl logs -f deploy/api --all-containers
```

Keep cloud polling reasonable: every metric call can be rate limited or
billed. One adapter fetching several metrics per call every 60 s is right.

## The system dashboard

Build this first, from whatever the collectors deliver, then add what the
human's question needs. Everything live, refresh `5s` (`10s` if only `exec`
adapters feed it).

1. **Health row** (stat panels, `w:4 h:4`): services up
   (`sum(dashr_container_up)` or the environment's equivalent), total request
   rate, 5xx ratio (red above 1 %), p95 latency (yellow > 300 ms, red > 1 s),
   host or cluster CPU %, memory %.
2. **One row per service** (the ones the human cares about first):
   - CPU % and memory (`timeseries`, memory in `bytes`, limit as a dashed line),
   - network rx/tx and disk read/write (`Bps`),
   - request rate split by 4xx / 5xx (stacked), p50/p95/p99 latency (`ms`).
3. **Database row** when one exists: connections by state, transactions (or
   queries) per second, cache hit ratio (`percentunit`, red below 0.9),
   longest running query, slowest normalized queries (`table` of
   `dashr_pg_query_mean_ms` sorted, `instant` query).
4. **Logs**: a `logs` panel of warnings and errors across services
   (`{service_name=~".+"} |~ "(?i)(error|exception|fail|warn)"`), and one per
   key service.

Useful queries:

```text
sum by (container) (dashr_container_cpu_percent)
dashr_container_memory_bytes / dashr_container_memory_limit_bytes
sum by (service) (dashr_http_requests_per_second)
sum(dashr_http_server_errors_per_second) / clamp_min(sum(dashr_http_requests_per_second), 1e-9)
max by (service) (dashr_http_latency_p95_ms)
topk(10, dashr_pg_query_mean_ms)
```

Name panels for what they answer ("Checkout p95 latency"), not for the
metric. Put the thing the human asked about top-left.
