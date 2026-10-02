#!/usr/bin/env bash
# End-to-end suite (DASHR-TECH-003): the real `dashr` binary, a real Jaeger
# container, services instrumented with the real OpenTelemetry SDK, a real
# Herdr server with the plugin linked from this checkout, and a real Chrome
# showing the viewer. No mocks.
#
#   scripts/e2e/run.sh [path/to/dashr]
#
# Scenarios: AC-SESSION, AC-OTLP, AC-MASK, AC-FLOW, AC-SOURCE, AC-FORMATS,
# AC-VIEW, AC-SPANS, AC-EXPORT, AC-THEME, AC-HERDR, AC-REAP, AC-DOCTOR and AC-LAUNCHER.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DASHR_BUILT="${1:-$ROOT/target/debug/dashr}"
WORK="$(mktemp -d)"
export HERDR_SESSION="dashr-e2e-$$"
SHOTS="${DASHR_E2E_ARTIFACTS:-$WORK/screens}"
mkdir -p "$SHOTS"
PASS=0
PIDS=()

log() { printf '\033[1m== %s\033[0m\n' "$*"; }
ok() { PASS=$((PASS + 1)); printf '  \033[32mok\033[0m %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*" >&2; exit 1; }

cleanup() {
  for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
  herdr server stop >/dev/null 2>&1 || true
  docker ps -aq --filter label=herdr.dashr=1 | xargs -r docker rm -f >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

wait_for() { # <seconds> <condition...>
  local seconds="$1"; shift
  for _ in $(seq 1 "$((seconds * 2))"); do
    if eval "$@" >/dev/null 2>&1; then return 0; fi
    sleep 0.5
  done
  return 1
}
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }

log "prerequisites"
command -v docker >/dev/null || fail "docker missing"
command -v python3 >/dev/null || fail "python3 missing"
if ! command -v herdr >/dev/null; then
  curl -fsSL https://herdr.dev/install.sh | HERDR_INSTALL_DIR="$WORK/herdr-bin" sh >/dev/null
  export PATH="$WORK/herdr-bin:$PATH"
fi
herdr --version
docker pull -q jaegertracing/jaeger:2.11.0 >/dev/null
python3 -m venv "$WORK/venv"
"$WORK/venv/bin/pip" install -q opentelemetry-sdk opentelemetry-exporter-otlp-proto-http \
  opentelemetry-exporter-otlp-proto-grpc websocket-client >/dev/null
PY="$WORK/venv/bin/python"
mkdir -p "$ROOT/bin"
install -m 0755 "$DASHR_BUILT" "$ROOT/bin/dashr"
mkdir -p "$ROOT/node_modules"
ln -sfn ../npm/dashr "$ROOT/node_modules/herdr-dashr"
[ "$(node "$ROOT/node_modules/herdr-dashr/bin.js" --version)" = "$("$ROOT/bin/dashr" --version)" ] || fail "the npm launcher does not run the build under test"
ok "AC-LAUNCHER: herdr, docker, the OpenTelemetry SDK and dashr ready; the npm launcher runs this build"

STATE="$WORK/state"
CONF="$WORK/config"
mkdir -p "$STATE" "$CONF"
D=("$ROOT/bin/dashr" --state-dir "$STATE" --config-dir "$CONF")
dashr_() { "${D[@]}" "$@"; }

log "AC-SESSION: a session runs a locked-down Jaeger on loopback"
"${D[@]}" serve --session e2e >"$WORK/serve.log" 2>&1 &
SERVE_PID=$!
PIDS+=("$SERVE_PID")
wait_for 120 "grep -q 'viewer' $WORK/serve.log" || { cat "$WORK/serve.log"; fail "dashr serve did not start"; }
"${D[@]}" wait --timeout 30 >"$WORK/wait.json" || fail "dashr wait did not see the session"
OTLP_HTTP="$(json 'd["otlp"]["http"]' <"$WORK/wait.json")"
OTLP_GRPC="$(json 'd["otlp"]["grpc"]' <"$WORK/wait.json")"
JAEGER="$(json 'd["jaeger_ui"]' <"$WORK/wait.json")"
VIEWER="$(grep -o 'http://127.0.0.1:[0-9]*/#[0-9a-f]*' "$WORK/serve.log" | head -n 1)"
API="${VIEWER%%/#*}"
VTOKEN="${VIEWER##*#}"
CONTAINER="$(docker ps --filter label=herdr.dashr.session=e2e --format '{{.Names}}')"
[ -n "$CONTAINER" ] || fail "no Jaeger container labelled with the session"
INSPECT="$(docker inspect "$CONTAINER")"
python3 - "$INSPECT" <<'EOF' || fail "the Jaeger container is not locked down"
import json, sys
c = json.loads(sys.argv[1])[0]
host = c["HostConfig"]
assert host["ReadonlyRootfs"] and host["CapDrop"] == ["ALL"] and "no-new-privileges" in host["SecurityOpt"][0], host
assert host["AutoRemove"] and host["LogConfig"]["Type"] == "none"
for port, bindings in c["NetworkSettings"]["Ports"].items():
    assert all(b["HostIp"] == "127.0.0.1" for b in bindings or []), (port, bindings)
assert "OTEL_TRACES_SAMPLER=always_off" in c["Config"]["Env"]
EOF
RECORD="$STATE/sessions/e2e.json"
[ "$(stat -c %a "$RECORD")" = 600 ] || fail "the session record is readable by others"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$API/api/status")" = 401 ] || fail "the agent API answers without a token"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$API/v/state")" = 401 ] || fail "the viewer API answers without a token"
[ "$(curl -s -o /dev/null -w '%{http_code}' -H "x-dashr-viewer: $VTOKEN" "$API/v/state")" = 200 ] || fail "the viewer token is refused"
ok "Jaeger read-only, no capabilities, no logs, ports on loopback; record owner-only; both APIs need their token"

