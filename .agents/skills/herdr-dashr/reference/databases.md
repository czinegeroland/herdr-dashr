# Database query performance

`dashr db` connects a PostgreSQL or SQL Server database — local, in a
container, on RDS/Aurora, Azure, Cloud SQL, anywhere — to the session's
Grafana and builds the dashboard a DBA opens first: which statements take
the time, which databases carry the load, what sessions wait on, who blocks
whom, which tables are scanned, which indexes are missing or unused. The
pane samples the database every 30 seconds for trends; the tables query it
live on every refresh.

You never see a query text, a row or a password. `dashr db add` answers
with flags and hints only; the table panels are masked for you like every
personal datasource (`panel_status` says `ok` / `empty` / `error`).

## The flow

```bash
dashr db add <name> …                       # connect, check, start sampling
dashr db dashboard <name> | dashr tool apply_dashboard --args-file -
dashr tool panel_status                      # every panel ok or empty
dashr db list                                # collector health
dashr db remove <name>
```

`add` prints `capabilities` and `hints`. **Relay the hints to the human** —
they are the one-time setup a DBA does (enable `pg_stat_statements`, grant
`pg_monitor` or `VIEW SERVER STATE`, turn Query Store on). Never run those
statements yourself: they change the human's server; ask first.

Keeping an existing dashboard: `dashr db dashboard <name> --id-offset 100
--y-offset <rows below yours>` prints panels you can append to your own
dashboard's `panels` before applying.

## Connecting

Never ask the human to paste a password into the chat. Prefer, in order:

1. A command that prints the password — `--password-command`. The pane
   re-runs it every `--refresh-secs` (600), so short-lived tokens work.
2. An environment variable — `--url-env NAME` or `--password-env NAME`.
3. The connection string on stdin — `… | dashr db add <name> --url -`
   (keeps it out of shell history and the process list).

```bash
# A connection string (PostgreSQL URI or key=value, or ADO.NET for SQL Server)
dashr db add orders --url-env ORDERS_DATABASE_URL
printf '%s' "$CONN" | dashr db add orders --url -

# Parts
dashr db add orders --engine postgres --host db.internal --port 5432 \
  --database orders --user dashr_ro --password-env PGPASSWORD
```

A database on this machine's loopback (a tunnel, a local server) is fine:
dashr makes it reachable from the Grafana container itself.

The pane keeps the tunnel, the password and that loopback relay up, so they
need a pane opened with `DASHR_OTEL=1`; `add` returns once the pane's
collector for the database is `ok` (its `collector` field), and the
dashboard's tables work from then on.

### AWS RDS / Aurora PostgreSQL through the AWS CLI

Typical for an ephemeral environment: no connection string, only `aws`.
Discover first:

```bash
aws rds describe-db-instances --query 'DBInstances[].{id:DBInstanceIdentifier,engine:Engine,host:Endpoint.Address,port:Endpoint.Port,public:PubliclyAccessible,iam:IAMDatabaseAuthenticationEnabled,pi:PerformanceInsightsEnabled,secret:MasterUserSecret.SecretArn,db:DBName}'
```

**Password** — pick what the instance supports:

- IAM authentication (`iam: true`; the user needs `GRANT rds_iam TO <user>`):
  ```bash
  --password-command "aws rds generate-db-auth-token --hostname <host> --port 5432 --username <user> --region <region>"
  ```
  The token lives 15 minutes; the default refresh (600 s) renews it in time.
  With a tunnel, `--hostname` is still the RDS endpoint, not 127.0.0.1.
- An RDS-managed master secret (`secret` is set), or any Secrets Manager
  secret:
  ```bash
  --password-command "aws secretsmanager get-secret-value --secret-id <arn> --query SecretString --output text | python3 -c 'import json,sys; print(json.load(sys.stdin)[\"password\"])'"
  ```

**Reachability** — a private instance (`public: false`) needs a tunnel. With
a bastion or any SSM-managed instance in the VPC (`aws ssm
describe-instance-information`), and the Session Manager plugin installed:

```bash
--host 127.0.0.1 --port 15432 \
--tunnel-command "aws ssm start-session --target <instance-id> --document-name AWS-StartPortForwardingSessionToRemoteHost --parameters host=<rds-host>,portNumber=5432,localPortNumber=15432"
```

The pane keeps the tunnel running and restarts it when it drops. `ssh -L
15432:<rds-host>:5432 <bastion> -N` and `kubectl port-forward` work the same
way.

Full example:

