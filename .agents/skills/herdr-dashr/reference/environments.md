# Discovering and feeding any environment

`dashr discover` covers this machine. For everything else, discover with the
environment's own CLI, then feed the dashboard with `dashr collect exec`
(metrics as Prometheus text) and `dashr collect stream` (log lines). If a
CLI is missing, install it; if it is not logged in, ask the human to log in
(or to let you start the login) — never ask for secrets in the chat and never
write credentials into files or commands.

`dashr discover` → `tools.installed` and `tools.configured` say which CLIs
exist and which clouds look logged in; `project.files` hints at how the thing
is deployed (`docker-compose.yml`, `*.tf`, `serverless.yml`, `template.yaml`,
`azure.yaml`, `app.yaml`, `Chart.yaml`, `k8s/`, ...). Read those files to find
service names, regions, clusters and resource groups rather than guessing.

## This machine

`dashr discover`, then typically:

```bash
dashr collect docker
dashr collect host
dashr collect logs <each app container>
dashr collect postgres <db container>     # mysql / redis alike
dashr collect process dotnet              # an app run with `dotnet run`, `npm start`, ...
```

An app run in a terminal without Docker: ask the human to start it through
`dashr tail -- <command>` (its output then reaches Loki), or `stream` its log
file (`tail -F app.log`; `Get-Content -Wait app.log` on Windows).

## Kubernetes

Discover: `kubectl config current-context`, `kubectl get deploy,sts,svc,pods -A`,
`kubectl top pods -A` (needs metrics-server).

```bash
# CPU (millicores) and memory (MiB) per pod, every 30 s
cat > /tmp/k8s-top.sh <<'EOF'
kubectl top pods -n "$1" --no-headers | awk '{gsub("m","",$2); gsub("Mi","",$3);
  printf "k8s_pod_cpu_millicores{pod=\"%s\"} %s\nk8s_pod_memory_mib{pod=\"%s\"} %s\n", $1, $2, $1, $3}'
EOF
dashr collect exec --service k8s --every 30 -- sh /tmp/k8s-top.sh shop
dashr collect stream --service api -- kubectl logs -f -n shop deploy/api --all-containers --since=1s
```

Restarts: `kubectl get pods -n shop -o json` → `status.containerStatuses[].restartCount`.
A cluster with Prometheus already running: port-forward it and `scrape`
selected endpoints, or scrape the app's `/metrics` through a port-forward.

## AWS

Discover: `aws sts get-caller-identity`, `aws configure get region`, then the
deployment: `aws cloudformation list-stacks`, `list-stack-resources`, or the
service directly (`aws ecs list-services`, `aws lambda list-functions`,
`aws rds describe-db-instances`, `aws elbv2 describe-load-balancers`). A
CodePipeline link can instead be passed to the pane (`DASHR_PIPELINE_URL`),
which applies a first dashboard of the pipeline's stacks.

What to collect (CloudWatch): ECS `CPUUtilization`, `MemoryUtilization`
(ClusterName, ServiceName); Lambda `Invocations`, `Errors`, `Duration`
(p95), `Throttles`, `ConcurrentExecutions`; ALB `RequestCount`,
`TargetResponseTime`, `HTTPCode_Target_5XX_Count`; RDS `CPUUtilization`,
`DatabaseConnections`, `ReadLatency`, `WriteLatency`, `FreeableMemory`,
`ReadIOPS`, `WriteIOPS`; SQS `ApproximateNumberOfMessagesVisible`,
`ApproximateAgeOfOldestMessage`; DynamoDB `ConsumedRead/WriteCapacityUnits`,
`ThrottledRequests`.

```bash
cat > /tmp/aws.py <<'EOF'
import json, subprocess, datetime as dt
now = dt.datetime.now(dt.timezone.utc)
queries = [  # id, namespace, metric, dimensions, stat, label
  ("cpu", "AWS/ECS", "CPUUtilization", {"ClusterName": "prod", "ServiceName": "api"}, "Average", 'ecs_cpu_percent{service="api"}'),
  ("err", "AWS/Lambda", "Errors", {"FunctionName": "orders"}, "Sum", 'lambda_errors{function="orders"}'),
]
req = [{"Id": q[0], "MetricStat": {"Metric": {"Namespace": q[1], "MetricName": q[2],
        "Dimensions": [{"Name": k, "Value": v} for k, v in q[3].items()]}, "Period": 60, "Stat": q[4]}}
       for q in queries]
out = json.loads(subprocess.check_output(["aws", "cloudwatch", "get-metric-data",
      "--metric-data-queries", json.dumps(req),
      "--start-time", (now - dt.timedelta(minutes=10)).isoformat(), "--end-time", now.isoformat()]))
for q, r in zip(queries, sorted(out["MetricDataResults"], key=lambda r: [x[0] for x in queries].index(r["Id"]))):
    if r["Values"]:
        print(f"{q[5]} {r['Values'][0]}")
EOF
dashr collect exec --service aws --every 60 -- python3 /tmp/aws.py
dashr collect stream --service api-logs -- aws logs tail /ecs/api --follow --since 1m
```