log "AC-OTLP: services exporting over OTLP/HTTP and OTLP/gRPC make one trace"
export OTEL_EXPORTER_OTLP_ENDPOINT="$OTLP_HTTP" DASHR_OTLP_GRPC="$OTLP_GRPC"
"$PY" "$ROOT/scripts/e2e/shop.py" stock 18181 >"$WORK/stock.log" 2>&1 &
PIDS+=("$!")
"$PY" "$ROOT/scripts/e2e/shop.py" orders 18180 http://127.0.0.1:18181 >"$WORK/orders.log" 2>&1 &
PIDS+=("$!")
wait_for 20 "curl -s -o /dev/null http://127.0.0.1:18180/" || fail "the shop did not start"
drive() { "$PY" "$ROOT/scripts/e2e/shop.py" drive http://127.0.0.1:18180 "$@"; }
TRACE="$(drive r-1 3 ann@example.com | json 'd["trace_id"]')"
wait_for 20 "dashr_ trace $TRACE --json | grep -q 'orders publish'" || fail "the shop's trace did not arrive"
"${D[@]}" trace "$TRACE" --json >"$WORK/trace.json"
python3 - "$WORK/trace.json" <<'EOF' || { cat "$WORK/trace.json"; fail "the trace is not whole"; }
import json, sys
t = json.load(open(sys.argv[1]))
s = t["summary"]
assert set(s["services"]) == {"e2e-driver", "orders-api", "stock-api"}, s
assert s["orphans"] == 0 and s["errors"] == 0 and s["spans"] == 8, s
seq = t["sequence"]
for line in ("e2e-driver -> orders-api: POST /orders", "orders-api -> stock-api: PUT /stock",
             "stock-api -> postgresql: UPDATE stock", "orders-api ~> order-events: orders publish", "orders-api ·: validate order"):
    assert line in seq, (line, seq)
EOF
ok "orders-api (HTTP/protobuf), stock-api (gRPC) and the driver are one trace; calls, self-steps, the database and the queue in the sequence"

log "AC-MASK: personal data never reaches the agent; the human sees it"
"${D[@]}" traces --since 10m >"$WORK/agent-traces.json"
cat "$WORK/trace.json" "$WORK/agent-traces.json" >"$WORK/agent-all.txt"
grep -q 'ann@example.com' "$WORK/agent-all.txt" && fail "an email reached the agent"
grep -q '<customer_email#1>' "$WORK/trace.json" || fail "no pseudonym where the email was"
curl -s -H "x-dashr-viewer: $VTOKEN" "$API/v/trace/$TRACE" | grep -q 'ann@example.com' || fail "the human's viewer does not show the real value"
ok "the agent sees <customer_email#1>; the viewer shows the real address"

log "AC-FLOW: a flow is used as soon as it is set"
cat >"$WORK/checkout.json" <<'EOF'
{"name": "checkout", "description": "An order reserves stock and publishes an event",
 "match": {"attributes": {"test.run": "r-2"}},
 "steps": [
  {"id": "api", "service": "orders-api", "span": "POST /orders", "kind": "server",
   "attributes": {"order.items": 3, "order.id": "re:^o-\\d+$"}, "code": "scripts/e2e/shop.py:orders", "why": "entry point"},
  {"id": "validate", "service": "orders-api", "span": "validate order",
   "attributes": {"customer.email": "re:@example\\.com$", "card.number": "!"}, "why": "no card number recorded"},
  {"id": "reserve", "service": "stock-api", "span": "reserve stock", "count": {"min": 1, "max": 1}, "attributes": {"stock.requested": 3}},
  {"id": "db", "service": "stock-api", "span": "UPDATE stock", "kind": "client"},
  {"id": "publish", "service": "orders-api", "span": "orders publish", "kind": "producer"}
 ],
 "settle_secs": 3}
EOF
"${D[@]}" flow set "$WORK/checkout.json" >"$WORK/set.json" || fail "the flow was refused"
grep -q '"changed": true' "$WORK/set.json" || fail "a new flow is not taken"
grep -qi 'review' "$WORK/set.json" && fail "flow set still talks about a review"
set +e; "${D[@]}" flow wait checkout --timeout 2 >"$WORK/early.json"; code=$?; set -e
[ "$code" = 4 ] && grep -q '"waiting"' "$WORK/early.json" || fail "flow wait before a run did not time out waiting (exit $code)"
set +e; "${D[@]}" flow wait checkout --review --timeout 2 >/dev/null 2>&1; code=$?; set -e
[ "$code" = 2 ] || fail "flow wait --review is still accepted (exit $code)"
"${D[@]}" flow set "$WORK/checkout.json" | grep -q '"changed": false' || fail "resending the same flow re-armed it"
python3 - "$WORK/checkout.json" <<'EOF'
import json, sys
f = json.load(open(sys.argv[1])); f["description"] += " (v2)"
json.dump(f, open(sys.argv[1], "w"))
EOF
"${D[@]}" flow set "$WORK/checkout.json" | grep -q '"changed": true' || fail "a changed flow is not taken"
ok "no review: a flow counts from flow set; an early wait times out (exit 4); --review is gone; the same flow keeps its arming, a changed one is taken"

log "AC-VIEW: the human's browser shows the flow, its verdicts and the sequence"
CHROME="${DASHR_E2E_CHROME:-$(command -v google-chrome || command -v chromium || command -v chromium-browser || true)}"
[ -z "$CHROME" ] && [ -x /opt/pw-browsers/chromium-1194/chrome-linux/chrome ] && CHROME=/opt/pw-browsers/chromium-1194/chrome-linux/chrome
[ -n "$CHROME" ] || fail "no Chrome or Chromium for the viewer scenario"
"$CHROME" --headless=new --no-sandbox --disable-gpu --window-size=1500,1000 --remote-debugging-port=9321 \
  --user-data-dir="$WORK/chrome" "$VIEWER" >/dev/null 2>&1 &
PIDS+=("$!")
js() { "$PY" "$ROOT/scripts/e2e/cdp_eval.py" 9321 127.0.0.1 "$1"; }
shot() { "$PY" "$ROOT/scripts/e2e/cdp_screenshot.py" 9321 127.0.0.1 "$SHOTS/$1.png" || echo "  (screenshot $1 failed)"; }
wait_for 30 "js 'document.querySelectorAll(\"[data-flow]\").length' | grep -q 1" || fail "the viewer shows no flow"
js 'document.querySelector("[data-flow]").click(), 1' >/dev/null
wait_for 10 "js 'document.querySelectorAll(\"table.steps tr\").length' | grep -q 6" || fail "the flow tab does not show the five steps"
js 'document.getElementById("approve") || document.getElementById("changes") ? 1 : 0' | grep -q 0 || fail "the flow tab still offers a review"
shot 1-flow

"${D[@]}" flow arm checkout >/dev/null
drive r-2 3 ann@example.com >/dev/null
set +e; "${D[@]}" flow wait checkout --timeout 60 >"$WORK/pass.json"; code=$?; set -e
[ "$code" = 0 ] || { cat "$WORK/pass.json"; fail "a correct run did not pass (exit $code)"; }
python3 - "$WORK/pass.json" <<'EOF' || fail "the verdict is incomplete"
import json, sys
v = json.load(open(sys.argv[1]))
assert v["status"] == "pass" and v["settled"] and v["summary"] == "5/5 steps ok", v
assert all(s["status"] == "ok" for s in v["steps"]) and "orders-api -> stock-api" in v["sequence"]
EOF
ok "a correct run passes: 5/5 steps, with the sequence for the agent"

"${D[@]}" flow arm checkout >/dev/null
drive r-2 7 ann@example.com 4111111111111111 >/dev/null
set +e; "${D[@]}" flow wait checkout --timeout 60 >"$WORK/fail.json"; code=$?; set -e
[ "$code" = 1 ] || { cat "$WORK/fail.json"; fail "a broken run did not fail (exit $code)"; }
python3 - "$WORK/fail.json" <<'EOF' || { cat "$WORK/fail.json"; fail "the failing verdict does not say why"; }
import json, sys
v = json.load(open(sys.argv[1]))
steps = {s["id"]: s for s in v["steps"]}
assert steps["api"]["problems"][0] == "order.items: expected 3, got 7", steps["api"]
assert steps["validate"]["status"] == "mismatch" and "card.number: must not be recorded" in steps["validate"]["problems"][0]
assert steps["reserve"]["status"] == "error" and any("out of stock" in p for p in steps["reserve"]["problems"])
assert steps["publish"]["status"] == "missing" and len(v["unexpected_errors"]) >= 2
EOF
grep -q 4111111111111111 "$WORK/fail.json" && fail "the card number reached the agent"
ok "a broken run fails with each step's reason: wrong count, a recorded card number (masked), out of stock, no event"

wait_for 10 "js 'document.querySelectorAll(\".badge.b-bad\").length' | grep -qv '^0$'" || fail "the viewer does not show the failure"
shot 2-flow-failed
js 'document.querySelector("[data-tab=sequence]").click(), 1' >/dev/null
sleep 2
js 'document.querySelectorAll("svg g.msg").length' | grep -qv '^0$' || fail "the viewer drew no sequence"
ok "the viewer shows the flow's steps without a review, the failure, and the sequence"

log "AC-SPANS: every span the code has, and its code, editable in the viewer"
REPO="$WORK/repo"
mkdir -p "$REPO/src"
cat >"$REPO/src/stock.py" <<'EOF'
import json


def refund_order(order):
    pass


def reserve_stock(order, items):
    # the span "reserve stock" is made here
    return items
EOF
echo '{"secret": true}' >"$WORK/outside.json"
ln -s "$WORK/outside.json" "$REPO/src/link.json"
cat >"$WORK/catalog.json" <<'EOF'
{"spans": [
  {"service": "stock-api", "span": "reserve stock", "kind": "internal", "file": "src/stock.py", "function": "reserve_stock",
   "why": "one reservation per order", "attributes": ["stock.requested"]},
  {"service": "orders-api", "span": "refund order", "file": "src/stock.py", "function": "refund_order", "why": "planned, not built yet"}
]}
EOF
(cd "$REPO" && dashr_ spans set "$WORK/catalog.json") >"$WORK/spans-set.json" || fail "spans set was refused"
python3 - "$WORK/spans-set.json" "$REPO" <<'EOF' || { cat "$WORK/spans-set.json"; fail "spans set did not take the repository as the code root"; }
import json, os, sys
r = json.load(open(sys.argv[1]))
assert r["spans"] == 2 and r["located"] >= 2 and r["code_root"] == os.path.realpath(sys.argv[2]), r
EOF
"${D[@]}" spans >"$WORK/spans.json"
python3 - "$WORK/spans.json" <<'EOF' || { cat "$WORK/spans.json"; fail "the agent's span inventory is wrong"; }
import json, sys
spans = {(s["service"], s["span"]): s for s in json.load(open(sys.argv[1]))}
reserve = spans[("stock-api", "reserve stock")]
assert reserve["planned"] and reserve["seen"] >= 1 and reserve["located_by"] == "catalog", reserve
refund = spans[("orders-api", "refund order")]
assert refund["planned"] and refund["seen"] == 0, refund
assert spans[("orders-api", "POST /orders")]["located_by"] == "flow", "the flow step's code locates its span"
assert spans[("orders-api", "validate order")]["attributes"]["customer.email"].startswith("<"), "attributes are masked"
EOF
grep -q 'ann@example.com' "$WORK/spans.json" && fail "an email reached the agent through dashr spans"
V=(-H "x-dashr-viewer: $VTOKEN")
LINE="$(curl -s "${V[@]}" "$API/v/spans" | json 'next(s["code"]["line"] for s in d if s["span"] == "reserve stock")')"
[ "$LINE" = 8 ] || fail "the function's line was not found (got $LINE)"
curl -s "${V[@]}" "$API/v/code?path=src/stock.py" >"$WORK/code.json"
[ "$(json 'd["path"]' <"$WORK/code.json")" = src/stock.py ] && [ "$(json 'd["language"]' <"$WORK/code.json")" = python ] || fail "the viewer cannot read the span's file"
for bad in "../outside.json" "src/../../outside.json" "$WORK/outside.json" "/etc/passwd" "src/link.json"; do
  code="$(curl -s -o "$WORK/bad.json" -w '%{http_code}' -G "${V[@]}" "$API/v/code" --data-urlencode "path=$bad")"
  [ "$code" = 403 ] || [ "$code" = 404 ] || fail "$bad: read outside the code root (HTTP $code)"
  grep -q secret "$WORK/bad.json" && fail "$bad: a file outside the code root leaked"
done
[ "$(curl -s -o /dev/null -w '%{http_code}' -X PUT "${V[@]}" "$API/v/code" -d '{"path":"../outside.json","content":"x","version":"0"}')" = 403 ] || fail "a write outside the code root was not refused"
grep -q secret "$WORK/outside.json" || fail "a file outside the code root was changed"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$API/v/code?path=src/stock.py")" = 401 ] || fail "the code is readable without the viewer token"
[ "$(curl -s -o /dev/null -w '%{http_code}' -X PUT "${V[@]}" "$API/v/code" -d '{"path":"src/stock.py","content":"x","version":"stale"}')" = 409 ] || fail "a stale save was not refused"
grep -q 'def reserve_stock' "$REPO/src/stock.py" || fail "a stale save changed the file"
js 'document.querySelector("[data-tab=spans]").click(), 1' >/dev/null
wait_for 15 "js 'document.querySelectorAll(\"#span-list [data-key]\").length' | grep -qv '^0$'" || fail "the Spans tab lists no spans"
js '[...document.querySelectorAll("#span-list [data-key]")].map((e) => e.textContent).join("|")' >"$WORK/span-list.txt"
grep -q 'refund order' "$WORK/span-list.txt" && grep -q 'not seen' "$WORK/span-list.txt" || fail "a planned span that never ran is not listed"
js '[...document.querySelectorAll("#span-list [data-key]")].find((e) => e.textContent.includes("reserve stock")).click(), 1' >/dev/null
wait_for 20 "js 'editor && (editor.getValue ? editor.getValue() : editor.value).includes(\"def reserve_stock\") ? 1 : 0' | grep -q 1" \
  || { js '[monacoState, selectedKey, document.getElementById("code-path").textContent, document.getElementById("editor").innerHTML.slice(0, 300)].join(" | ")'; fail "clicking the span did not open its code"; }
