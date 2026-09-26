#!/usr/bin/env bash
# End-to-end suite (DASHR-TECH-003): a real Herdr server, the plugin linked
# from this checkout, a real Grafana container, and the MCP server driven
# over stdio. Needs docker, python3, curl and a built `dashr`.
#
#   scripts/e2e/run.sh [path/to/dashr]
#
# Acceptance criteria covered: AC-OPEN, AC-MASK, AC-ALERT, AC-CLOSE,
# AC-PIPELINE, plus the startup reaper.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DASHR_BUILT="${1:-$ROOT/target/debug/dashr}"
WORK="$(mktemp -d)"
export HERDR_SESSION="dashr-e2e-$$"
PASS=0

log() { printf '\033[1m== %s\033[0m\n' "$*"; }
ok() { PASS=$((PASS + 1)); printf '  \033[32mok\033[0m %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*" >&2; exit 1; }

cleanup() {
  herdr server stop >/dev/null 2>&1 || true
  docker ps -q --filter label=herdr.dashr=1 | xargs -r docker rm -f >/dev/null 2>&1 || true
  docker rm -f dashr-e2e-orphan dashr-e2e-foreign dashr-e2e-target >/dev/null 2>&1 || true
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
mkdir -p "$ROOT/bin"
install -m 0755 "$DASHR_BUILT" "$ROOT/bin/dashr"
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
STATE_DIR="$HOME/.local/state/herdr/plugins/herdr-dashr"
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

wait_for 30 "grep -q '\"chat_pane\": \"' $RECORD" || fail "chat pane not recorded"
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

wait_for 30 "herdr pane read $PANE --source visible | grep -q 'Dashboard: http://127.0.0.1:$PORT/d/'" || fail "text view does not show the dashboard URL"
wait_for 30 "herdr pane read $PANE --source visible | grep -q 'Heartbeat (TestData)'" || fail "text view does not list panels"
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
assert "dashr://guide/SKILL.md" in uris and len(uris) == 4, uris
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

log "doctor"
"$ROOT/bin/dashr" --config-dir "$CONFIG_DIR" --state-dir "$STATE_DIR" doctor >"$WORK/doctor.out" || { cat "$WORK/doctor.out"; fail "doctor failed"; }
grep -q 'docker' "$WORK/doctor.out" && grep -q 'terminal-browser' "$WORK/doctor.out" || fail "doctor output incomplete"
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

log "browser pane: terminal-browser in a kitty-graphics terminal"
if ! command -v terminal-browser >/dev/null || [ "$(id -u)" = 0 ]; then
  [ "${DASHR_E2E_BROWSER:-}" = 1 ] && fail "terminal-browser scenario required but terminal-browser is missing or running as root"
  echo "  skipped: needs terminal-browser and a non-root user (set DASHR_E2E_BROWSER=1 to require it)"
else
  SOCK="$HOME/.config/herdr/sessions/$HERDR_SESSION/herdr.sock"
  ROOT_PANE="$(herdr pane list | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["panes"][0]["pane_id"])')"
  BPANE="$(herdr pane split "$ROOT_PANE" --direction right --no-focus | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["pane"]["pane_id"])')"
  BCONF="$WORK/browser-config"
  mkdir -p "$BCONF"
  cat >"$BCONF/dashr.toml" <<EOF2
[agent]
enabled = false

[monitor]
interval_secs = 2
EOF2
  # The real pane process, in a terminal that answers kitty graphics queries.
  # LANG is left unset on purpose: dashr must supply a usable browser locale.
  env -u LANG -u LC_ALL HERDR_PANE_ID="$BPANE" HERDR_SOCKET_PATH="$SOCK" HERDR_BIN_PATH="$(command -v herdr)" \
    python3 "$ROOT/scripts/e2e/kitty_term.py" "$WORK/term.json" 0 -- \
    "$ROOT/bin/dashr" --config-dir "$BCONF" --state-dir "$STATE_DIR" herdr pane dashboard &
  TERM_PID=$!
  wait_for 90 "grep -l '\"pane_id\": \"$BPANE\"' $STATE_DIR/sessions/*.json" || fail "browser pane session did not start"
  BRECORD="$(grep -l "\"pane_id\": \"$BPANE\"" "$STATE_DIR"/sessions/*.json | head -n 1)"
  BSESSION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["session_id"])' "$BRECORD")"
  BUID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["dashboard_uid"])' "$BRECORD")"
  BRUNTIME="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["runtime_dir"])' "$BRECORD")"
  wait_for 60 "terminal-browser ls --all --json | grep -q '/d/$BUID'" || fail "terminal-browser is not showing the dashboard"
  CDP="$(terminal-browser ls --all --json | python3 -c 'import json,sys; b=[b for b in json.load(sys.stdin)["browsers"] if any("/d/'"$BUID"'" in t["url"] for t in b["tabs"])]; print(b[0]["cdpPort"])')"
  ok "terminal-browser opened the kiosk URL for $BUID"
  [ -d "$BRUNTIME/browser" ] || fail "browser profile is not in the session runtime dir"
  case "$BRUNTIME" in
    /dev/shm/* | "${XDG_RUNTIME_DIR:-/nonexistent}"/*) ;;
    *) [ "$(uname -s)" = Linux ] && fail "runtime dir $BRUNTIME is not memory-backed" ;;
  esac
  ok "browser profile lives in the session runtime dir ($BRUNTIME/browser)"
  wait_for 60 "python3 $ROOT/scripts/e2e/cdp_eval.py $CDP /d/$BUID 'document.body.innerText' | grep -q 'Heartbeat (TestData)'" \
    || { python3 "$ROOT/scripts/e2e/cdp_eval.py" "$CDP" "/d/$BUID" 'document.body.innerText' | head -5; fail "Grafana did not render the dashboard"; }
  python3 "$ROOT/scripts/e2e/cdp_eval.py" "$CDP" "/d/$BUID" 'document.body.innerText' | grep -q 'unexpected error' && fail "Grafana shows an error page"
  ok "Grafana rendered the welcome dashboard in the browser pane"

  mkdir -p "$WORK/browser"
  cat >"$WORK/browser/script.json" <<'EOF2'
[
  {"tool": "apply_dashboard", "arguments": {"dashboard": {"title": "browser check", "panels": [
    {"id": 1, "type": "timeseries", "title": "Reloaded panel", "datasource": {"uid": "dashr-testdata"},
     "targets": [{"refId": "A", "scenarioId": "random_walk"}]}]}}},
  {"tool": "screenshot"}
]
EOF2
  python3 "$ROOT/scripts/e2e/mcp_client.py" "$ROOT/bin/dashr" "$STATE_DIR" "$BCONF" "$BSESSION" "$WORK/browser/script.json" >"$WORK/browser/mcp.out"
  python3 - "$WORK/browser/mcp.out" <<'EOF2' || fail "browser MCP assertions failed"
import json, sys
by = {json.loads(l)["tool"]: json.loads(l) for l in open(sys.argv[1])}
applied = json.loads(by["apply_dashboard"]["text"])
assert applied["browser_reloaded"] is True, applied
assert not by["screenshot"]["isError"], by["screenshot"]["text"]
assert "saved to" in by["screenshot"]["text"], by["screenshot"]["text"]
EOF2
  wait_for 30 "python3 $ROOT/scripts/e2e/cdp_eval.py $CDP /d/$BUID 'document.body.innerText' | grep -q 'Reloaded panel'" \
    || fail "the browser did not reload into the new dashboard"
  ok "apply_dashboard reloaded the browser into the new dashboard"
  SHOT="$WORK/browser/script.json.screenshot.0.png"
  python3 - "$SHOT" <<'EOF2' || fail "screenshot is not a real PNG"
import sys
data = open(sys.argv[1], "rb").read()
assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
assert len(data) > 5000, f"only {len(data)} bytes"
EOF2
  ok "screenshot tool returned a $(wc -c <"$SHOT") byte PNG of a non-personal dashboard"

  kill -TERM "$TERM_PID"
  wait "$TERM_PID" || true
  wait_for 30 "! docker ps --format '{{.Names}}' | grep -q herdr-grafana-$BSESSION" || fail "browser pane container still running"
  [ ! -e "$BRUNTIME" ] || fail "runtime dir (with the browser profile) left behind"
  [ ! -e "$BRECORD" ] || fail "browser pane session record left behind"
  python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); assert r["frames"] > 0 and r["graphics_queries"] > 0, r' "$WORK/term.json" \
    || fail "terminal-browser drew no kitty graphics frames"
  ok "closing the terminal stopped Grafana and deleted the browser profile; $(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["frames"])' "$WORK/term.json") frames were drawn"
fi

log "all $PASS checks passed"
