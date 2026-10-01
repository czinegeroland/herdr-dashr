# dashr

[![CI](https://github.com/czinegeroland/herdr-dashr/actions/workflows/build-and-test.yml/badge.svg?branch=main)](https://github.com/czinegeroland/herdr-dashr/actions/workflows/build-and-test.yml)
[![End to end](https://github.com/czinegeroland/herdr-dashr/actions/workflows/end-to-end.yml/badge.svg?branch=main)](https://github.com/czinegeroland/herdr-dashr/actions/workflows/end-to-end.yml)
[![npm](https://img.shields.io/npm/v/herdr-dashr.svg)](https://www.npmjs.com/package/herdr-dashr)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)

**End-to-end testing by traces.** Your AI agent instruments the feature you
just built with OpenTelemetry, you browse every span and the code that
makes it (and edit it, right there), and every service's spans — local, AWS X-Ray, Azure, Google Cloud, Jaeger, Zipkin —
meet in one live sequence diagram that the agent checks against the flow
the feature should produce.

No more "tail the logs while I click around": the agent sees the whole run
as one trace and tells you which step failed, where, and why.

![A local checkout request flowing into a Step Functions run with ten Lambdas and an ECS task, one Lambda failing](https://raw.githubusercontent.com/czinegeroland/herdr-dashr/main/docs/images/sequence.png)

## How it works

1. **Instrument.** The agent adds spans at your feature's business steps
   (`reserve stock`, `charge card`) with the data that proves each step did
   the right thing, on top of the language's automatic instrumentation.
2. **Spans and flow.** It lists the spans it added and where each is made,
   and writes the *flow*: the trace the feature should produce — services,
   spans, attributes, expected values. The viewer's **Spans** tab shows
   every span the code has; click one to open its code in a VS Code-style
   editor, change it and save (or jump to VS Code).
3. **Connect.** Local services export straight to the session's Jaeger.
   Remote ones are pulled from wherever they report — `aws xray`,
   `az monitor`, Cloud Trace, a team Jaeger or Tempo — through commands the
   agent writes. A Step Functions run with ten Lambdas in another account
   joins the trace of the request that started it.
4. **Run.** You trigger the feature (or the agent does).
5. **Verdict.** dashr matches the run against the flow: every step `ok`,
   `missing`, `out of order`, `mismatch` (wrong values, retries, a card
   number that must not be recorded), `error` or `slow` — live in your
   browser, as JSON for the agent.

![The Spans tab: every span the code has, grouped by service; clicking one opens its code at the line that makes it, editable and saved from the browser](https://raw.githubusercontent.com/czinegeroland/herdr-dashr/main/docs/images/spans.png)

![The Flow tab: the steps the agent expects, with code locations, and the verdict of a failing run](https://raw.githubusercontent.com/czinegeroland/herdr-dashr/main/docs/images/flow.png)

## Quickstart

You need [Herdr](https://herdr.dev/docs/install/), Docker and Node.js 18+.

```sh
herdr plugin install czinegeroland/herdr-dashr
```

That installs the plugin, the `dashr` command and the agent skill for your
coding agents. Then tell your agent what you built and ask it to test it
end to end. It opens a narrow dashr pane beside the chat: Ctrl-click the
link for the live view.

Without Herdr: `npm install -g herdr-dashr`, then `dashr serve`.

## What's in a session

- A **Jaeger** container per pane, receiving OTLP on `4317` (gRPC) and
  `4318` (HTTP), on loopback, read-only, in memory. Close the pane and every
  span is gone.
- A **live viewer**: sequence diagram, waterfall, flow verdicts, the spans
  inventory with a code editor, sources' health, span details, and a link
  to the same trace in Jaeger's UI. The editor reads and writes only files
  under the repository the agent works in.
- **Pull sources**: `dashr source add aws --format xray -- <command>` runs
  the command every few seconds with the time window in `DASHR_SINCE` /
  `DASHR_UNTIL`. Formats: X-Ray, Application Insights / Log Analytics,
  Cloud Trace, Jaeger, Zipkin, OTLP (JSON or protobuf, Tempo too).
- **Privacy**: personal data and secrets in spans reach the agent only as
  pseudonyms (`<email#1>`, `<card_number#1>`). You see the real values.

## Commands

```text
dashr serve                       run a session without Herdr
dashr wait --session <pane>       wait for a session; print endpoints and OTEL_* env
dashr env                         export OTEL_* for a local service
dashr spans [set catalog.json]    every span the code has; set where each is made
dashr flow set|list|show|arm|wait|rm
dashr source add|list|rm
dashr traces [--since 10m] [--service S] [--attr k=v] [--errors]
dashr trace <id> [--json]
dashr ingest <file|-> [--format F]
dashr status | sessions | doctor
```

Configuration (optional): `config.toml` in `herdr plugin config-dir
herdr-dashr` — `[jaeger]` image, bind address, ports, memory; `[masking]`;
`[sources]` lookback and overlap.

## License

[Apache-2.0](LICENSE)