js 'document.getElementById("code-vscode").href' | grep -q "^vscode://file/.*/src/stock.py:8$" || fail "no Open in VS Code link to the span's line"
EDITOR_KIND="$(js 'editor.getValue ? "Monaco" : "plain"')"
shot 4-spans-code
js 'const t = "    # checked in the viewer\n"; if (editor.getValue) { editor.executeEdits("e2e", [{ range: new monaco.Range(10, 1, 10, 1), text: t }]); } else { const l = editor.value.split("\n"); l.splice(9, 0, t.slice(0, -1)); editor.value = l.join("\n"); editor.dispatchEvent(new Event("input")); } 1' >/dev/null
js 'document.dispatchEvent(new KeyboardEvent("keydown", { key: "s", ctrlKey: true, bubbles: true })), 1' >/dev/null
wait_for 10 "grep -q 'checked in the viewer' '$REPO/src/stock.py'" || { js 'document.getElementById("code-state").textContent'; fail "Ctrl+S did not save the file"; }
grep -q 'def reserve_stock' "$REPO/src/stock.py" || fail "the save broke the file"
wait_for 5 "js 'document.getElementById(\"code-state\").textContent' | grep -q saved" || fail "the editor does not say it saved"
"${D[@]}" status | json '[e["file"] for e in d["human_edits"]]' | grep -q 'src/stock.py' || fail "the human's edit is not reported to the agent"
shot 5-spans-saved
ok "spans set makes the repository the code root; the Spans tab lists planned and seen spans, opens the code at the function ($EDITOR_KIND editor) and saves with Ctrl+S; the agent sees the edit; stale saves and paths outside the root are refused"

