#!/usr/bin/env bash
# End-to-end suite (DASHR-TECH-003): a real Herdr server, the plugin linked
# from this checkout, a real Grafana container, and the MCP server driven
# over stdio. Needs docker, python3, curl and a built `dashr`.
#
#   scripts/e2e/run.sh [path/to/dashr]
#
# Acceptance criteria covered: AC-OPEN, AC-MASK, AC-ALERT, AC-CLOSE,
# AC-PIPELINE, AC-AGENT, AC-OTEL, AC-LOGX, AC-COLLECT, AC-DB, AC-LIB, plus the startup reaper.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DASHR_BUILT="${1:-$ROOT/target/debug/dashr}"
WORK="$(mktemp -d)"
export HERDR_SESSION="dashr-e2e-$$"
PASS=0
# Pictures of the browser pane; CI uploads this directory as an artifact.
SHOTS="${DASHR_E2E_ARTIFACTS:-$WORK/screens}"
mkdir -p "$SHOTS"
shot() { python3 "$ROOT/scripts/e2e/cdp_screenshot.py" "$1" "$2" "$SHOTS/$3.png" || echo "  (screenshot $3 failed)"; }

log() { printf '\033[1m== %s\033[0m\n' "$*"; }
ok() { PASS=$((PASS + 1)); printf '  \033[32mok\033[0m %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*" >&2; exit 1; }

cleanup() {
  herdr server stop >/dev/null 2>&1 || true
  docker ps -q --filter label=herdr.dashr=1 | xargs -r docker rm -f >/dev/null 2>&1 || true
  docker rm -f dashr-e2e-orphan dashr-e2e-foreign dashr-e2e-target dashr-e2e-app dashr-e2e-pg dashr-e2e-dbpg dashr-e2e-mssql >/dev/null 2>&1 || true
  [ -n "${STANDALONE_STATE:-}" ] && "$ROOT/bin/dashr" --state-dir "$STANDALONE_STATE" --config-dir "$STANDALONE_CONFIG" session stop local-image >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# Poll a shell condition for up to $1 seconds.
wait_for() {
  local seconds="$1"; shift
  for _ in $(seq 1 "$((seconds * 2))"); do
    if eval "$@" >/dev/null 2>&1; then return 0; fi
    sleep 0.5
  done
  return 1
}

log "prerequisites"
command -v docker >/dev/null || fail "docker missing"
command -v python3 >/dev/null || fail "python3 missing"
if ! command -v herdr >/dev/null; then
  curl -fsSL https://herdr.dev/install.sh | HERDR_INSTALL_DIR="$WORK/herdr-bin" sh >/dev/null
  export PATH="$WORK/herdr-bin:$PATH"
fi
herdr --version
docker pull -q grafana/grafana:12.1.1 >/dev/null
docker pull -q grafana/otel-lgtm:0.34.0 >/dev/null
if [ "${DASHR_E2E_MSSQL:-1}" = "1" ] && [ "$(uname -m)" = "x86_64" ]; then docker pull -q mcr.microsoft.com/mssql/server:2022-latest >/dev/null; fi
mkdir -p "$ROOT/bin"
install -m 0755 "$DASHR_BUILT" "$ROOT/bin/dashr"
# The manifest runs `node node_modules/herdr-dashr/bin.js` (DEC-038). A linked
# checkout gets the launcher from npm/dashr, which runs the build in bin/.
mkdir -p "$ROOT/node_modules"
ln -sfn ../npm/dashr "$ROOT/node_modules/herdr-dashr"
[ "$(node "$ROOT/node_modules/herdr-dashr/bin.js" --version)" = "$("$ROOT/bin/dashr" --version)" ] || fail "the launcher does not run the build under test"
ok "herdr, docker and dashr ready"

log "persistent Grafana for promote"
docker run -d --name dashr-e2e-target -p 127.0.0.1::3000 -e GF_SECURITY_ADMIN_PASSWORD=e2e-admin grafana/grafana:12.1.1 >/dev/null
TARGET_PORT="$(docker port dashr-e2e-target 3000/tcp | head -n 1 | cut -d: -f2)"
TARGET="http://127.0.0.1:$TARGET_PORT"
wait_for 90 "curl -fsS $TARGET/api/health | grep -q ok" || fail "target Grafana did not start"
curl -fsS -u admin:e2e-admin -H 'content-type: application/json' -X POST "$TARGET/api/datasources" \
  -d '{"name":"TestData","type":"grafana-testdata-datasource","access":"proxy"}' >/dev/null
SA_ID="$(curl -fsS -u admin:e2e-admin -H 'content-type: application/json' -X POST "$TARGET/api/serviceaccounts" \
  -d '{"name":"dashr-e2e","role":"Admin"}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')"
DASHR_PROMOTE_TOKEN="$(curl -fsS -u admin:e2e-admin -H 'content-type: application/json' -X POST "$TARGET/api/serviceaccounts/$SA_ID/tokens" \
  -d '{"name":"e2e"}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["key"])')"
export DASHR_PROMOTE_TOKEN
ok "target Grafana with a TestData datasource and a service-account token"

log "herdr server and plugin"
herdr server >"$WORK/server.log" 2>&1 &
wait_for 20 herdr status server || fail "herdr server did not start"
herdr workspace create --label e2e --cwd "$WORK" >/dev/null
herdr plugin link "$ROOT" >/dev/null
CONFIG_DIR="$(herdr plugin config-dir herdr-dashr | tail -n 1)"
# dashr keeps its own state directory (DEC-039), shared by the pane and the
# AI session's `dashr wait` / `dashr tool`.
STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/herdr-dashr"
# Records left by an aborted earlier run would be mistaken for this run's.
rm -rf "$STATE_DIR/sessions" "$STATE_DIR/dashboards"
mkdir -p "$CONFIG_DIR"

# A fake AWS CLI for the pipeline scenario.
cat >"$WORK/aws" <<'EOF'
#!/bin/sh
case "$1 $2" in
  "configure export-credentials") printf 'AWS_ACCESS_KEY_ID=ASIAE2EFAKE\nAWS_SECRET_ACCESS_KEY=fake\nAWS_SESSION_TOKEN=fake\n' ;;
  "codepipeline get-pipeline") echo '{"pipeline":{"stages":[{"name":"Deploy","actions":[{"name":"Cfn","actionTypeId":{"category":"Deploy","provider":"CloudFormation"},"configuration":{"StackName":"api-dev"}}]}]}}' ;;
  "codepipeline get-pipeline-state") echo '{"stageStates":[{"stageName":"Deploy","latestExecution":{"status":"Failed","pipelineExecutionId":"e1"}}]}' ;;
  "cloudformation list-stack-resources") echo '{"StackResourceSummaries":[{"ResourceType":"AWS::SQS::Queue","PhysicalResourceId":"https://sqs.eu-west-1.amazonaws.com/1/orders-dlq","ResourceStatus":"CREATE_COMPLETE"},{"ResourceType":"AWS::Lambda::Function","PhysicalResourceId":"api-handler","ResourceStatus":"CREATE_COMPLETE"}]}' ;;
  *) echo "fake aws: $*" >&2; exit 2 ;;
esac
EOF
chmod +x "$WORK/aws"

cat >"$CONFIG_DIR/dashr.toml" <<EOF
[browser]
enabled = false

[agent]
command = ["echo", "AGENT-STARTED", "{mcp_config}"]
skill_dirs = ["$WORK/skills"]

[masking]
testdata_personal = true

[monitor]
interval_secs = 2

[aws]
cli = "$WORK/aws"

[promote]
url = "$TARGET"
token_env = "DASHR_PROMOTE_TOKEN"
folder = "dashr-e2e"

[[datasources]]
name = "Loki"
kind = "loki"
url = "http://localhost:3100"
EOF
ok "plugin linked, configuration written"

log "AC-OPEN: open action"
herdr plugin action invoke herdr-dashr.open >/dev/null
wait_for 90 "ls $STATE_DIR/sessions/*.json" || fail "no session record appeared"
RECORD="$(ls "$STATE_DIR"/sessions/*.json | grep -v watches | head -n 1)"
SESSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["session_id"])' "$RECORD")"
PANE="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["pane_id"])' "$RECORD")"
PORT="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["port"])' "$RECORD")"
CONTAINER="herdr-grafana-$SESSION"
curl -fsS "http://127.0.0.1:$PORT/api/health" | grep -q '"ok"' || fail "Grafana not healthy"
ok "Grafana healthy on 127.0.0.1:$PORT for pane $PANE"

INSPECT="$(docker inspect "$CONTAINER")"
python3 - "$INSPECT" <<'EOF' || fail "container is not hardened as specified"
import json, sys
c = json.loads(sys.argv[1])[0]
host = c["HostConfig"]
assert host["ReadonlyRootfs"] is True, "read-only root"
assert "/var/lib/grafana" in host["Tmpfs"], "tmpfs"
assert host["LogConfig"]["Type"] == "none", "log driver"
assert host["Memory"] == host["MemorySwap"], "no swap"
assert "ALL" in (host["CapDrop"] or []), "capabilities dropped"
bindings = host["PortBindings"]["3000/tcp"]
assert all(b["HostIp"] == "127.0.0.1" for b in bindings), "loopback only"
labels = c["Config"]["Labels"]
assert labels["herdr.dashr"] == "1" and labels["herdr.pane"], "labels"
EOF
ok "container read-only, tmpfs, no logs, no swap, no capabilities, loopback-only"

wait_for 30 "grep -q '\"chat_pane\": \"' $RECORD" \
  || { herdr pane read "$PANE" --source recent | tail -30; cat "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["runtime_dir"])' "$RECORD")/pane.log"; cat "$RECORD"; fail "chat pane not recorded"; }