```bash
dashr db add ephemeral --engine postgres --database app --user dashr_ro \
  --host 127.0.0.1 --port 15432 \
  --tunnel-command "aws ssm start-session --target i-0abc --document-name AWS-StartPortForwardingSessionToRemoteHost --parameters host=app.cluster-xyz.eu-west-1.rds.amazonaws.com,portNumber=5432,localPortNumber=15432" \
  --password-command "aws rds generate-db-auth-token --hostname app.cluster-xyz.eu-west-1.rds.amazonaws.com --port 5432 --username dashr_ro --region eu-west-1"
```

RDS connections are TLS; the default `sslmode=require` fits. Parameter
group: `shared_preload_libraries` must contain `pg_stat_statements` (the
default on current RDS/Aurora versions), then `CREATE EXTENSION
pg_stat_statements;` once per database.

**No database connection at all?** When Performance Insights is enabled
(`pi: true`), the top SQL by load and the wait breakdown come from the AWS
API alone — an `exec` collector (`reference/collectors.md`):

```bash
dashr collect exec --service rds-pi --every 60 -- sh -c '
aws pi describe-dimension-keys --service-type RDS --identifier <DbiResourceId> \
  --start-time $(date -u -d "-5 min" +%FT%TZ) --end-time $(date -u +%FT%TZ) \
  --metric db.load.avg --group-by "{\"Group\":\"db.sql_tokenized\",\"Limit\":10}" \
  --output json | python3 -c "
import json,sys
for k in json.load(sys.stdin)[\"Keys\"]:
    s = k[\"Dimensions\"][\"db.sql_tokenized.statement\"][:80].replace(\"\\\\\",\"\").replace(\"\\\"\",\"\")
    print(f\"rds_pi_sql_load{{sql=\\\"{s}\\\"}} {k[\"Total\"]}\")"'
```

(`DbiResourceId` is in `describe-db-instances`; group by `db.wait_event` for
waits; `date -d` is GNU date — on macOS use `date -u -v-5M +%FT%TZ`.) Pair it with CloudWatch (`CPUUtilization`, `DatabaseConnections`,
`ReadIOPS`, `WriteIOPS`, `FreeableMemory`) if the session has the CloudWatch
datasource.

### Azure SQL Database / Managed Instance

```bash
az sql server list --query '[].{name:name,host:fullyQualifiedDomainName}'
az sql db list --server <server> --resource-group <rg> --query '[].name'
```

The ADO.NET string from the portal works as is (`Server=tcp:<server>.database.windows.net,1433;Initial Catalog=<db>;User ID=…;Password=…;Encrypt=True;`).
The server firewall must allow this machine (`az sql server firewall-rule
create` — ask the human first). Keep the password in Key Vault:

```bash
--password-command "az keyvault secret show --vault-name <vault> --name <secret> --query value -o tsv"
```

On Azure SQL Database the DMVs cover the connected database only and need
`VIEW DATABASE STATE`; dashr detects it and uses `sys.dm_db_wait_stats` and
`sys.dm_db_resource_stats` (CPU, data IO, log write, memory %). Entra-only
(passwordless) servers are not supported yet: ask for a SQL login with
`VIEW DATABASE STATE`.

Azure Database for PostgreSQL: allow `pg_stat_statements` in
`azure.extensions`, add it to `shared_preload_libraries`, restart, then
`CREATE EXTENSION pg_stat_statements;`. Password from Key Vault as above, or
an Entra token: `--password-command "az account get-access-token
--resource-type oss-rdbms --query accessToken -o tsv"`.

### Google Cloud SQL

`gcloud sql instances list`; connect through the Cloud SQL Auth Proxy as a
tunnel: `--host 127.0.0.1 --port 15432 --tunnel-command "cloud-sql-proxy
<project>:<region>:<instance> --port 15432"`. `pg_stat_statements` is on by
default (`CREATE EXTENSION` once).

## Least-privilege users

```sql
-- PostgreSQL
CREATE ROLE dashr_ro LOGIN PASSWORD '…';  -- or GRANT rds_iam TO dashr_ro;
GRANT pg_monitor TO dashr_ro;

-- SQL Server (on 2022: VIEW SERVER PERFORMANCE STATE)
CREATE LOGIN dashr_ro WITH PASSWORD = '…';
GRANT VIEW SERVER STATE TO dashr_ro;
-- plus, in each database for Query Store and missing indexes:
CREATE USER dashr_ro FOR LOGIN dashr_ro; GRANT VIEW DATABASE STATE TO dashr_ro;

-- Azure SQL Database, in the database
CREATE USER dashr_ro WITH PASSWORD = '…'; GRANT VIEW DATABASE STATE TO dashr_ro;
```