log "AC-EXPORT: a report to attach to a pull request, masked"
"${D[@]}" export --flow checkout -o "$WORK/report.md" >/dev/null || fail "dashr export (markdown) failed"
grep -q '^## ❌ dashr: `checkout` failed' "$WORK/report.md" && grep -q '^```mermaid' "$WORK/report.md" && grep -q 'sequenceDiagram' "$WORK/report.md" \
  || { cat "$WORK/report.md"; fail "the markdown report lacks the verdict or the diagram"; }
grep -q '| ❌ | api | orders-api | POST /orders |' "$WORK/report.md" || { cat "$WORK/report.md"; fail "the markdown report lacks the steps"; }
"${D[@]}" export --flow checkout --format html -o "$WORK/report.html" >/dev/null || fail "dashr export (html) failed"
grep -q '<!doctype html>' "$WORK/report.html" && grep -q 'class="mermaid"' "$WORK/report.html" || fail "the html report is not a page with the diagram"
"${D[@]}" export --format json >"$WORK/report.json" || fail "dashr export (json) failed"
python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); assert r["masked"] and r["verdict"]["flow"] == "checkout" and r["spans"], r.keys()' "$WORK/report.json" || fail "the json report is incomplete"
grep -lE 'ann@example.com|4111111111111111' "$WORK/report.md" "$WORK/report.html" "$WORK/report.json" && fail "the agent's export leaked personal data"
curl -s "${V[@]}" "$API/v/export?flow=checkout&format=json" | grep -q 'ann@example.com' && fail "the viewer's default export is not masked"
curl -s "${V[@]}" "$API/v/export?flow=checkout&format=json&raw=1" | grep -q 'ann@example.com' || fail "the viewer's raw export does not carry the real values"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$API/v/export?format=md")" = 401 ] || fail "the export is readable without the viewer token"
js 'document.querySelector("[data-tab=flow]").click(), 1' >/dev/null
sleep 1
js 'document.getElementById("export-btn").click(), document.getElementById("export-pop").hidden ? 0 : 1' | grep -q 1 || fail "the export menu does not open"
js 'document.querySelector("[data-export=html]").click(), 1' >/dev/null
wait_for 10 "js 'document.getElementById(\"toast\").textContent' | grep -q 'Saved dashr-checkout.html'" || fail "exporting HTML from the viewer did not save a file"
ok "dashr export writes markdown (verdict table, Mermaid diagram), HTML and JSON, masked; the viewer exports masked by default, raw on request, from its Export menu"