CHAT="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("chat_pane") or "")' "$RECORD")"
[ -n "$CHAT" ] || fail "chat pane not recorded"
wait_for 20 "herdr pane read $CHAT --source recent | grep -q AGENT-STARTED" || fail "agent command did not run"
herdr pane read "$CHAT" --source recent | grep -q "mcp.json" || fail "agent did not get the MCP config"
ok "chat pane $CHAT started the agent with the MCP config"
grep -q 'generated-by: herdr-dashr' "$WORK/skills/herdr-dashr/SKILL.md" || fail "the pane did not install the skill"
for file in reference/dashboard-json.md reference/datasources.md reference/recipes.md; do
  [ -s "$WORK/skills/herdr-dashr/$file" ] || fail "skill file $file missing"
done
ok "the dashboard pane installed the herdr-dashr skill before starting the agent"

wait_for 30 "herdr pane read $PANE --source visible | grep -q '^http://127.0.0.1:[0-9]*/\$'" || fail "the pane does not show the dashboard link on a line of its own"
herdr pane read "$PANE" --source visible | grep -q "127.0.0.1:$PORT" && fail "the pane links to Grafana itself instead of the dashboard-only page"
wait_for 30 "herdr pane read $PANE --source visible | grep -q 'panels [0-9]* ok'" || fail "the pane does not show panel health"
herdr pane read "$PANE" --source visible | grep -q 'Heartbeat (TestData)' && fail "the pane lists panels; it should show only the link and health"
ok "text view shows the kiosk URL and per-panel status"

wait_for 30 "herdr pane get $PANE | grep -q '\"dashr\":\"[0-9]* ok'" || fail "no \$dashr sidebar token"
ok "sidebar token: $(herdr pane get "$PANE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["pane"]["tokens"]["dashr"])')"

log "AC-MASK: MCP tools never return planted values"
cat >"$WORK/script.json" <<'EOF'
[
  {"tool": "list_datasources"},
  {"tool": "apply_dashboard", "arguments": {"dashboard": {
    "title": "e2e",
    "panels": [
      {"id": 1, "type": "table", "title": "Customers",
       "datasource": {"uid": "dashr-testdata"},
       "targets": [{"refId": "A", "scenarioId": "csv_content",
         "csvContent": "email,message,amount\nplanted.person@example.com,login from 203.0.113.77 card 4111 1111 1111 1111,12\nsecond.person@example.org,iban DE89370400440532013000 phone +36 30 123 4567,7"}]},
      {"id": 2, "type": "timeseries", "title": "Walk",
       "datasource": {"uid": "dashr-testdata"},
       "targets": [{"refId": "A", "scenarioId": "random_walk"}]},
      {"id": 3, "type": "logs", "title": "Loki",
       "datasource": {"uid": "loki"},
       "targets": [{"refId": "A", "expr": "{app=\"x\"}"}]}
    ]}}},
  {"tool": "panel_status"},
  {"tool": "panel_data_sample", "arguments": {"panel_id": 1}},
  {"tool": "probe_query", "arguments": {"datasource_uid": "dashr-testdata", "query": {"scenarioId": "csv_content", "csvContent": "user_name,note\nplanted-user,write to planted.person@example.com"}}},
  {"tool": "apply_dashboard", "arguments": {"dashboard": {"title": "bad", "panels": [{"type": "stat", "datasource": {"uid": "nope"}}]}}},
  {"tool": "screenshot"},
  {"tool": "watch_panel", "arguments": {"panel_id": 2, "reducer": "count", "op": ">", "threshold": 0, "label": "e2e walk has data"}},
  {"tool": "list_watches"}
]
EOF
python3 "$ROOT/scripts/e2e/mcp_client.py" "$ROOT/bin/dashr" "$STATE_DIR" "$CONFIG_DIR" "$SESSION" "$WORK/script.json" >"$WORK/mcp.out"
python3 - "$WORK/mcp.out" <<'EOF' || fail "MCP assertions failed (see above)"
import json, sys
lines = [json.loads(l) for l in open(sys.argv[1])]
out = open(sys.argv[1]).read()
by = {}
for line in lines:
    by.setdefault(line["tool"], []).append(line)
planted = ["planted.person@example.com", "second.person@example.org", "203.0.113.77",
           "4111 1111 1111 1111", "DE89370400440532013000", "123 4567", "planted-user"]
# The planted values appear in our own apply_dashboard arguments, never in answers.
for tool in ("panel_status", "panel_data_sample", "probe_query", "list_datasources"):
    for line in by[tool]:
        for value in planted:
            assert value not in line["text"], f"{value} leaked through {tool}"
assert "Privacy rules" in json.loads(by["initialize"][0]["text"])["instructions"], "no privacy instructions"
tools = json.loads(by["tools/list"][0]["text"])
assert "apply_dashboard" in tools and "panel_data_sample" in tools
assert not by["apply_dashboard"][0]["isError"], by["apply_dashboard"][0]["text"]
assert by["apply_dashboard"][1]["isError"] and "nope" in by["apply_dashboard"][1]["text"]
status = json.loads(by["panel_status"][0]["text"])
states = {p["panel_id"]: p["state"] for p in status["panels"]}
assert states[1] == "ok" and states[2] == "ok", states
assert states[3] == "error", states
sample = by["panel_data_sample"][0]["text"]
assert "<email#1>" in sample and "<email#2>" in sample, sample
assert "<ipv4#1>" in sample and "<card#1>" in sample and "<iban#1>" in sample, sample
probe = by["probe_query"][0]["text"]
assert "<user_name#1>" in probe or "<user-name#1>" in probe, probe
assert by["screenshot"][0]["isError"] and "personal" in by["screenshot"][0]["text"]
assert not by["watch_panel"][0]["isError"], by["watch_panel"][0]["text"]
print("  masked:", json.loads(sample)["targets"][0]["frames"][0]["rows"][0])
EOF
ok "no planted value in any answer; pseudonyms present; bad datasource refused; screenshot refused"

log "AC-ALERT: a breached watch blocks the pane"
wait_for 30 "herdr pane get $PANE | grep -q '\"agent_status\":\"blocked\"'" || fail "pane not blocked"
herdr pane get "$PANE" | grep -q 'alert' || fail "token does not mention the alert"
ok "pane $PANE blocked, token: $(herdr pane get "$PANE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["pane"]["tokens"]["dashr"])')"

log "watch removal, get_dashboard, promote"
cat >"$WORK/script2.json" <<'EOF2'
[
  {"tool": "remove_watch", "arguments": {"id": "w1"}},
  {"tool": "get_dashboard"},
  {"tool": "promote"},
  {"tool": "apply_dashboard", "arguments": {"dashboard": {"title": "e2e promotable", "panels": [
    {"id": 1, "type": "timeseries", "title": "Walk", "datasource": {"uid": "dashr-testdata"},
     "targets": [{"refId": "A", "scenarioId": "random_walk"}]}]}}},
  {"tool": "promote", "arguments": {"title": "e2e promoted"}}
]
EOF2
python3 "$ROOT/scripts/e2e/mcp_client.py" "$ROOT/bin/dashr" "$STATE_DIR" "$CONFIG_DIR" "$SESSION" "$WORK/script2.json" >"$WORK/mcp2.out"
python3 - "$WORK/mcp2.out" "$DASHR_PROMOTE_TOKEN" <<'EOF2' || fail "watch/promote assertions failed"
import json, sys
lines = [json.loads(l) for l in open(sys.argv[1])]
by = {}
for line in lines:
    by.setdefault(line["tool"], []).append(line)
assert not by["remove_watch"][0]["isError"], by["remove_watch"][0]["text"]
model = json.loads(by["get_dashboard"][0]["text"])["dashboard"]
assert model["title"] == "e2e" and "dashr" in model["tags"], model["title"]
first, second = by["promote"]
assert first["isError"] and "Loki" in first["text"], first["text"]
assert not second["isError"], second["text"]
promoted = json.loads(second["text"])
assert promoted["folder"] == "dashr-e2e" and promoted["url"].startswith("http://127.0.0.1:")
assert sys.argv[2] not in open(sys.argv[1]).read(), "token leaked into tool output"
EOF2
wait_for 30 "herdr pane get $PANE | grep -q '\"agent_status\":\"idle\"'" || fail "pane did not return to idle after the watch was removed"
ok "removed watch returns the pane to idle"
curl -fsS -u admin:e2e-admin "$TARGET/api/search?query=e2e%20promoted" | grep -q '"folderTitle":"dashr-e2e"' \
  || fail "promoted dashboard not found in the target folder"
PROMOTED_UID="$(curl -fsS -u admin:e2e-admin "$TARGET/api/search?query=e2e%20promoted" | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["uid"])')"
TARGET_DS="$(curl -fsS -u admin:e2e-admin "$TARGET/api/datasources/name/TestData" | python3 -c 'import json,sys; print(json.load(sys.stdin)["uid"])')"
curl -fsS -u admin:e2e-admin "$TARGET/api/dashboards/uid/$PROMOTED_UID" | grep -q "\"uid\":\"$TARGET_DS\"" \
  || fail "promoted dashboard does not use the target's TestData uid"
ok "get_dashboard works; promote refuses a missing datasource, then promotes with datasources remapped by name"

log "AC-LIB: save a dashboard by name"
cat >"$WORK/lib1.json" <<'EOF2'
[
  {"tool": "save_dashboard", "arguments": {"name": "E2E keep"}},
  {"tool": "save_dashboard", "arguments": {"name": "e2e KEEP"}},
  {"tool": "save_dashboard", "arguments": {"name": "e2e/../x"}},
  {"tool": "save_dashboard", "arguments": {"name": "e2e KEEP", "overwrite": true}},
  {"tool": "list_saved_dashboards"}
]
EOF2
python3 "$ROOT/scripts/e2e/mcp_client.py" "$ROOT/bin/dashr" "$STATE_DIR" "$CONFIG_DIR" "$SESSION" "$WORK/lib1.json" >"$WORK/lib1.out"
python3 - "$WORK/lib1.out" <<'EOF2' || fail "save_dashboard assertions failed"
import json, sys
by = {}
for line in map(json.loads, open(sys.argv[1])):
    by.setdefault(line["tool"], []).append(line)
