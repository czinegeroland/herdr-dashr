---
name: herdr-dashr
description: >-
  End-to-end test a feature by its traces. Instrument the code with
  OpenTelemetry spans, list them with their code locations so the human
  can browse and edit that code in the viewer, collect every service's spans into one live sequence diagram — local services,
  AWS X-Ray (Lambda, Step Functions, ECS), Azure Application Insights,
  Google Cloud Trace, Jaeger, Zipkin — and check the run against the flow
  the feature should produce. Use when the human wants to test, verify or
  "see" a feature end to end, wants to know whether a change works across
  services, asks to trace or log-trail a deployment, or mentions dashr.
metadata:
  generated-by: herdr-dashr
---

# End-to-end testing by traces with dashr

The human finished a feature and wants to know it works — across every
service it touches, in the environment it really runs in — without reading
the code. You make the feature explain itself through traces:

1. **Instrument** the feature with OpenTelemetry spans at its business
   steps, carrying the data that proves each step did the right thing.
2. **List the spans** you added and where each is made (`dashr spans set`):
   the human sees every span in the viewer's Spans tab, opens the code
   that makes it, and may edit it there.
3. **Write the flow**: the trace the feature should produce, step by step.
4. **Connect the traces** of every service involved: local services export
   to the session's Jaeger; remote ones are pulled from where they report
   (X-Ray, Application Insights, Cloud Trace, ...).
5. **The feature runs** — the human triggers it, or you do.
6. **Verdict**: dashr matches the run's trace against the flow and tells
   you each step's outcome; the human watches the sequence diagram live.

dashr masks personal data and secrets in everything it shows you
(`<email#1>`, `<card_number#1>`): equal values keep equal pseudonyms, so you
can still follow an id. Expected values you write are compared against the
real values inside dashr. Never try to read raw span data another way
(Jaeger's API, the viewer's endpoints, `herdr pane read`).

## 1. Open the session

```bash
"${HERDR_BIN_PATH:-herdr}" plugin pane open --plugin herdr-dashr --entrypoint traces \
  --placement split --target-pane "$HERDR_PANE_ID" --direction right --no-focus
dashr wait --session <pane_id from the JSON it printed>
```

The pane starts a Jaeger container (Docker must run; the first start pulls
the image) and prints a viewer link the human Ctrl-clicks. `dashr wait`
prints the OTLP endpoints and the `OTEL_*` environment for local services.
Without Herdr: `dashr serve` in a terminal does the same. `dashr doctor`
checks the prerequisites. If `dashr` is missing: `npm install -g herdr-dashr`.

Tell the human in one line that the trace view is in the pane, then go on.
Closing the pane stops Jaeger and drops every span.

## 2. Check what is needed — and ask the human for what is missing

Before instrumenting, find out where each service of the feature runs and
how its traces can be read. Check, don't assume:

- local processes: will they export OTLP to the session (`dashr env`)?
- AWS: `aws sts get-caller-identity`; is X-Ray tracing on for the Lambdas,
  the state machine, the API? (`aws lambda get-function-configuration
  --query TracingConfig`, `aws stepfunctions describe-state-machine --query
  tracingConfiguration`)
- Azure: `az account show`; the Application Insights resource and its app id;
- Google Cloud: `gcloud auth list`; the project; the Cloud Trace API enabled;
- a remote Jaeger/Tempo/Zipkin: its URL and how to reach it.

When something is missing — a login, an IAM permission, tracing switched
off, an exporter not configured in the deployed environment — **stop and
ask the human**, saying exactly what to run or change. Never log in for
them, change cloud configuration, or deploy without asking.

## 3. Instrument

Read `reference/instrumenting.md`. In short:

- Prefer the language's automatic instrumentation for plumbing (HTTP
  servers and clients, AWS SDK, database drivers, messaging) and the
  platform's tracing (X-Ray active tracing, the ADOT Lambda layer, Azure's
  OpenTelemetry distro). Add **manual spans only at business steps**:
  usually 5-15 for a feature, not one per function.
- Name spans for what happened (`reserve stock`, `charge card`), set
  attributes that prove it (`order.id`, `order.items`, `payment.status`),
  and record failures as error status with a message.