log "AC-THEME: dark mode, a waterfall that tells each span's time"
js 'document.querySelector("#theme [data-theme=dark]").click(), document.documentElement.dataset.theme' | grep -q dark || fail "the dark theme was not applied"
wait_for 5 "js 'getComputedStyle(document.body).backgroundColor' | grep -q 'rgb(13, 15, 20)'" || fail "the page did not turn dark"
js 'localStorage.getItem("dashr-theme")' | grep -q dark || fail "the theme choice is not remembered"
js "selectedTrace = '$TRACE', document.getElementById('follow').checked = false, setTab('waterfall'), 1" >/dev/null
wait_for 10 "js 'document.querySelectorAll(\".wf-row\").length' | grep -qv '^0$'" || fail "the waterfall has no rows"
js 'const r = document.querySelector(".wf-row"), b = r.getBoundingClientRect(); r.dispatchEvent(new MouseEvent("mousemove", { clientX: b.left + b.width / 2, clientY: b.top + 5, bubbles: true })); document.getElementById("tip").hidden ? "hidden" : document.getElementById("tip").textContent' >"$WORK/tip.txt"
grep -q 'start' "$WORK/tip.txt" && grep -qE '[0-9.]+ (ms|s)' "$WORK/tip.txt" || { cat "$WORK/tip.txt"; fail "hovering a waterfall bar does not show its time"; }
shot 6-dark-waterfall
js 'document.querySelector("#theme [data-theme=light]").click(), 1' >/dev/null
wait_for 5 "js 'getComputedStyle(document.body).backgroundColor' | grep -q 'rgb(244, 245, 248)'" || fail "the light theme was not applied"
js 'document.querySelector("#theme [data-theme=auto]").click(), 1' >/dev/null
ok "the theme switch turns the viewer dark or light and remembers it; hovering a span shows how long it ran, its start and end"