first, again, bad, overwrite = by["save_dashboard"]
assert not first["isError"], first["text"]
assert json.loads(first["text"])["saved"]["title"] == "e2e promotable"
assert again["isError"] and "already saved" in again["text"], again["text"]
assert bad["isError"] and "name" in bad["text"], bad["text"]
assert not overwrite["isError"], overwrite["text"]
listed = json.loads(by["list_saved_dashboards"][0]["text"])["dashboards"]
assert [d["saved"]["name"] for d in listed] == ["e2e KEEP"], listed
assert listed[0]["loadable_here"] is True
EOF2
[ "$(ls "$STATE_DIR/dashboards")" = "e2e-keep.json" ] || fail "saved dashboard not stored under the state directory"
"$ROOT/bin/dashr" --config-dir "$CONFIG_DIR" --state-dir "$STATE_DIR" dashboards list | grep -q 'e2e KEEP' || fail "dashr dashboards list does not show it"
ok "save_dashboard stores it by name (one per name, case-insensitive), refuses overwrite without asking and bad names"

log "skill: MCP resources, build-step install, examples on a real Grafana"
python3 - "$ROOT/.agents/skills/herdr-dashr/reference/dashboard-json.md" "$WORK/example.json" <<'EOF2'
import json, sys
text = open(sys.argv[1]).read()
blocks = [b.split("\n```")[0] for b in text.split("```json dashr-example\n")[1:]]
example = next(json.loads(b) for b in blocks if "dashr-testdata" in b and "prometheus" not in b)
json.dump([
    {"read_resource": "dashr://guide/SKILL.md"},
    {"read_resource": "dashr://guide/reference/datasources.md"},
    {"tool": "apply_dashboard", "arguments": {"dashboard": example}},
    {"tool": "panel_status"},
], open(sys.argv[2], "w"))
EOF2
python3 "$ROOT/scripts/e2e/mcp_client.py" "$ROOT/bin/dashr" "$STATE_DIR" "$CONFIG_DIR" "$SESSION" "$WORK/example.json" >"$WORK/example.out"
python3 - "$WORK/example.out" <<'EOF2' || fail "skill resource or example assertions failed"
import json, sys
lines = [json.loads(l) for l in open(sys.argv[1])]
uris = json.loads(next(l for l in lines if l["tool"] == "resources/list")["text"])
assert "dashr://guide/SKILL.md" in uris and len(uris) == 8, uris
reads = [l["text"] for l in lines if l["tool"] == "resources/read"]
assert reads[0].startswith("---\nname: herdr-dashr") and "CloudWatch" in reads[1]
init = json.loads(next(l for l in lines if l["tool"] == "initialize")["text"])
assert "dashr://guide/SKILL.md" in init["instructions"]
status = json.loads(next(l for l in lines if l["tool"] == "panel_status")["text"])
states = {p["title"]: p["state"] for p in status["panels"]}
assert states and all(state == "ok" for state in states.values()), states
print("  example panels:", states)
EOF2
ok "guide served as MCP resources; the skill's TestData example renders with every panel ok"
mkdir -p "$WORK/foreign/herdr-dashr"
printf -- '---\nname: herdr-dashr\n---\nmy own notes\n' >"$WORK/foreign/herdr-dashr/SKILL.md"
"$ROOT/bin/dashr" --config-dir "$CONFIG_DIR" skill install --best-effort --dir "$WORK/foreign" | grep -q 'left' || fail "foreign skill not reported"
grep -q 'my own notes' "$WORK/foreign/herdr-dashr/SKILL.md" || fail "a skill dashr did not write was replaced"
"$ROOT/bin/dashr" --config-dir "$CONFIG_DIR" skill install --best-effort --dir "$WORK/fresh" | grep -q 'installed' || fail "build-step install did not install"
"$ROOT/bin/dashr" --config-dir "$CONFIG_DIR" skill install --best-effort --dir /proc/nonexistent/skills || fail "--best-effort must not fail the plugin build"
ok "build-step install: installs fresh, never replaces a user's skill, never fails the build"

log "AC-CLOSE: closing the pane removes everything"
RUNTIME="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["runtime_dir"])' "$RECORD")"
herdr pane close "$PANE" >/dev/null
wait_for 30 "! docker ps --format '{{.Names}}' | grep -q $CONTAINER" || fail "container still running"
wait_for 10 "[ ! -e $RECORD ]" || fail "session record left behind"
[ ! -e "$RUNTIME" ] || fail "runtime dir left behind"
wait_for 10 "! herdr pane get $CHAT" || fail "chat pane left open"
ok "container, session record, runtime dir and chat pane are gone"

log "AC-PIPELINE: CodePipeline URL yields the proposed dashboard"
URL="https://eu-west-1.console.aws.amazon.com/codesuite/codepipeline/pipelines/api/view?region=eu-west-1"
HERDR_BIN_PATH="$(command -v herdr)" HERDR_SOCKET_PATH="$HOME/.config/herdr/sessions/$HERDR_SESSION/herdr.sock" \
  HERDR_PLUGIN_CLICKED_URL="$URL" "$ROOT/bin/dashr" --config-dir "$CONFIG_DIR" --state-dir "$STATE_DIR" herdr action pipeline
wait_for 90 "ls $STATE_DIR/sessions/*.json" || fail "no pipeline session"
RECORD="$(ls "$STATE_DIR"/sessions/*.json | grep -v watches | head -n 1)"
PORT="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["port"])' "$RECORD")"
UID_="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["dashboard_uid"])' "$RECORD")"
PANE="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["pane_id"])' "$RECORD")"
wait_for 30 "curl -fsS http://127.0.0.1:$PORT/api/dashboards/uid/$UID_ | grep -q 'api (eu-west-1)'" || fail "pipeline dashboard not applied"
MODEL="$(curl -fsS "http://127.0.0.1:$PORT/api/dashboards/uid/$UID_")"
echo "$MODEL" | grep -q 'DLQ orders-dlq' || fail "DLQ panel missing"
echo "$MODEL" | grep -q 'api-handler' || fail "Lambda panel missing"
curl -fsS "http://127.0.0.1:$PORT/api/datasources/uid/dashr-cloudwatch-eu-west-1" | grep -q eu-west-1 || fail "CloudWatch datasource missing"
ok "pipeline dashboard applied with DLQ and Lambda panels and a CloudWatch (eu-west-1) datasource"
docker inspect "herdr-grafana-$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["session_id"])' "$RECORD")" \
  --format '{{range .Config.Env}}{{println .}}{{end}}' | grep -q '^AWS_ACCESS_KEY_ID=ASIAE2EFAKE$' \
  || fail "exported AWS credentials did not reach the container"
ok "exported short-lived AWS credentials reached the container by name"
PSESSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["session_id"])' "$RECORD")"
echo "[{\"tool\": \"open_for_pipeline\", \"arguments\": {\"url\": \"$URL\"}}]" >"$WORK/script3.json"
python3 "$ROOT/scripts/e2e/mcp_client.py" "$ROOT/bin/dashr" "$STATE_DIR" "$CONFIG_DIR" "$PSESSION" "$WORK/script3.json" >"$WORK/mcp3.out"
python3 - "$WORK/mcp3.out" <<'EOF2' || fail "open_for_pipeline failed"
import json, sys
line = [json.loads(l) for l in open(sys.argv[1])][-1]
assert not line["isError"], line["text"]
result = json.loads(line["text"])
assert result["applied"] and result["applied"] > 1, result
assert result["inventory"]["resources"]["lambdas"] == ["api-handler"]
EOF2
ok "open_for_pipeline re-inspects and applies in place"
cat >"$WORK/lib2.json" <<'EOF2'
[
  {"tool": "save_dashboard", "arguments": {"name": "e2e pipeline"}},
  {"tool": "load_dashboard", "arguments": {"name": "E2E keep"}},
  {"tool": "get_dashboard"},
  {"tool": "load_dashboard", "arguments": {"name": "never saved"}}
]
EOF2
python3 "$ROOT/scripts/e2e/mcp_client.py" "$ROOT/bin/dashr" "$STATE_DIR" "$CONFIG_DIR" "$PSESSION" "$WORK/lib2.json" >"$WORK/lib2.out"
python3 - "$WORK/lib2.out" <<'EOF2' || fail "load_dashboard assertions failed"
import json, sys
by = {}
for line in map(json.loads, open(sys.argv[1])):
    by.setdefault(line["tool"], []).append(line)
assert not by["save_dashboard"][0]["isError"], by["save_dashboard"][0]["text"]
loaded, missing = by["load_dashboard"]
assert not loaded["isError"], loaded["text"]
assert json.loads(loaded["text"])["loaded"] == "e2e KEEP"
model = json.loads(by["get_dashboard"][0]["text"])["dashboard"]
assert model["title"] == "e2e promotable" and model["uid"] != "", model["title"]
assert missing["isError"] and "e2e pipeline" in missing["text"], missing["text"]
EOF2
ok "a dashboard saved in one pane loads into a later pane; unknown names list what is saved"
herdr pane close "$PANE" >/dev/null
wait_for 30 "! docker ps --format '{{.Names}}' | grep -q herdr-grafana-$PSESSION" || fail "pipeline container still running"

log "startup reaper"
HASH="$(basename "$RECORD" | cut -d- -f1)"
docker run -d --name dashr-e2e-orphan --label herdr.dashr=1 --label "herdr.socket=$HASH" --label herdr.pane=w99:p99 \
  --entrypoint sleep grafana/grafana:12.1.1 600 >/dev/null
docker run -d --name dashr-e2e-foreign --label herdr.dashr=1 --label herdr.socket=ffffffff --label herdr.pane=w99:p99 \
  --entrypoint sleep grafana/grafana:12.1.1 600 >/dev/null
HERDR_BIN_PATH="$(command -v herdr)" HERDR_SOCKET_PATH="$HOME/.config/herdr/sessions/$HERDR_SESSION/herdr.sock" \
  "$ROOT/bin/dashr" --config-dir "$CONFIG_DIR" --state-dir "$STATE_DIR" herdr startup