- Never put secrets or card numbers in attributes; dashr masks personal
  data for you, but the backend stores what you send.
- Make sure the trace context crosses every hop (HTTP headers, queue
  message attributes, Step Functions input); a span whose parent never
  arrives shows as a broken arrow from `?`.
- Give the test run an id the spans carry (`test.run`), so its trace can be
  picked out in a shared environment.

Show the human the instrumentation diff only if they ask; the Spans tab
is where they look at it.

## 4. List the spans, write the flow

Run both from the repository's root: it becomes the root the viewer's
editor opens files under.

**The span catalog** — every span you added by hand, and the automatic
ones worth pointing at, with the file and the function (or line) that
makes it:

```json
{"spans": [
  {"service": "orders-api", "span": "reserve stock", "kind": "internal",
   "file": "src/Orders/StockService.cs", "function": "ReserveAsync",
   "why": "one reservation per order line", "attributes": ["order.id", "stock.requested"]}
]}
```

```bash
dashr spans set spans.json     # replaces the catalog; prints how many have a location
dashr spans                    # every span: planned and seen, counts, last attributes (masked)
```

`file` is relative to the repository (`root` in the JSON overrides it);
`line` is optional when `function` is given. Spans carrying OpenTelemetry's
`code.*` attributes are located without a catalog entry. Keep the catalog
up to date when you add, move or rename spans.

**The flow** — the trace the feature should produce (`reference/flows.md`):
every step with its service, span name, expected attributes and values,
and `code` (`src/Orders/StockService.cs:ReserveAsync`) and `why`.

```bash
dashr flow set flow.json
```

Nothing waits for an approval: go on to connect the traces and run. The
human may edit code in the viewer while you work; `dashr status` lists
those edits under `human_edits` (file, lines changed, when). When one
appears, re-read that file before you change it, and rebuild or restart
what runs it.

## 5. Connect every service's traces

- **Local services** export straight to the session's Jaeger: start them
  with the environment from `dashr env` (or `dashr wait`'s `env`). OTLP over
  HTTP (4318) and gRPC (4317) both work.
- **Remote services** (cloud, a deployed test environment): add a pull
  source — a command that prints traces in a format dashr reads, run every
  few seconds with the time window in `DASHR_SINCE`/`DASHR_UNTIL`. Recipes
  for X-Ray, Application Insights, Log Analytics, Cloud Trace, Jaeger,
  Tempo and Zipkin are in `reference/sources.md`.

```bash
dashr source add aws --format xray --every 10 -- sh -c '<recipe>'
dashr source list
```

`source add` tries the command once and reports how many spans and which
services it found — never their values. A failing trial is refused (pass
`--keep-on-error` when the human is about to log in). Spans of one trace
join no matter where they came from: X-Ray ids are W3C trace ids.

## 6. Run and judge

```bash
dashr flow arm checkout            # count only runs from now on
# the human triggers the feature, or you do (curl, a test, a CLI)
dashr flow wait checkout           # exit 0 pass, 1 fail, 4 timeout
```

The verdict lists every step: `ok`, `missing`, `out_of_order`,
`mismatch` (wrong attribute values, wrong count, an expected error that did
not happen), `error`, `slow`, plus unexpected error spans and forbidden
spans, and the run's sequence as text. Report to the human in plain words:
what passed, what failed, where, and your diagnosis. When a step fails,
look at the trace (`dashr trace <id>`) before guessing.

Pulled sources deliver late (X-Ray indexes within seconds to a minute):
`dashr flow wait` waits until the trace has been quiet for the flow's
`settle_secs`.

## Exploring

```bash
dashr status                                  # endpoints, counts, flows, sources
dashr traces --since 10m --attr test.run=r-42 # newest first
dashr traces --errors --service 'orders*'
dashr trace <trace_id>                        # the sequence, masked
dashr trace <trace_id> --json                 # every span, masked
dashr ingest export.json --format zipkin      # one-off import
```

## Rules

- Measure what the feature does; don't flood services with traffic.
- Ask before logging in, changing cloud configuration, enabling tracing,
  deploying, or restarting services.
- Respect the human's edits made in the viewer (`human_edits`): never
  overwrite them; ask when one conflicts with what you meant to do.
- Never read span data around dashr's masking.