log "AC-SOURCE: a pull source joins a Step Functions run (X-Ray) to the local trace"
TRACE3="$(drive r-3 2 ann@example.com | json 'd["trace_id"]')"
wait_for 20 "dashr_ trace $TRACE3 --json | grep -q 'orders publish'" || fail "the third trace did not arrive"
PUBLISH="$("${D[@]}" trace "$TRACE3" --json | json 'next(s["span_id"] for s in d["spans"] if s["name"] == "orders publish")')"
START="$(python3 -c 'import time; print(time.time())')"
"${D[@]}" source add aws --format xray --every 5 -- python3 "$ROOT/scripts/e2e/xray_fixture.py" "$TRACE3" "$PUBLISH" "$START" ChargeCard >"$WORK/source.json" \
  || { cat "$WORK/source.json"; fail "the X-Ray source was refused"; }
python3 - "$WORK/source.json" <<'EOF' || fail "the trial report is wrong"
import json, sys
r = json.load(open(sys.argv[1]))["trial"]
assert r["spans"] == 35 and r["traces"] == 1 and "checkout-machine" in r["services"] and "shipping-task" in r["services"], r
EOF
grep -q 'bob@example.com' "$WORK/source.json" && fail "the trial report leaked a value"
"${D[@]}" trace "$TRACE3" >"$WORK/joined.txt"
for line in "orders-api ~> checkout-machine: orders publish" "checkout-machine -> chargecard-fn" "ERROR: ChargeCard failed for customer <email#1>" \
  "checkout-machine -> shipping-task" "shipping-task -> DynamoDB: PutItem"; do
  grep -qF "$line" "$WORK/joined.txt" || { cat "$WORK/joined.txt"; fail "missing in the joined sequence: $line"; }