## What the dashboard shows

**PostgreSQL** (13 or later):

| Panel | From | What it tells |
|---|---|---|
| Transactions/s, active and blocked sessions, longest transaction, cache hit ratio, deadlocks | collector | Health at a glance. A long transaction holds back vacuum; hit ratio < 0.99 on OLTP means reads go to disk. |
| Active sessions by wait | collector | `CPU`, `IO`, `Lock`, `LWLock`, `Client`… — what the database waits on. |
| Statement time (ms/s), mean time per call, calls/s, time share per database | collector (pg_stat_statements) | Which statements take the time now, and whether one got slower. |
| Top statements by total time / slowest per call / most disk and temp IO | `pg_stat_statements` | Since the last `pg_stat_statements_reset()`. `pct_of_all_time` is the share of all statement time; `cache_hit_pct` low = reads from disk; `temp_blocks_written` = sorts or hashes spilling (raise `work_mem` or add an index). |
| Share per database | `pg_stat_statements` | The counterpart of SQL Server's per-database view. |
| Running now | `pg_stat_activity` | State, wait, durations, and `blocked_by` pids. |
| Tables: sequential scans and dead rows | `pg_stat_user_tables` | Many rows read by sequential scans on a big table → missing index. High `dead_pct` and an old `last_vacuum` → autovacuum falls behind. |
| Unused indexes | `pg_stat_user_indexes` | Never scanned since stats reset; they still cost every write. |

**SQL Server** (2016 or later, Azure SQL):

| Panel | From | What it tells |
|---|---|---|
| Batch requests/s, compilations/s, blocked processes, page life expectancy, buffer cache hit ratio, user connections | collector (performance counters) | Compilations close to batch requests → no plan reuse (parameterize). Falling PLE → memory pressure. |
| CPU by statement, mean CPU per execution, CPU share by database, waits (ms/s) | collector | Live versions of the tables below. |
| Top 20 statements by CPU / elapsed / logical reads | `sys.dm_exec_query_stats` | Grouped by `query_hash` (one row per query shape); `plans` > 1 = the same query compiled many times. `query_hash` feeds `dashr db plan`. |
| Share per database | plan cache | CPU, elapsed, reads, writes, memory grant share per database. |
| Top waits | `sys.dm_os_wait_stats` | `CXPACKET`/`CXCONSUMER` parallelism, `PAGEIOLATCH_*` disk reads, `LCK_M_*` blocking, `WRITELOG` log disk, `SOS_SCHEDULER_YIELD` CPU, `RESOURCE_SEMAPHORE` memory grants. |
| Read/write latency per file | `sys.dm_io_virtual_file_stats` | > 20 ms reads or > 5 ms log writes is slow storage. |
| Running now | `sys.dm_exec_requests` | Waits, blockers, CPU, reads. |
| Missing index suggestions | missing-index DMVs | The optimizer's wish list — review, don't create blindly. |
| Query Store: slowest queries, last hour | Query Store | Survives restarts and plan-cache evictions; empty when Query Store is off. |

The plan cache and `pg_stat_statements` are cumulative since a restart or
reset; the collector's per-second series show *now*.

### Execution plans

SQL Server: the human sees `query_hash` in the top-statement tables; with
it, `dashr db plan <name> 0x…` saves the cached plan as a `.sqlplan` file
(for SSMS or Azure Data Studio) and prints only its path. The plan contains
the statement's literals: **do not read the file**; tell the human where it
is. PostgreSQL keeps no plans: suggest the human run `EXPLAIN (ANALYZE,
BUFFERS) <statement>` (or enable `auto_explain`).

## When things go wrong

- **`Grafana cannot connect`**: wrong host/port (is the tunnel's local port
  the one in `--host`/`--port`?), a firewall or security group, TLS
  (`--tls disable` for a local server without TLS; SQL Server's self-signed
  certificate needs `TrustServerCertificate=True` or
  `--trust-server-certificate`), or credentials (tell the human; you cannot
  fix them).
- **Collector `password command: …`**: the token command failed — often an
  expired `aws sso login` / `az login`; ask the human to log in again.
- **Statement panels empty on PostgreSQL**: `pg_stat_statements` is missing
  in the connected database (see the hints), or nothing ran yet.
- **`<insufficient privilege>` query texts** (the human sees them): grant
  `pg_monitor`.
- **Tables `error` on Azure SQL Database**: server-scoped views are not
  available there; the dashboard already uses the database-scoped ones if
  `add` detected Azure — re-run `dashr db add` after changing the server.