## Azure

Discover: `az account show`, `az resource list -g <group> -o table`,
`az webapp list`, `az containerapp list`, `az aks list`, `az sql db list`.

What to collect (`az monitor metrics list --resource <id> --metric ...`):
App Service `CpuTime`, `MemoryWorkingSet`, `Requests`, `Http5xx`,
`HttpResponseTime`; Container Apps `UsageNanoCores`, `WorkingSetBytes`,
`Requests`; Azure SQL `cpu_percent`, `dtu_consumption_percent`,
`connection_successful`, `deadlock`; Functions `FunctionExecutionCount`,
`Http5xx`.

```bash
cat > /tmp/azure.sh <<'EOF'
az monitor metrics list --resource "$1" --metric Requests Http5xx HttpResponseTime \
  --interval PT1M --aggregation Total Average -o json |
python3 -c '
import json, sys
for m in json.load(sys.stdin)["value"]:
    points = [p for t in m["timeseries"] for p in t["data"] if (p.get("total") or p.get("average")) is not None]
    if points:
        p = points[-1]
        print("azure_%s{resource=\"web\"} %s" % (m["name"]["value"].lower(), p.get("total", p.get("average"))))'
EOF
dashr collect exec --service azure --every 60 -- sh /tmp/azure.sh "<resource id>"
dashr collect stream --service web-logs -- az webapp log tail -g <group> -n <app>
```

## Google Cloud

Discover: `gcloud config list`, `gcloud run services list`,
`gcloud compute instances list`, `gcloud sql instances list`,
`gcloud container clusters list`.

What to collect (Cloud Monitoring): Cloud Run `run.googleapis.com/request_count`,
`request_latencies`, `container/cpu/utilizations`,
`container/memory/utilizations`; Cloud SQL
`cloudsql.googleapis.com/database/cpu/utilization`,
`database/postgresql/num_backends`; GCE
`compute.googleapis.com/instance/cpu/utilization`.

```bash
cat > /tmp/gcp.sh <<'EOF'
TOKEN=$(gcloud auth print-access-token); P=$(gcloud config get-value project 2>/dev/null)
END=$(date -u +%Y-%m-%dT%H:%M:%SZ); START=$(date -u -d '-5 min' +%Y-%m-%dT%H:%M:%SZ)
curl -s -H "Authorization: Bearer $TOKEN" -G "https://monitoring.googleapis.com/v3/projects/$P/timeSeries" \
  --data-urlencode 'filter=metric.type="run.googleapis.com/container/cpu/utilizations"' \
  --data-urlencode "interval.startTime=$START" --data-urlencode "interval.endTime=$END" \
  --data-urlencode 'aggregation.alignmentPeriod=60s' --data-urlencode 'aggregation.perSeriesAligner=ALIGN_PERCENTILE_95' |
python3 -c '
import json, sys
for s in json.load(sys.stdin).get("timeSeries", []):
    v = s["points"][0]["value"]; v = v.get("doubleValue", v.get("int64Value"))
    print("gcp_run_cpu_p95{service=\"%s\"} %s" % (s["resource"]["labels"].get("service_name", "?"), v))'
EOF
dashr collect exec --service gcp --every 60 -- sh /tmp/gcp.sh
dashr collect stream --service run-logs -- gcloud beta run services logs tail <service> --region <region>
```

## A remote host

With SSH access: an `exec` adapter running `ssh host 'cat /proc/loadavg; free -b; df -P'`
and printing gauges, or `stream` of `ssh host journalctl -f -u <unit>`.

## When data is not available

Say what is missing and what would provide it (metrics-server on a cluster,
`pg_stat_statements` on Postgres, OpenTelemetry in the app pointed at the
session's OTLP endpoint from `session_info`). Do not change the human's code
or infrastructure without asking.