done
grep -q 'bob@example.com' "$WORK/joined.txt" && fail "an email in an X-Ray error reached the agent"
wait_for 20 "curl -s '$JAEGER/api/services' | grep -q checkout-machine" || fail "the pulled spans were not sent on to Jaeger"
set +e; "${D[@]}" source add broken -- sh -c 'echo "token expired" >&2; exit 2' >/dev/null 2>"$WORK/broken.err"; code=$?; set -e
[ "$code" = 5 ] && grep -q 'token expired' "$WORK/broken.err" || fail "a failing source was not refused with its reason"
"${D[@]}" source add later --keep-on-error -- sh -c 'exit 2' >/dev/null || fail "--keep-on-error did not keep the source"
"${D[@]}" source list | grep -q '"later"' || fail "the kept source is not listed"
"${D[@]}" source rm later >/dev/null && "${D[@]}" source rm aws >/dev/null
"${D[@]}" source list | grep -q '"aws"' && fail "a removed source is still listed"
ok "Step Functions, ten Lambdas and an ECS task join the OpenTelemetry trace; the failing Lambda is flagged, masked; spans reach Jaeger; broken sources are refused"

js "selectedTrace = '$TRACE3', document.getElementById('follow').checked = false, poll(true), 1" >/dev/null || true
sleep 3
shot 3-joined-sequence

log "AC-FORMATS: Zipkin, Jaeger, Application Insights and Cloud Trace exports import"
cat >"$WORK/zipkin.json" <<'EOF'
[{"traceId": "a1a1a1a1a1a1a1a1", "id": "b1b1b1b1b1b1b1b1", "name": "get /z", "kind": "SERVER", "timestamp": 1700000000000000, "duration": 1000, "localEndpoint": {"serviceName": "zipkin-svc"}}]
EOF
cat >"$WORK/jaeger.json" <<'EOF'
{"data": [{"traceID": "c1c1c1c1c1c1c1c1", "spans": [{"traceID": "c1c1c1c1c1c1c1c1", "spanID": "d1d1d1d1d1d1d1d1", "operationName": "op", "references": [], "startTime": 1700000000000000, "duration": 5, "processID": "p1", "tags": []}], "processes": {"p1": {"serviceName": "jaeger-svc"}}}]}
EOF
cat >"$WORK/azure.json" <<'EOF'
{"tables": [{"name": "PrimaryResult", "columns": [{"name": "itemType"}, {"name": "operation_Id"}, {"name": "id"}, {"name": "name"}, {"name": "timestamp"}, {"name": "duration"}, {"name": "success"}, {"name": "cloud_RoleName"}],
  "rows": [["request", "e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1", "f1f1f1f1f1f1f1f1", "GET /a", "2026-09-28T08:00:00Z", 12, "True", "azure-svc"]]}]}
EOF
cat >"$WORK/gcp.json" <<'EOF'
{"traces": [{"projectId": "p", "traceId": "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a", "spans": [{"spanId": "7", "kind": "RPC_SERVER", "name": "/g", "startTime": "2026-09-28T08:00:00Z", "endTime": "2026-09-28T08:00:01Z", "labels": {"service.name": "gcp-svc"}}]}]}
EOF
for f in zipkin jaeger azure gcp; do
  "${D[@]}" ingest "$WORK/$f.json" --source "$f" >"$WORK/ingest-$f.json" || fail "importing $f failed"
  [ "$(json 'd["spans"]' <"$WORK/ingest-$f.json")" = 1 ] || fail "$f: wrong span count"
done
for service in zipkin-svc jaeger-svc azure-svc gcp-svc; do
  "${D[@]}" traces --service "$service" --limit 1 | grep -q "$service" || fail "no trace for $service"
done
set +e; echo '{"hello": 1}' | "${D[@]}" ingest - >/dev/null 2>"$WORK/unknown.err"; code=$?; set -e
[ "$code" = 5 ] && grep -q 'recognise' "$WORK/unknown.err" || fail "an unknown format was not refused"
ok "four more formats recognised by shape and imported; an unknown one is refused"