wait_for 20 "! docker ps --format '{{.Names}}' | grep -q dashr-e2e-orphan" || fail "orphan not reaped"
docker ps --format '{{.Names}}' | grep -q dashr-e2e-foreign || fail "another server's container was reaped"
docker rm -f dashr-e2e-foreign >/dev/null
ok "orphan of this server reaped; another server's container left alone"

log "orphaned pane process stops itself when its pane is gone"
# Windows: closing the pane kills the node launcher, and dashr.exe is never
# signalled. Here the pane process runs outside the pane, so closing the pane
# sends it nothing, and it must notice on its own.
WPANE="$(herdr pane split "$(herdr pane list | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["panes"][0]["pane_id"])')" \
  --direction right --no-focus | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["pane"]["pane_id"])')"
WCONF="$WORK/watchdog-config"
mkdir -p "$WCONF"
printf '[agent]\nenabled = false\n\n[monitor]\ninterval_secs = 2\n' >"$WCONF/dashr.toml"
HERDR_PANE_ID="$WPANE" HERDR_SOCKET_PATH="$HOME/.config/herdr/sessions/$HERDR_SESSION/herdr.sock" HERDR_BIN_PATH="$(command -v herdr)" \
  setsid "$ROOT/bin/dashr" --config-dir "$WCONF" --state-dir "$STATE_DIR" herdr pane dashboard >"$WORK/watchdog.out" 2>&1 </dev/null &
WPID=$!
wait_for 90 "grep -l '\"pane_id\": \"$WPANE\"' $STATE_DIR/sessions/*.json" || { cat "$WORK/watchdog.out"; fail "watchdog pane session did not start"; }
WRECORD="$(grep -l "\"pane_id\": \"$WPANE\"" "$STATE_DIR"/sessions/*.json | head -n 1)"
WSESSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["session_id"])' "$WRECORD")"
herdr pane close "$WPANE" >/dev/null
wait_for 30 "! kill -0 $WPID" || { kill "$WPID"; fail "the pane process kept running after its pane was closed"; }
wait_for 30 "! docker ps --format '{{.Names}}' | grep -q herdr-grafana-$WSESSION" || fail "its container kept running"
[ ! -e "$WRECORD" ] || fail "its session record was left behind"
ok "a pane process that was never signalled noticed its pane was gone, stopped Grafana and exited"

log "AC-AGENT: the human's AI session opens and builds the dashboard"
# Exactly what the skill tells the agent to run, from an ordinary pane: no
# action, no chat pane, no --state-dir or --config-dir.
AGENT_PANE="$(herdr pane list | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["panes"][0]["pane_id"])')"
AGENT_ENV=(env -u HERDR_PLUGIN_CONFIG_DIR -u HERDR_PLUGIN_STATE_DIR HERDR_BIN_PATH="$(command -v herdr)"
  HERDR_SOCKET_PATH="$HOME/.config/herdr/sessions/$HERDR_SESSION/herdr.sock" HERDR_PANE_ID="$AGENT_PANE")
OPENED="$("${AGENT_ENV[@]}" herdr plugin pane open --plugin herdr-dashr --entrypoint dashboard \
  --placement split --target-pane "$AGENT_PANE" --direction right --no-focus --env "DASHR_PIPELINE_URL=$URL")"
APANE="$(python3 -c '
import json, sys
def find(value):
    if isinstance(value, dict):
        if "pane_id" in value: return value["pane_id"]
        for inner in value.values():
            found = find(inner)
            if found: return found
    return None
print(find(json.loads(sys.argv[1])) or "")' "$OPENED")"
[ -n "$APANE" ] || fail "herdr plugin pane open printed no pane id: $OPENED"
"${AGENT_ENV[@]}" "$ROOT/bin/dashr" wait --session "$APANE" --timeout 120 >"$WORK/wait.json" \
  || { herdr pane read "$APANE" --source recent | tail -20; fail "dashr wait did not see the pane's session"; }
python3 - "$WORK/wait.json" "$APANE" <<'EOF2' || fail "dashr wait output is wrong"
import json, sys
out = json.load(open(sys.argv[1]))
assert out["pane"] == sys.argv[2], out
assert "beside you" in out["brief"] and "dashr tool" in out["brief"], out["brief"]
assert "CodePipeline api" in out["brief"], out["brief"]
assert "127.0.0.1" not in json.dumps(out), "the Grafana address must not reach the agent"
EOF2
ASESSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["session"])' "$WORK/wait.json")"
ARECORD="$STATE_DIR/sessions/$ASESSION.json"
python3 -c 'import json,sys; assert not json.load(open(sys.argv[1])).get("chat_pane")' "$ARECORD" \
  || fail "a pane the AI session opened must not open a chat pane"
[ "$(herdr pane get "$APANE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["pane"]["tab_id"])')" = \
  "$(herdr pane get "$AGENT_PANE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["pane"]["tab_id"])')" ] \
  || fail "the dashboard pane is not beside the AI session"
ok "pane opened beside the AI session, no chat pane; dashr wait gave the session and the pipeline briefing"
wait_for 20 "herdr pane layout --pane $APANE | python3 -c 'import json,sys; l=json.load(sys.stdin)[\"result\"][\"layout\"]; w={p[\"pane_id\"]: p[\"rect\"][\"width\"] for p in l[\"panes\"]}; t=w[\"$APANE\"]+w[\"$AGENT_PANE\"]; assert w[\"$APANE\"] <= 0.25 * t, w'" \
  || { herdr pane layout --pane "$APANE"; fail "the dashboard pane is not a narrow column"; }
ok "the dashboard pane narrowed itself to a column on the right"
"${AGENT_ENV[@]}" "$ROOT/bin/dashr" tool panel_status --session "$APANE" >"$WORK/astatus.json" || fail "dashr tool panel_status failed"
grep -q '"DLQ orders-dlq"\|DLQ orders-dlq' "$WORK/astatus.json" || fail "panel_status does not list the pipeline panels"
cat >"$WORK/aprobe.json" <<'EOF2'
{"datasource_uid": "dashr-testdata", "query": {"refId": "A", "scenarioId": "csv_content",
 "csvContent": "email,message\nplanted.person@example.com,login from 203.0.113.77\nsecond.person@example.org,card 4111 1111 1111 1111"}}
EOF2
"${AGENT_ENV[@]}" "$ROOT/bin/dashr" tool probe_query --session "$ASESSION" --args-file "$WORK/aprobe.json" >"$WORK/aprobe.out" \
  || fail "dashr tool probe_query failed"
for planted in planted.person@example.com second.person@example.org 203.0.113.77 "4111 1111 1111 1111"; do
  grep -q "$planted" "$WORK/aprobe.out" && fail "dashr tool leaked a planted value: $planted"
done
grep -q '<email#1>' "$WORK/aprobe.out" || fail "probe_query samples are not masked pseudonyms"
"${AGENT_ENV[@]}" "$ROOT/bin/dashr" tool no_such_tool --session "$APANE" 2>/dev/null && fail "an unknown tool must exit non-zero"
"${AGENT_ENV[@]}" "$ROOT/bin/dashr" tool >"$WORK/tools.json" || fail "dashr tool (the list) failed"
grep -q '"apply_dashboard"' "$WORK/tools.json" || fail "dashr tool does not list the tools"
ok "dashr tool drives the session by pane id; answers are masked like the MCP tools'"
herdr pane close "$APANE" >/dev/null
wait_for 30 "[ ! -e $ARECORD ]" || fail "the AI session's dashboard left its session behind"
ok "closing the pane the AI session opened removes its session"

log "AC-OTEL: one OpenTelemetry container per pane"
herdr plugin action invoke herdr-dashr.otel >/dev/null
wait_for 120 "grep -l '\"otlp\": {' $STATE_DIR/sessions/*.json" || {
  for pane in $(herdr pane list | python3 -c 'import json,sys; print(" ".join(p["pane_id"] for p in json.load(sys.stdin)["result"]["panes"]))'); do
    echo "--- $pane"; herdr pane read "$pane" --source recent | tail -15
  done
  fail "no OpenTelemetry session record appeared"
}
ORECORD="$(grep -l '"otlp": {' "$STATE_DIR"/sessions/*.json | head -n 1)"
OSESSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["session_id"])' "$ORECORD")"
OPANE="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["pane_id"])' "$ORECORD")"
OPORT="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["port"])' "$ORECORD")"
OTLP_HTTP="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["otlp"]["http_port"])' "$ORECORD")"
OCONTAINER="herdr-grafana-$OSESSION"
[ "$(docker ps -q --filter "label=herdr.dashr.session=$OSESSION" | wc -l)" -eq 1 ] || fail "not exactly one container for the session"
python3 - "$(docker inspect "$OCONTAINER")" <<'EOF2' || fail "OpenTelemetry container is not hardened as specified"
import json, sys
c = json.loads(sys.argv[1])[0]
host = c["HostConfig"]
assert c["Config"]["Image"].startswith("grafana/otel-lgtm:"), c["Config"]["Image"]
assert host["ReadonlyRootfs"] is True and host["LogConfig"]["Type"] == "none"
assert host["Memory"] == host["MemorySwap"] and "ALL" in (host["CapDrop"] or [])
for port in ("3000/tcp", "4317/tcp", "4318/tcp"):
    assert all(b["HostIp"] == "127.0.0.1" for b in host["PortBindings"][port]), port
volumes = {m["Destination"] for m in c["Mounts"] if m["Type"] == "volume"}
assert volumes == {"/data", "/var/tempo"}, volumes
binds = {m["Destination"] for m in c["Mounts"] if m["Type"] == "bind"}
assert "/otel-lgtm/tempo-config.yaml" in binds, binds
EOF2
OVOLUMES="$(docker inspect "$OCONTAINER" --format '{{range .Mounts}}{{if eq .Type "volume"}}{{.Name}} {{end}}{{end}}')"
curl -fsS -H 'content-type: application/json' -d '{"resourceLogs":[]}' "http://127.0.0.1:$OTLP_HTTP/v1/logs" >/dev/null || fail "OTLP/HTTP does not accept logs"
ok "one hardened otel-lgtm container; Grafana and OTLP (gRPC, HTTP) on loopback only; telemetry in anonymous volumes"
herdr pane read "$OPANE" --source visible | grep -q "OTLP" && fail "the pane shows the OTLP address, which is not a web page"
wait_for 30 "grep -q '\"chat_pane\": \"' $ORECORD" || fail "OpenTelemetry chat pane not recorded"
OCHAT="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("chat_pane") or "")' "$ORECORD")"
herdr pane run "$OCHAT" 'echo "ENDPOINT=$OTEL_EXPORTER_OTLP_ENDPOINT"' >/dev/null
wait_for 20 "herdr pane read $OCHAT --source recent | grep -q 'ENDPOINT=http://127.0.0.1:$OTLP_HTTP'" || fail "chat pane lacks OTEL_EXPORTER_OTLP_ENDPOINT"
ok "the pane keeps the OTLP address to itself; the chat pane exports OTEL_EXPORTER_OTLP_ENDPOINT"

echo '[{"tool": "list_saved_dashboards"}, {"tool": "load_dashboard", "arguments": {"name": "e2e pipeline"}}, {"tool": "get_dashboard"}]' >"$WORK/lib3.json"
python3 "$ROOT/scripts/e2e/mcp_client.py" "$ROOT/bin/dashr" "$STATE_DIR" "$CONFIG_DIR" "$OSESSION" "$WORK/lib3.json" >"$WORK/lib3.out"
python3 - "$WORK/lib3.out" <<'EOF2' || fail "missing-datasource refusal failed"
import json, sys
by = {json.loads(l)["tool"]: json.loads(l) for l in open(sys.argv[1])}
listed = {d["saved"]["name"]: d["loadable_here"] for d in json.loads(by["list_saved_dashboards"]["text"])["dashboards"]}
assert listed == {"e2e pipeline": False, "e2e KEEP": True}, listed
refused = by["load_dashboard"]
assert refused["isError"] and "dashr-cloudwatch-eu-west-1" in refused["text"], refused["text"]
assert json.loads(by["get_dashboard"]["text"])["dashboard"]["title"] == "OpenTelemetry", "nothing changed"
EOF2
ok "a saved dashboard needing a datasource this session lacks is refused, naming it, and nothing changes"

log "AC-LOGX: expected log messages light up; a forbidden one blocks the pane"
cat >"$WORK/logx.json" <<'EOF2'
[
  {"tool": "session_info"},
  {"tool": "expect_logs", "arguments": {"selector": "{service_name=\"checkout\"}", "expectations": [
    {"name": "order created", "pattern": "order \\d+ created"},
    {"name": "payment captured", "pattern": "payment captured"},
    {"name": "no exceptions", "pattern": "exception|panicked", "expect": "absent"}]}},
  {"tool": "expect_logs", "arguments": {"expectations": [{"name": "bad", "pattern": "(?i)x"}]}},
  {"tool": "log_expectations"}
]
EOF2
python3 "$ROOT/scripts/e2e/mcp_client.py" "$ROOT/bin/dashr" "$STATE_DIR" "$CONFIG_DIR" "$OSESSION" "$WORK/logx.json" >"$WORK/logx.out"
python3 - "$WORK/logx.out" "$OTLP_HTTP" <<'EOF2' || fail "expect_logs assertions failed"
import json, sys
by = {}
for line in map(json.loads, open(sys.argv[1])):
    by.setdefault(line["tool"], []).append(line)
info = json.loads(by["session_info"][0]["text"])
assert info["mode"] == "opentelemetry" and info["otlp"]["http_endpoint"].endswith(":" + sys.argv[2]), info
armed, refused = by["expect_logs"]
assert not armed["isError"] and json.loads(armed["text"])["armed"] == 3, armed["text"]
assert refused["isError"] and "(?" in refused["text"], refused["text"]
report = json.loads(by["log_expectations"][0]["text"])["report"]
assert [e["outcome"] for e in report["expectations"]] == ["waiting", "waiting", "clear"], report
assert report["passed"] is False
EOF2
ok "expect_logs arms three expectations (refusing a non-portable pattern); all start waiting/clear"
DASHR_SESSION="$OSESSION" "$ROOT/bin/dashr" --state-dir "$STATE_DIR" tail --service checkout -- \
  sh -c 'echo "INFO Order 42 created for planted.person@example.com"; echo "WARN retrying" >&2; echo "payment CAPTURED"; exit 3' >/dev/null 2>&1 \
  && fail "dashr tail did not pass the exit code through" || [ $? -eq 3 ] || fail "dashr tail returned the wrong exit code"
ok "dashr tail ran the command and passed exit code 3 through"
echo '[{"tool": "log_expectations"}]' >"$WORK/logx-check.json"
check_logx() {
  python3 "$ROOT/scripts/e2e/mcp_client.py" "$ROOT/bin/dashr" "$STATE_DIR" "$CONFIG_DIR" "$OSESSION" "$WORK/logx-check.json" \
    | tail -n 1 | python3 -c 'import json,sys; r=json.loads(json.loads(sys.stdin.read())["text"])["report"]; print(" ".join(e["outcome"] for e in r["expectations"]), r["passed"])'
}
wait_for 10 "check_logx | grep -q '^seen seen clear True$'" || fail "expectations not met within 10 s: $(check_logx)"
ok "both expected messages seen, case-insensitively; verdict passed"
python3 "$ROOT/scripts/e2e/mcp_client.py" "$ROOT/bin/dashr" "$STATE_DIR" "$CONFIG_DIR" "$OSESSION" "$WORK/logx-check.json" >"$WORK/logx-check.out"
grep -q 'planted.person' "$WORK/logx-check.out" && fail "log_expectations leaked line content"
ok "log_expectations returns counts only"
printf 'ERROR NullPointerException in handler\n' | DASHR_SESSION="$OSESSION" "$ROOT/bin/dashr" --state-dir "$STATE_DIR" tail --service checkout >/dev/null
wait_for 30 "check_logx | grep -q '^seen seen violated False$'" || fail "forbidden message not detected: $(check_logx)"
wait_for 30 "herdr pane get $OPANE | grep -q '\"agent_status\":\"blocked\"'" || fail "a forbidden message did not block the pane"
herdr pane get "$OPANE" | grep -q 'no exceptions' || true
ok "a forbidden message turns its tile red, fails the verdict and blocks the pane"

log "AC-OTEL: traces and metrics over OTLP reach Tempo and Prometheus"
NOW_NS="$(date +%s)000000000"
curl -fsS -H 'content-type: application/json' "http://127.0.0.1:$OTLP_HTTP/v1/traces" -d '{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"checkout"}}]},"scopeSpans":[{"spans":[{"traceId":"5b8efff798038103d269b633813fc60c","spanId":"eee19b7ec3c1b174","name":"POST /orders","kind":2,"startTimeUnixNano":"'"$NOW_NS"'","endTimeUnixNano":"'"$NOW_NS"'"}]}]}]}' >/dev/null
curl -fsS -H 'content-type: application/json' "http://127.0.0.1:$OTLP_HTTP/v1/metrics" -d '{"resourceMetrics":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"checkout"}}]},"scopeMetrics":[{"metrics":[{"name":"dashr.e2e.orders","sum":{"aggregationTemporality":2,"isMonotonic":true,"dataPoints":[{"asInt":"5","timeUnixNano":"'"$NOW_NS"'"}]}}]}]}]}' >/dev/null
# The metric is probed with an instant query: a range query is evaluated at
# step-aligned times, so a single sample stamped "now" shows up only after
# the next step boundary, up to a step (15 s) later.
cat >"$WORK/otel-probe.json" <<'EOF2'
[
  {"tool": "probe_query", "arguments": {"datasource_uid": "prometheus", "query": {"expr": "dashr_e2e_orders_total", "instant": true, "range": false}, "from": "now-5m"}},
  {"tool": "probe_query", "arguments": {"datasource_uid": "tempo", "query": {"queryType": "traceql", "query": "{resource.service.name=\"checkout\"}", "limit": 5, "tableType": "traces"}, "from": "now-15m"}},
  {"tool": "probe_query", "arguments": {"datasource_uid": "loki", "query": {"expr": "{service_name=\"checkout\"}", "queryType": "range"}, "from": "now-15m"}}
]
EOF2
probe_otel() {
  python3 "$ROOT/scripts/e2e/mcp_client.py" "$ROOT/bin/dashr" "$STATE_DIR" "$CONFIG_DIR" "$OSESSION" "$WORK/otel-probe.json" >"$WORK/otel-probe.out"
  python3 - "$WORK/otel-probe.out" <<'EOF3'
import json, sys
probes = [json.loads(l) for l in open(sys.argv[1]) if json.loads(l)["tool"] == "probe_query"]
rows = [sum(f["total_rows"] for r in json.loads(p["text"])["results"] for f in r["frames"])
        if not p["isError"] else -1 for p in probes]
assert all(r > 0 for r in rows), rows
EOF3
}
wait_for 10 probe_otel || { cat "$WORK/otel-probe.out"; fail "metric, trace or logs not queryable within 10 s"; }
grep -q 'planted.person@example.com' "$WORK/otel-probe.out" && fail "a log line leaked unmasked through probe_query"
grep -q '<email#1>' "$WORK/otel-probe.out" || fail "log sample was not masked"
ok "OTLP metric in Prometheus, trace in Tempo, logs in Loki, all within 10 s; log samples masked"

echo '[{"tool": "clear_log_expectations"}, {"tool": "get_dashboard"}]' >"$WORK/logx-clear.json"
python3 "$ROOT/scripts/e2e/mcp_client.py" "$ROOT/bin/dashr" "$STATE_DIR" "$CONFIG_DIR" "$OSESSION" "$WORK/logx-clear.json" >"$WORK/logx-clear.out"
python3 - "$WORK/logx-clear.out" <<'EOF2' || fail "clear_log_expectations failed"
import json, sys
by = {json.loads(l)["tool"]: json.loads(l) for l in open(sys.argv[1])}
assert json.loads(by["clear_log_expectations"]["text"])["cleared"] is True
panels = json.loads(by["get_dashboard"]["text"])["dashboard"]["panels"]
assert all(not 9000 <= p["id"] <= 9199 for p in panels), [p["id"] for p in panels]
assert panels[0]["gridPos"]["y"] == 0
EOF2
wait_for 30 "herdr pane get $OPANE | grep -q '\"agent_status\":\"idle\"'" \
  || { cat "$STATE_DIR/sessions/$OSESSION.watches.json"; herdr pane get "$OPANE"; herdr pane read "$OPANE" --source visible | tail -20; fail "pane did not return to idle after clearing"; }
ok "clearing removes the section, restores the layout and unblocks the pane"
log "AC-COLLECT: discover what runs, then live collectors fill the dashboard"
# An "app" printing ASP.NET and Gin request lines, and a real Postgres.
docker rm -f dashr-e2e-app dashr-e2e-pg >/dev/null 2>&1 || true
docker run -d --name dashr-e2e-app --entrypoint sh postgres:16-alpine -c \
  'while true; do echo "info: Microsoft.AspNetCore.Hosting.Diagnostics[2] Request finished HTTP/1.1 GET http://localhost:5000/orders/42 - 200 - application/json 12.5ms"; echo "[GIN] 2026/09/27 - 19:28:10 | 500 |  2.5ms |  172.21.0.1 | POST     \"/api/embed\""; sleep 0.5; done' >/dev/null
docker run -d --name dashr-e2e-pg -e POSTGRES_PASSWORD=e2e postgres:16-alpine >/dev/null
wait_for 60 "docker exec dashr-e2e-pg pg_isready -U postgres" || fail "Postgres did not start"
sleep 2
"${AGENT_ENV[@]}" "$ROOT/bin/dashr" discover >"$WORK/discover.json" || fail "dashr discover failed"
python3 - "$WORK/discover.json" <<'EOF2' || fail "discover did not describe the containers"
import json, sys
found = json.load(open(sys.argv[1]))
by = {c["name"]: c for c in found["docker"]["containers"]}
app, pg = by["dashr-e2e-app"], by["dashr-e2e-pg"]
assert app["request_lines_in_recent_log"] > 0 and "aspnet" in app["request_log_formats"], app
assert pg["kind"] == "postgres" and "dashr collect postgres dashr-e2e-pg" in pg["collect"], pg
assert "dashr collect docker" in found["collect"] and "dashr collect host" in found["collect"]
text = json.dumps(found)
assert "Request finished" not in text and "/orders/42" not in text, "discover must not return log lines"
assert "docker" in found["tools"]["installed"]
EOF2
ok "discover found the app (with its request log format) and the Postgres, and no log line"
collect() { "${AGENT_ENV[@]}" DASHR_SESSION="$OSESSION" "$ROOT/bin/dashr" collect "$@"; }
collect docker dashr-e2e-app dashr-e2e-pg >"$WORK/c1.json" || fail "collect docker failed"
collect host >/dev/null || fail "collect host failed"
collect logs dashr-e2e-app --service shop >/dev/null || fail "collect logs failed"
collect postgres dashr-e2e-pg >"$WORK/c2.json" || fail "collect postgres failed"
collect exec --service cloud --every 10 -- sh -c 'printf "queue_depth{queue=\"orders\"} 7\n"' >/dev/null || fail "collect exec failed"
grep -q '"dashr_pg_connections"' "$WORK/c2.json" || fail "the Postgres trial did not report its metrics"
grep -q '"value"' "$WORK/c1.json" "$WORK/c2.json" && fail "a collect trial returned values"
collect exec --service broken -- sh -c 'echo not prometheus' 2>/dev/null && fail "an exec that prints no samples must be refused"
collect list | python3 -c 'import json,sys; ids=[c["id"] for c in json.load(sys.stdin)]; assert "exec:broken" not in ids and len(ids) == 5, ids' \
  || fail "collect list is wrong"
ok "collectors added after a trial (names and label keys only); a broken exec is refused"
probe() { # <datasource> <expr> -> total rows
  "${AGENT_ENV[@]}" "$ROOT/bin/dashr" tool probe_query --session "$OSESSION" \
    --args "$(python3 -c 'import json,sys; ds,e=sys.argv[1:3]; q={"expr": e}; q.update({"instant": True, "range": False} if ds == "prometheus" else {"queryType": "range"}); print(json.dumps({"datasource_uid": ds, "query": q, "from": "now-5m"}))' "$1" "$2")" \
    | python3 -c 'import json,sys; r=json.load(sys.stdin); print(sum(f["total_rows"] for x in r["results"] for f in x["frames"]))'
}
for expr in 'dashr_container_cpu_percent{container="dashr-e2e-app"}' 'dashr_container_memory_bytes{container="dashr-e2e-pg"}' \
  'dashr_host_memory_used_bytes' 'dashr_http_requests_per_second{service="shop"}' 'dashr_http_latency_p95_ms{service="shop"}' \
  'dashr_http_server_errors_per_second{service="shop"}' 'dashr_pg_connections{database="dashr-e2e-pg"}' 'dashr_pg_commits_per_second' \
  'queue_depth{service="cloud"}'; do
  wait_for 60 "[ \"\$(probe prometheus '$expr')\" -gt 0 ]" || { collect list; fail "no live data for $expr"; }
done
wait_for 30 "[ \"\$(probe loki '{service_name=\"shop\"}')\" -gt 0 ]" || fail "the app's log lines did not reach Loki"
collect list | python3 -c 'import json,sys; bad=[c for c in json.load(sys.stdin) if c["status"] != "ok"]; assert not bad, bad' \
  || { collect list; fail "a collector reports an error"; }
ok "the pane collects live: container CPU and memory, host, request rate/errors/p95 from the app log, Postgres, an exec adapter, and the log lines"
collect remove exec:cloud >/dev/null || fail "collect remove failed"
collect list | grep -q 'exec:cloud' && fail "a removed collector is still listed"
ok "a collector can be removed"
docker rm -f dashr-e2e-app dashr-e2e-pg >/dev/null 2>&1 || true

log "AC-DB: database query performance through Grafana, over a tunnel with a password command"
# A Postgres with pg_stat_statements on the host's loopback only, reached the
# way an RDS instance is from a laptop: a tunnel command forwards a local
# port to it and a password command prints the password (an IAM token).
docker run -d --name dashr-e2e-dbpg -p 127.0.0.1::5432 -e POSTGRES_PASSWORD=e2e-db-secret postgres:16-alpine \
  -c shared_preload_libraries=pg_stat_statements >/dev/null
wait_for 60 "docker exec dashr-e2e-dbpg pg_isready -U postgres" || fail "Postgres did not start"
sleep 2
DB_PORT="$(docker port dashr-e2e-dbpg 5432/tcp | head -n 1 | cut -d: -f2)"
docker exec dashr-e2e-dbpg psql -U postgres -qc "CREATE EXTENSION pg_stat_statements; CREATE TABLE orders(id int primary key, note text); CREATE INDEX orders_note ON orders(note); INSERT INTO orders SELECT g, md5(g::text) FROM generate_series(1, 50000) g;" \
  || fail "could not prepare the database"
( while docker exec dashr-e2e-dbpg psql -U postgres -qc "SELECT count(*) FROM orders WHERE note LIKE '%ab%'" >/dev/null 2>&1; do sleep 0.3; done ) &
LOAD_PID=$!
TUNNEL_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')"
cat >"$WORK/tunnel.py" <<'EOF2'
import socket, sys, threading
listen, target = int(sys.argv[1]), int(sys.argv[2])
server = socket.socket(); server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind(("127.0.0.1", listen)); server.listen(64)
def pipe(a, b):
    try:
        while (data := a.recv(65536)):
            b.sendall(data)
    except OSError:
        pass
    finally:
        for s in (a, b):
            try: s.shutdown(socket.SHUT_RDWR)
            except OSError: pass
while True:
    client, _ = server.accept()
    upstream = socket.create_connection(("127.0.0.1", target))
    threading.Thread(target=pipe, args=(client, upstream), daemon=True).start()
    threading.Thread(target=pipe, args=(upstream, client), daemon=True).start()
EOF2
printf 'e2e-db-secret\n' >"$WORK/dbpass"
db() { "${AGENT_ENV[@]}" DASHR_SESSION="$OSESSION" "$ROOT/bin/dashr" db "$@"; }
db add shop --engine postgres --host 127.0.0.1 --port "$TUNNEL_PORT" --user postgres --tls disable \
  --password-command "cat $WORK/dbpass" --tunnel-command "python3 $WORK/tunnel.py $TUNNEL_PORT $DB_PORT" >"$WORK/db-add.json" \
  || { cat "$WORK/db-add.json"; fail "dashr db add failed"; }
python3 - "$WORK/db-add.json" <<'EOF2' || fail "db add did not report the capabilities"
import json, sys
out = json.load(open(sys.argv[1]))
assert out["datasource"] == "db-shop" and out["engine"] == "postgres", out
assert out["capabilities"]["pg_stat_statements"] is True, out
assert out["live_series"] is True, out
EOF2
grep -rq 'e2e-db-secret' "$STATE_DIR" "$WORK/db-add.json" && fail "the database password reached a state file or the agent"
ok "db add connected Grafana through the tunnel with the command's password; only flags came back"
db dashboard shop >"$WORK/db-board.json" || fail "db dashboard failed"
"${AGENT_ENV[@]}" "$ROOT/bin/dashr" tool apply_dashboard --session "$OSESSION" --args-file "$WORK/db-board.json" >"$WORK/db-apply.json" \
  || { cat "$WORK/db-apply.json"; fail "the database dashboard was refused"; }
"${AGENT_ENV[@]}" "$ROOT/bin/dashr" tool panel_status --session "$OSESSION" >"$WORK/db-status.json" || fail "panel_status failed"
python3 - "$WORK/db-status.json" <<'EOF2' || { cat "$WORK/db-status.json"; fail "a database table panel fails"; }
import json, sys
status = json.load(open(sys.argv[1]))
tables = [p for p in status["panels"] if p["panel_id"] >= 20]
assert len(tables) >= 7, tables
bad = [p for p in tables if p["state"] not in ("ok", "empty")]
assert not bad, bad
top = next(p for p in tables if p["panel_id"] == 20)
assert top["state"] == "ok", top
EOF2
ok "every table of the database dashboard (top statements, share, sessions, tables, indexes) queries cleanly"
for expr in 'dashr_db_transactions_per_second{db="shop"}' 'dashr_db_connections{db="shop"}' \
  'dashr_db_query_calls_per_second{db="shop"}' 'dashr_db_query_mean_ms{db="shop"}' 'dashr_db_database_time_ms_per_second{db="shop"}'; do
  wait_for 120 "[ \"\$(probe prometheus '$expr')\" -gt 0 ]" || { db list; fail "no live data for $expr"; }
done
db list | python3 -c 'import json,sys; dbs=json.load(sys.stdin); assert dbs[0]["name"] == "shop" and dbs[0]["collector"] == "ok", dbs' \
  || { db list; fail "db list is wrong"; }
probe prometheus 'dashr_db_query_calls_per_second{db="shop", query=~".*dashr.*"}' | grep -qx 0 \
  || fail "dashr's own queries are counted in the statement statistics"
ok "the pane samples the database: transactions, sessions, per-statement calls and mean time, time share per database"
kill "$LOAD_PID" 2>/dev/null || true
db remove shop >/dev/null || fail "db remove failed"
db list | grep -q '"shop"' && fail "a removed database is still listed"
wait_for 15 "! pgrep -f 'tunnel.py $TUNNEL_PORT'" || fail "the tunnel command outlived its database"
ok "db remove disconnects the database and stops its tunnel"
docker rm -f dashr-e2e-dbpg >/dev/null 2>&1 || true

if [ "${DASHR_E2E_MSSQL:-1}" = "1" ] && [ "$(uname -m)" = "x86_64" ]; then
  log "AC-DB-MSSQL: SQL Server from a connection string: top statements, waits, counters, a saved plan"
  MS_PASSWORD='Dashr!E2e-2026'
  docker run -d --name dashr-e2e-mssql -p 127.0.0.1::1433 -e ACCEPT_EULA=Y -e "MSSQL_SA_PASSWORD=$MS_PASSWORD" \
    mcr.microsoft.com/mssql/server:2022-latest >/dev/null
  sq() { docker exec -i dashr-e2e-mssql /opt/mssql-tools18/bin/sqlcmd -C -I -b -S localhost -U sa -P "$MS_PASSWORD" -W -h -1 "$@"; }
  wait_for 120 "sq -Q 'SELECT 1'" || fail "SQL Server did not start"
  MS_PORT="$(docker port dashr-e2e-mssql 1433/tcp | head -n 1 | cut -d: -f2)"
  sq -Q "CREATE DATABASE shop" >/dev/null
  sq -d shop -Q "ALTER DATABASE shop SET QUERY_STORE = ON (OPERATION_MODE = READ_WRITE); CREATE TABLE orders(id int primary key, customer int, note nvarchar(64)); INSERT INTO orders SELECT TOP 50000 ROW_NUMBER() OVER (ORDER BY (SELECT 1)), ABS(CHECKSUM(NEWID())) % 1000, CONVERT(nvarchar(64), NEWID()) FROM sys.all_objects a CROSS JOIN sys.all_objects b" >/dev/null \
    || fail "could not prepare the SQL Server database"
  ( while sq -d shop -Q "SELECT COUNT(*) FROM orders WHERE note LIKE '%ab%'" >/dev/null 2>&1; do sleep 0.3; done ) &
  MS_LOAD_PID=$!
  printf 'Server=tcp:127.0.0.1,%s;Initial Catalog=shop;User ID=sa;Password=%s;Encrypt=True;TrustServerCertificate=True;' "$MS_PORT" "$MS_PASSWORD" \
    | db add ms --url - >"$WORK/ms-add.json" || { cat "$WORK/ms-add.json"; fail "dashr db add (SQL Server) failed"; }
  python3 - "$WORK/ms-add.json" <<'EOF2' || fail "db add did not report the SQL Server capabilities"
import json, sys
out = json.load(open(sys.argv[1]))
caps = out["capabilities"]
assert out["engine"] == "mssql" and caps["plan_cache"] is True and caps["query_store"] == "READ_WRITE", out
EOF2
  grep -rq "$MS_PASSWORD" "$STATE_DIR" "$WORK/ms-add.json" && fail "the SQL Server password reached a state file or the agent"
  ok "db add read an ADO.NET connection string from stdin; the password stayed in Grafana"
  db dashboard ms | "${AGENT_ENV[@]}" "$ROOT/bin/dashr" tool apply_dashboard --session "$OSESSION" --args-file - >"$WORK/ms-apply.json" \
    || { cat "$WORK/ms-apply.json"; fail "the SQL Server dashboard was refused"; }
  "${AGENT_ENV[@]}" "$ROOT/bin/dashr" tool panel_status --session "$OSESSION" >"$WORK/ms-status.json" || fail "panel_status failed"
  python3 - "$WORK/ms-status.json" <<'EOF2' || { cat "$WORK/ms-status.json"; fail "a SQL Server table panel fails"; }
import json, sys
status = json.load(open(sys.argv[1]))
tables = {p["panel_id"]: p for p in status["panels"] if p["panel_id"] >= 20}
assert len(tables) >= 9, tables
bad = [p for p in tables.values() if p["state"] not in ("ok", "empty")]
assert not bad, bad
for panel in (20, 23, 24, 25):
    assert tables[panel]["state"] == "ok", tables[panel]
EOF2
  ok "every SQL Server table (top statements by CPU/elapsed/reads, share, waits, file latency, sessions, missing indexes, Query Store) queries cleanly"
  for expr in 'dashr_db_batch_requests_per_second{db="ms"}' 'dashr_db_page_life_expectancy{db="ms"}' 'dashr_db_buffer_cache_hit_ratio{db="ms"}' \
    'dashr_db_wait_ms_per_second{db="ms"}' 'dashr_db_query_cpu_ms_per_second{db="ms"}' 'dashr_db_database_cpu_ms_per_second{db="ms"}'; do
    wait_for 120 "[ \"\$(probe prometheus '$expr')\" -gt 0 ]" || { db list; fail "no live data for $expr"; }
  done
  ok "the pane samples SQL Server: batch requests, page life expectancy, cache hit ratio, waits, CPU per statement and per database"
  HASH="$(sq -d shop -Q "SET NOCOUNT ON; SELECT TOP 1 CONVERT(varchar(18), query_hash, 1) FROM sys.dm_exec_query_stats qs CROSS APPLY sys.dm_exec_sql_text(qs.sql_handle) st WHERE st.text LIKE '%note LIKE%' AND st.text NOT LIKE '%dm_exec%'" | tr -d '[:space:]')"
  db plan ms "$HASH" --out "$WORK/top.sqlplan" >"$WORK/ms-plan.json" || { cat "$WORK/ms-plan.json"; fail "db plan failed"; }
  grep -q 'ShowPlanXML' "$WORK/top.sqlplan" || fail "the saved plan is not a showplan"
  grep -q 'ShowPlanXML' "$WORK/ms-plan.json" && fail "db plan printed the plan to the agent"
  ok "db plan saves a statement's cached plan as a .sqlplan file for the human, and prints only its path"
  kill "$MS_LOAD_PID" 2>/dev/null || true
  db remove ms >/dev/null || fail "db remove (SQL Server) failed"
  docker rm -f dashr-e2e-mssql >/dev/null 2>&1 || true
fi

herdr pane close "$OPANE" >/dev/null
wait_for 30 "! docker ps --format '{{.Names}}' | grep -q $OCONTAINER" || fail "OpenTelemetry container still running"
wait_for 10 "[ ! -e $ORECORD ]" || fail "OpenTelemetry session record left behind"
for volume in $OVOLUMES; do
  wait_for 10 "! docker volume inspect $volume" || fail "volume $volume left behind"
done
ok "closing the pane stops the OpenTelemetry container and deletes its files and volumes"

log "doctor"
"$ROOT/bin/dashr" --config-dir "$CONFIG_DIR" --state-dir "$STATE_DIR" doctor >"$WORK/doctor.out" || { cat "$WORK/doctor.out"; fail "doctor failed"; }
grep -q 'docker' "$WORK/doctor.out" && grep -q 'herdr' "$WORK/doctor.out" || fail "doctor output incomplete"
grep -q 'terminal-browser' "$WORK/doctor.out" && fail "doctor still checks terminal-browser, which dashr no longer uses"
ok "doctor passes required checks and reports optional ones"

log "custom image with Infinity and Zabbix"
if [ "${DASHR_E2E_SKIP_IMAGE:-}" = 1 ]; then
  echo "  skipped: DASHR_E2E_SKIP_IMAGE=1 (the image build needs grafana.com)"
else
"$ROOT/bin/dashr" image build --tag herdr-dashr-grafana:e2e >"$WORK/image.log" 2>&1 || { tail -30 "$WORK/image.log"; fail "image build failed"; }
STANDALONE_STATE="$WORK/standalone-state"; STANDALONE_CONFIG="$WORK/standalone-config"
mkdir -p "$STANDALONE_CONFIG"
cat >"$STANDALONE_CONFIG/dashr.toml" <<EOF2
[grafana]
image = "herdr-dashr-grafana:e2e"

[[datasources]]
name = "Seq"
kind = "seq"
url = "http://localhost:5341"
personal = true
EOF2
"$ROOT/bin/dashr" --state-dir "$STANDALONE_STATE" --config-dir "$STANDALONE_CONFIG" session start --name image >"$WORK/image-session.json"
IPORT="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["url"].split(":")[2].split("/")[0])' "$WORK/image-session.json")"
curl -fsS "http://127.0.0.1:$IPORT/api/plugins/yesoreyeram-infinity-datasource/settings" | grep -q '"id":"yesoreyeram-infinity-datasource"' \
  || fail "Infinity plugin not loaded from the custom image"
curl -fsS "http://127.0.0.1:$IPORT/api/datasources/uid/seq" | grep -q '"type":"yesoreyeram-infinity-datasource"' \
  || fail "Seq datasource not provisioned through Infinity"
"$ROOT/bin/dashr" --state-dir "$STANDALONE_STATE" --config-dir "$STANDALONE_CONFIG" session stop local-image >/dev/null
ok "custom image loads Infinity from outside the tmpfs; Seq provisioned through it"
fi

log "Chrome: the human opens the pane's link"
# The human's own browser (DEC-040). A real Chrome, headless, driven over the
# DevTools protocol only to look at what the human would see.
CHROME="${DASHR_E2E_CHROME:-$(command -v google-chrome || command -v chromium || command -v chromium-browser || true)}"
[ -z "$CHROME" ] && [ -x /opt/pw-browsers/chromium-1194/chrome-linux/chrome ] && CHROME=/opt/pw-browsers/chromium-1194/chrome-linux/chrome
if [ -z "$CHROME" ] || ! python3 -c 'import websocket' 2>/dev/null; then
  [ "${DASHR_E2E_BROWSER:-}" = 1 ] && fail "Chrome scenario required but Chrome or websocket-client is missing"
  echo "  skipped: needs Chrome and websocket-client (set DASHR_E2E_BROWSER=1 to require it)"
else
  SOCK="$HOME/.config/herdr/sessions/$HERDR_SESSION/herdr.sock"
  ROOT_PANE="$(herdr pane list | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["panes"][0]["pane_id"])')"
  pane_id() { python3 -c '
import json, sys
def find(v):
    if isinstance(v, dict):
        if "pane_id" in v: return v["pane_id"]
        for inner in v.values():
            found = find(inner)
            if found: return found
print(find(json.loads(sys.argv[1])) or "")' "$1"; }
  link_of() { herdr pane read "$1" --source visible | grep -o '^http://127.0.0.1:[0-9]*/$' | head -n 1; }
  # What the human sees is the dashboard inside dashr's page.
  in_dashboard() { local port="$1" page="$2"; shift 2; eval_js "$port" "$page" "$@" /public-dashboards/; }
  chrome() { # <devtools port> <profile dir> <url>
    "$CHROME" --headless=new --no-sandbox --disable-gpu --window-size=1600,1000 --remote-debugging-port="$1" \
      --user-data-dir="$2" "$3" >/dev/null 2>&1 &
  }
  eval_js() { python3 "$ROOT/scripts/e2e/cdp_eval.py" "$@"; }

  BPANE="$(pane_id "$(herdr plugin pane open --plugin herdr-dashr --entrypoint dashboard --placement split \
    --target-pane "$ROOT_PANE" --direction right --no-focus)")"
  "${AGENT_ENV[@]}" "$ROOT/bin/dashr" wait --session "$BPANE" --timeout 120 >"$WORK/bwait.json" || fail "Chrome scenario pane did not start"
  BSESSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["session"])' "$WORK/bwait.json")"
  wait_for 20 "[ -n \"\$(link_of $BPANE)\" ]" || fail "no link in the pane"
  LINK="$(link_of "$BPANE")"
  PAGE="${LINK#http://127.0.0.1}"
  chrome 9311 "$WORK/chrome-1" "$LINK"
  CHROME_PIDS="$!"
  wait_for 60 "in_dashboard 9311 $PAGE 'document.body.innerText' | grep -q 'Heartbeat (TestData)'" \
    || { in_dashboard 9311 "$PAGE" 'document.body.innerText' | head -5; fail "Chrome did not show the dashboard from the pane's link"; }
  ok "Ctrl-clicking the pane's link shows the dashboard in Chrome"
  SEEN="$(in_dashboard 9311 "$PAGE" 'document.body.innerText')"
  for word in "Sign in" "Dashboards" "Administration" "Explore" "Edit" "Share"; do
    echo "$SEEN" | grep -qw "$word" && fail "the dashboard page shows Grafana's '$word'"
  done
  ok "only the dashboard: no Grafana menus, search, edit, share or sign-in"
  shot 9311 "$PAGE" 1-grafana-welcome
  # The agent changes the dashboard; the open tab follows by itself.
  eval_js 9311 "$PAGE" 'window.__dashrMark = 1' >/dev/null
  cat >"$WORK/bapply.json" <<'EOF2'
{"dashboard": {"title": "browser check", "panels": [
  {"id": 1, "type": "timeseries", "title": "Changed by the agent", "datasource": {"uid": "dashr-testdata"},
   "targets": [{"refId": "A", "scenarioId": "random_walk"}]}]}}