log "AC-REAP: a session killed without cleanup is removed at Herdr's next start"
kill -9 "$SERVE_PID"
sleep 1
docker ps --format '{{.Names}}' | grep -q "$CONTAINER" || fail "the killed session's container is already gone (test needs it)"
"${D[@]}" herdr startup >/dev/null
wait_for 20 "! docker ps -a --format '{{.Names}}' | grep -q $CONTAINER" || fail "the startup hook did not remove the orphaned container"
[ ! -e "$RECORD" ] || fail "the startup hook left the dead session's record"
ok "the startup hook removed the orphaned Jaeger and the stale record"

log "AC-HERDR: the AI session opens the trace pane in Herdr; closing it removes everything"
herdr server >"$WORK/herdr.log" 2>&1 &
PIDS+=("$!")
# `herdr status server` exits 0 when not running too: wait for the word.
wait_for 30 "herdr status server | grep -q '^status: running'" || { cat "$WORK/herdr.log"; fail "herdr server did not start"; }
herdr workspace create --label e2e --cwd "$WORK" >/dev/null
herdr plugin link "$ROOT" >/dev/null
HSTATE="${XDG_STATE_HOME:-$HOME/.local/state}/herdr-dashr"
rm -rf "$HSTATE/sessions"
AGENT_PANE="$(herdr pane list | json 'd["result"]["panes"][0]["pane_id"]')"
AGENT_ENV=(env -u HERDR_PLUGIN_CONFIG_DIR -u HERDR_PLUGIN_STATE_DIR HERDR_BIN_PATH="$(command -v herdr)"
  HERDR_SOCKET_PATH="$HOME/.config/herdr/sessions/$HERDR_SESSION/herdr.sock" HERDR_PANE_ID="$AGENT_PANE")
OPENED="$("${AGENT_ENV[@]}" herdr plugin pane open --plugin herdr-dashr --entrypoint traces \
  --placement split --target-pane "$AGENT_PANE" --direction right --no-focus)"
PANE="$(python3 -c '
import json, sys
def find(v):
    if isinstance(v, dict):
        if "pane_id" in v: return v["pane_id"]
        for inner in v.values():
            found = find(inner)
            if found: return found
print(find(json.loads(sys.argv[1])) or "")' "$OPENED")"
[ -n "$PANE" ] || fail "herdr plugin pane open printed no pane id: $OPENED"
"${AGENT_ENV[@]}" "$ROOT/bin/dashr" wait --session "$PANE" --timeout 120 >"$WORK/hwait.json" \
  || { herdr pane read "$PANE" --source recent | tail -20; fail "dashr wait did not see the pane's session"; }
[ "$(json 'd["pane"]' <"$WORK/hwait.json")" = "$PANE" ] || fail "the session is not the pane's"
wait_for 20 "herdr pane read $PANE --source visible | grep -q 'http://127.0.0.1:[0-9]*/#'" || fail "the pane does not show the viewer link"
herdr pane read "$PANE" --source visible | grep -q 'Jaeger' || fail "the pane does not show the Jaeger link"
HSESSION="$(json 'd["session"]' <"$WORK/hwait.json")"
HCONTAINER="dashr-$HSESSION"
"${AGENT_ENV[@]}" "$ROOT/bin/dashr" flow set "$WORK/checkout.json" >/dev/null || fail "the AI session cannot drive the pane's session"
wait_for 15 "herdr pane read $PANE --source visible | grep -q 'checkout: waiting for a run'" || fail "the pane does not show the flow's state"
herdr pane read "$PANE" --source visible | grep -qi 'review' && fail "the pane still mentions a review"
herdr pane read "$PANE" --source visible | grep -Eq '^ *http://127\.0\.0\.1:[0-9]+/#[0-9a-f]{12} *$' \
  || { herdr pane read "$PANE" --source visible; fail "the viewer link is not whole on one line of the pane"; }
ok "the pane opened beside the AI session, shows the whole viewer link on one line, the Jaeger link and the flow's state"
herdr pane close "$PANE" >/dev/null
wait_for 30 "! docker ps -a --format '{{.Names}}' | grep -q $HCONTAINER" || fail "closing the pane left Jaeger running"
wait_for 10 "[ ! -e $HSTATE/sessions/$HSESSION.json ]" || fail "closing the pane left its session record"
ok "closing the pane stops Jaeger (and every span with it) and removes the session"

log "AC-DOCTOR: prerequisite checks"
"${D[@]}" doctor >"$WORK/doctor.out" || { cat "$WORK/doctor.out"; fail "doctor failed"; }
grep -q 'docker' "$WORK/doctor.out" && grep -q 'jaeger image' "$WORK/doctor.out" || fail "doctor output incomplete"
ok "doctor passes the required checks and reports the optional ones"

printf '\n\033[32m%d checks passed\033[0m\n' "$PASS"