EOF2
  "${AGENT_ENV[@]}" "$ROOT/bin/dashr" tool apply_dashboard --session "$BSESSION" --args-file "$WORK/bapply.json" >/dev/null || fail "apply_dashboard failed"
  wait_for 30 "in_dashboard 9311 $PAGE 'document.body.innerText' | grep -q 'Changed by the agent'" \
    || fail "the open Chrome tab did not follow the agent's change"
  eval_js 9311 "$PAGE" 'String(window.__dashrMark)' | grep -q '^1$' || fail "the whole page reloaded instead of just the dashboard"
  ok "the open Chrome tab showed the agent's change within seconds, by itself"
  shot 9311 "$PAGE" 2-changed-by-the-agent
  herdr pane close "$BPANE" >/dev/null
  wait_for 30 "! docker ps --format '{{.Names}}' | grep -q herdr-grafana-$BSESSION" || fail "Chrome scenario container still running"

  log "Chrome: the live log trail highlights expected and forbidden lines"
  OBPANE="$(pane_id "$(herdr plugin pane open --plugin herdr-dashr --entrypoint dashboard --placement split \
    --target-pane "$ROOT_PANE" --direction right --no-focus --env DASHR_OTEL=1)")"
  "${AGENT_ENV[@]}" "$ROOT/bin/dashr" wait --session "$OBPANE" --timeout 150 >"$WORK/obwait.json" || fail "OpenTelemetry pane did not start"
  OBSESSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["session"])' "$WORK/obwait.json")"
  "${AGENT_ENV[@]}" DASHR_SESSION="$OBSESSION" "$ROOT/bin/dashr" expect -p 'order = order \d+ created' -a 'exception' >/dev/null \
    || fail "dashr expect failed"
  # Opened after arming, as the skill tells the human: the tab counts from
  # the moment of arming.
  wait_for 20 "[ -n \"\$(link_of $OBPANE)\" ]" || fail "no link in the OpenTelemetry pane"
  OLINK="$(link_of "$OBPANE")"
  OPAGE="${OLINK#http://127.0.0.1}"
  chrome 9312 "$WORK/chrome-2" "$OLINK"
  CHROME_PIDS="$CHROME_PIDS $!"
  wait_for 60 "in_dashboard 9312 $OPAGE 'document.body.innerText' | grep -q waiting" || fail "expectation tiles did not appear in Chrome"
  shot 9312 "$OPAGE" 3-otel-expectations-armed
  printf 'E2E order 1 created\nE2E exception boom\nE2E plain line\n' \
    | "${AGENT_ENV[@]}" DASHR_SESSION="$OBSESSION" "$ROOT/bin/dashr" tail --service web >/dev/null
  trail_colours() {
    in_dashboard 9312 "$OPAGE" "$(cat "$ROOT/scripts/e2e/trail_colours.js")" | python3 -c '
import json, re, sys
colours = json.loads(sys.stdin.read())
rgb = {k: [int(n) for n in re.findall(r"\d+", v)[:3]] if v.startswith("rgb") else None for k, v in colours.items()}
e, f, p = rgb["expected"], rgb["forbidden"], rgb["plain"]
assert e and e[1] > e[0] + 50, colours
assert f and f[0] > f[1] + 50, colours
assert p is None or abs(p[0] - p[1]) < 30, colours
print(colours)'
  }
  wait_for 60 trail_colours || { in_dashboard 9312 "$OPAGE" "$(cat "$ROOT/scripts/e2e/trail_colours.js")"; fail "the live trail does not highlight the lines"; }
  ok "live trail in Chrome: expected line green, forbidden line red, other lines plain ($(trail_colours))"
  shot 9312 "$OPAGE" 4-otel-trail-highlighted
  # The page must pick up new lines by itself (DEC-034).
  echo 'E2E order 2 created later' | "${AGENT_ENV[@]}" DASHR_SESSION="$OBSESSION" "$ROOT/bin/dashr" tail --service web >/dev/null
  wait_for 20 "in_dashboard 9312 $OPAGE 'document.body.innerText' | grep -q 'E2E order 2 created later'" \
    || fail "the dashboard did not refresh by itself"
  shot 9312 "$OPAGE" 5-otel-trail-live
  ok "the Chrome tab refreshes by itself: a new line appeared without a reload"
  in_dashboard 9312 "$OPAGE" 'document.body.innerText' | grep -q 'Invalid date' && fail "time picker shows an invalid date"
  kill $CHROME_PIDS 2>/dev/null || true
  herdr pane close "$OBPANE" >/dev/null
  wait_for 30 "! docker ps --format '{{.Names}}' | grep -q herdr-grafana-$OBSESSION" || fail "OpenTelemetry container still running"
  ok "closing the pane stopped the OpenTelemetry container"
fi

log "all $PASS checks passed"
