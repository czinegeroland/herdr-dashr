# dashr Product Requirements Document

## Document control

| Field | Value |
|---|---|
| Product | dashr — end-to-end testing by traces |
| Command | `dashr` |
| Herdr plugin id | `herdr-dashr` |
| npm package | `herdr-dashr` |
| Repository | `czinegeroland/herdr-dashr` |
| Document status | Active |
| PRD version | 2.0.2 |
| Delivery phase | v2.0.2 — trace sessions, pull sources, flows checked against runs, live sequence diagrams, a Spans tab that opens and edits the code behind every span, reports for pull requests, and a light or dark viewer |
| Last updated | 2026-10-02T06:09:26Z |
| Product owner | @czinegeroland |

## Living PRD policy

This document is the authoritative product and delivery specification. Every
pull request updates it in the same pull request:

1. Reference the requirement IDs it implements or changes, or give a concrete
   `No requirement progress:` rationale.
2. Change the rows of every referenced requirement (status and evidence).
3. Update the delivery ledger (section 14).
4. Record new decisions (section 12) and open questions (section 13).
5. Change the `Last updated` timestamp.

The `PRD traceability` workflow enforces all five from the trusted base
revision (`.github/scripts/check-prd-traceability.ps1`). A row may only claim
`Verified` when its evidence cites a file, test, decision or acceptance
criterion in backticks.

### Requirement status values

| Status | Meaning |
|---|---|
| `Approved` | Accepted and ready for implementation. |
| `In progress` | Implementation begun; acceptance not complete. |
| `Implemented` | Code complete and unit tested; end-to-end evidence pending. |
| `Verified` | Acceptance evidence recorded. |
| `Deferred` | Postponed, with the reason recorded. |
| `Rejected` | Excluded, with the reason recorded. |

---

## 1. Executive summary

dashr turns a coding agent into an end-to-end tester that reads traces
instead of logs. When a feature is done, the human's AI session instruments
it with OpenTelemetry spans at its business steps, writes the **flow** the
feature should produce — services, spans, attributes, values, and where in
the code each span is made — and the human reviews that flow in the
browser. Then the feature runs where it really runs: locally, or in an
ephemeral cloud environment spread over many services and accounts. dashr
gathers every service's spans — exported straight to the session's Jaeger,
or pulled from AWS X-Ray, Azure Application Insights, Google Cloud Trace,
Jaeger, Tempo or Zipkin — into one trace, draws it live as a sequence
diagram, and gives the agent a step-by-step verdict against the flow.

Version 2.0 replaces the 1.x product (agent-built Grafana dashboards),
which its owner found of little use after a week; the 1.x delivery history
is kept in section 14.

## 2. Problem statement

The owner develops a feature, deploys the branch to an ephemeral
environment, and asks an AI agent to tail the logs while they trigger the
feature; the agent decides from log lines whether it worked. That fails in
three ways:

1. Logs are fragments. Whether a request crossed the five services it should
   have, in order, with the right data, must be reconstructed by guessing.
2. A distributed feature — a Step Functions state machine with ten Lambdas
   and an ECS task, services in several AWS accounts — reports to several
   backends. Nothing puts its pieces next to each other.
3. Nobody checks that the instrumentation itself is right: the human rarely
   reads the implementation.

## 3. Goals

| ID | Goal |
|---|---|
| G1 | One live trace of a feature run across every service and system it touches, wherever each reports. |
| G2 | The human checks the planned instrumentation (the flow) before the test, without reading code. |
| G3 | The agent gets a precise, step-by-step verdict it can act on, not a log tail to interpret. |
| G4 | The agent decides where traces come from; dashr is not tied to one cloud. |
| G5 | Personal data and secrets in spans never reach the agent. |
| G6 | Nothing left behind: closing the pane drops every span. |

## 4. Non-goals

- A tracing backend for teams or production: the session is disposable.
- Writing instrumentation automatically: the agent does it, the human reviews.
- Metrics and log storage: Jaeger takes traces only.
- Cloud-specific code paths: clouds are reached through their own CLIs, in
  commands the agent writes.

## 5. Personas and use cases

| Persona | Use case |
|---|---|
| Developer (owner) | "I finished the checkout change; test it end to end on my ephemeral env." |
| Developer | "Show me what really happens when an order is placed — all services." |
| AI coding agent | Instrument, write the flow, connect sources, trigger, judge, explain. |

Typical session: the agent opens the trace pane beside itself; adds spans to
the feature; `dashr flow set`; the human approves in the viewer; the agent
adds a pull source for X-Ray in two AWS accounts; `dashr flow arm`; the
human triggers the feature; `dashr flow wait` reports `ChargeCard`'s Lambda
failed with a masked message; the human watches the same trace as a
sequence diagram.

## 6. Architecture

```text
 the human's AI session (any pane)            the dashr pane (Herdr) / `dashr serve`
 ┌───────────────────────────┐   HTTP+token   ┌────────────────────────────────────────────┐
 │ dashr flow/source/trace … │ ─────────────▶ │ session API (masked)   viewer (raw) ◀── browser
 └───────────────────────────┘                │   │ trace store ◀── poller ◀── Jaeger query API
                                              │   │      ▲                      ▲
 local services ── OTLP gRPC/HTTP ────────────┼───┼──────┼──────▶ Jaeger container (loopback,
                                              │   │   pull sources (commands:     read-only,
 AWS X-Ray / Azure / GCP / Jaeger / Zipkin ◀──┼───┼── aws, az, gcloud, curl)      in-memory)
                                              │   └── flows, verdicts, spans ◀▶ code ▲
                                              │        pulled spans ── OTLP/JSON ──┘
                                              └────────────────────────────────────────────┘
```

- `dashr-core`: the span model, converters for every supported format, the
  trace store, flows and verdicts, sequence derivation, the span catalog
  and inventory, masking. No I/O.
- `dashr-runtime`: the Jaeger container, the poller, pull sources, the
  session API and viewer, the code root the viewer's editor works in,
  session records.
- `dashr-herdr`: the plugin manifest and the Herdr CLI wrapper.
- `dashr-cli`: the `dashr` binary: the pane, `serve`, and the agent's commands.

## 7. Scope and milestones

| Milestone | Content | Status |
|---|---|---|
| 2.0 | Everything in section 8 | Delivered |
| 2.0.1 | The pane's link fits on one line; the review step is gone; the Spans tab with a code editor | Delivered |
| 2.0.2 | Light and dark themes and a redesigned viewer; span times on hover; reports for pull requests (Markdown with Mermaid, HTML, JSON) | Delivered |
| Later | A session per test environment shared by a team; traces exported for bug reports; more formats (Datadog, Honeycomb) as converters | Not started |

## 8. Functional requirements

### 8.1 Session (SESSION)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-SESSION-001 | A session runs one Jaeger all-in-one container (in-memory storage) receiving OTLP over gRPC (4317) and HTTP (4318) and serving its UI and query API (16686). It is read-only, has every capability dropped, no-new-privileges, no logging driver, self-tracing off, a memory limit, and its ports are published on `127.0.0.1` unless `[jaeger] bind` says otherwise. The standard ports are used when free, so SDKs need no endpoint setting; free ports otherwise. | Must | Verified | `the_container_is_locked_down_and_on_loopback`; AC-SESSION |
| DASHR-SESSION-002 | A session writes an owner-only record (session id, Herdr pane, API port, agent token, Jaeger ports) to the state directory and removes it when it ends. `dashr` commands find the session by `--session` (id or pane id), `DASHR_SESSION`, or the one that answers. | Must | Verified | `records_are_found_by_id_pane_or_liveness`; AC-SESSION |
| DASHR-SESSION-003 | The session serves, on loopback, an API for `dashr` commands (agent token, masked answers) and a viewer for the human (its own token in the link's fragment, raw values). Neither answers without its token. | Must | Verified | `crates/dashr-runtime/src/api.rs`; AC-SESSION |
| DASHR-SESSION-004 | The pane reads every trace Jaeger received (polling its query API every 1.5 s over a configurable lookback) into the session's trace store. | Must | Verified | `Shared::poll_jaeger`; AC-OTLP |
| DASHR-SESSION-005 | `dashr doctor` checks the configuration, Docker, the Jaeger image, Herdr, and the AWS, Azure and Google Cloud CLIs, and fails only on required checks. | Should | Verified | `crates/dashr-cli/src/doctor.rs`; AC-DOCTOR |
| DASHR-SESSION-006 | Closing the pane (or stopping `dashr serve`) removes the container, and with it every span, and the record. A session killed without cleanup is removed by the Herdr startup hook; on Windows the `pane.closed` hook stops the container. | Must | Verified | `reap`; AC-REAP, AC-HERDR |

### 8.2 Traces (TRACE)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-TRACE-001 | Spans from any source share one model: W3C trace and span ids, service, kind, times, status and message, attributes, resource, events, links, source. | Must | Verified | `crates/dashr-core/src/model.rs` |
| DASHR-TRACE-002 | dashr reads OTLP/JSON (also Tempo's `batches`), OTLP/protobuf, AWS X-Ray (`batch-get-traces` output and segment documents, subsegments as nested spans, `aws`/`remote` subsegments as client calls), Zipkin v2, Jaeger query JSON, Azure Application Insights and Log Analytics query results, and Google Cloud Trace v1, recognising the format by shape when not named. X-Ray, Zipkin and Application Insights ids become W3C ids, so a trace that crosses systems is one trace. | Must | Verified | `crates/dashr-core/src/ingest/`; `a_step_function_execution_becomes_one_trace`; AC-SOURCE, AC-FORMATS |
| DASHR-TRACE-003 | The trace store groups spans by trace, keeps one copy per span (a later copy replaces an earlier one, except that Jaeger's echo of a pulled span never replaces it), counts orphans (spans whose parent never arrived), and drops the least recently updated traces past 2,000. | Must | Verified | `spans_group_into_traces_and_later_copies_replace_earlier_ones`, `jaeger_echoes_do_not_replace_pulled_spans` |
| DASHR-TRACE-004 | `dashr traces` lists recent traces (filters: window, service, span name, attributes, errors) and `dashr trace <id>` shows one as a masked text sequence, or every masked span with `--json`. | Must | Verified | `crates/dashr-cli/src/agent.rs`; AC-OTLP, AC-MASK |

### 8.3 Pull sources (SOURCE)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-SOURCE-001 | `dashr source add <name> --format F --every N -- <command>` runs a command (through `cmd /c` on Windows) every N seconds (at least 5) with the window to read in `DASHR_SINCE`/`DASHR_UNTIL` (seconds, `_MS`, `_ISO`), re-reading two minutes before the previous run for late spans, and converts what it prints. Several documents in a row and JSON lines are accepted; printing nothing is fine. | Must | Verified | `Shared::add_source`, `Shared::run_source`; AC-SOURCE |
| DASHR-SOURCE-002 | A source is tried once when added. The trial reports span, trace and service counts — never values — and a failing trial is refused with the command's last stderr line (masked), unless `--keep-on-error`. `dashr source list` shows each source's health; `rm` stops it. | Must | Verified | AC-SOURCE |
| DASHR-SOURCE-003 | Pulled spans are kept in the store and sent on to the session's Jaeger over OTLP/JSON, so Jaeger's UI shows the joined trace too. `dashr ingest <file or ->` imports once. | Must | Verified | `encoding_round_trips`; AC-SOURCE, AC-FORMATS |

### 8.4 Flows (FLOW)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-FLOW-001 | A flow is JSON: name, description, a selector (attributes any span carries, a root-name glob), order (sequence or any), and steps — service and span globs, kind, expected attributes (value, `*` present, `!` absent, `re:` regex), expected error, optional, count bounds, time budget, `code` and `why` for review — plus forbidden spans, `no_errors`, a trace budget and `settle_secs`. Invalid flows are refused with the reason. | Must | Verified | `invalid_flows_are_refused_with_a_reason`; `.agents/skills/herdr-dashr/reference/flows.md` |
| DASHR-FLOW-002 | ~~A new or changed flow waits for the human's review (Approve / Request changes, `dashr flow wait --review`).~~ Removed in 2.0.1: the human found the approval step unnecessary; a flow is the agent's expectation and is used as soon as it is set. The human looks at spans and their code in the Spans tab instead (DASHR-VIEW-005). | Must | Rejected | `DEC-054`; AC-FLOW |
| DASHR-FLOW-003 | Every trace updated since the flow was armed (`flow set` or `flow arm`) and started no earlier than five seconds before is matched against it; the best match (most steps met, then newest) is the verdict: per step `ok`, `skipped`, `missing`, `out_of_order`, `mismatch`, `error` or `slow` with problems, plus unexpected error spans and forbidden spans. An error or forbidden span fails it at once; otherwise it passes when every step is met and fails when the trace has been quiet for `settle_secs` without. | Must | Verified | `a_trace_that_does_what_the_flow_says_passes`, `errors_retries_forbidden_spans_and_order_fail`; AC-VIEW |
| DASHR-FLOW-004 | Expected values are compared against raw values inside dashr; the verdict shows the expected value as written and the actual value masked. | Must | Verified | `wrong_values_are_reported_masked`; AC-VIEW |
| DASHR-FLOW-005 | `dashr flow wait <name>` waits for a decided verdict and prints it with the trace's sequence (exit 0 pass, 1 fail, 4 timeout with the current state). | Must | Verified | AC-VIEW |

### 8.5 Viewer (VIEW)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-VIEW-001 | The pane shows the viewer link (Ctrl-click) on one line of the narrow pane, so the terminal makes all of it clickable; the Jaeger UI link, the OTLP endpoints, trace and span counts, one line per flow (waiting for a run, arriving, pass, fail), per file the human edited in the viewer, and per source. | Must | Verified | `crates/dashr-cli/src/pane.rs`, `DEC-055`; AC-HERDR |
| DASHR-VIEW-002 | The viewer draws the selected trace live as a sequence diagram: participants are services (plus `caller`, `?` for lost parents, and the peers client spans call that send no spans — databases, queues, AWS services); calls with activation bars and dashed returns carrying the duration, self-messages for internal spans (can be hidden), asynchronous producer/consumer arrows, errors in red, flow steps marked on their spans. | Must | Verified | `spans_become_calls_self_messages_and_peers`; AC-VIEW |
| DASHR-VIEW-003 | The viewer also shows a waterfall, the trace list (newest first, followed by default), sources' health, span details on click (raw), and a link to the trace in Jaeger. | Should | Verified | `crates/dashr-runtime/assets/viewer.html`; AC-VIEW |
| DASHR-VIEW-004 | The agent gets the same sequence as text: offsets, nesting, arrows, durations, errors with masked messages, masked attributes. | Must | Verified | `text_for_the_agent_is_masked`; AC-OTLP |
| DASHR-VIEW-005 | The viewer's Spans tab lists every span the code has, grouped by service: the spans the agent's catalog and flows name (shown even before a run produces them, as "not seen") and every span the traces hold, each with where it is made (from the catalog, a flow step's `code`, or OpenTelemetry's `code.*` attributes), how often it was seen, errors, last duration, last attributes (raw) and expected attributes not seen. Span details in the sequence and code links in the Flow tab open the same place. | Must | Verified | `the_inventory_joins_catalog_flows_and_traces`, `code_references`; AC-SPANS |
| DASHR-VIEW-007 | The viewer offers light, dark and system themes (a switch in the header, remembered in the browser, applied before the first paint, the code editor following it), with a redesigned layout: endpoint chips that copy on click, a live indicator, stat tiles for the trace, duration bars in the trace list, a waterfall time axis. | Should | Verified | `crates/dashr-runtime/assets/viewer.html`; AC-THEME |
| DASHR-VIEW-008 | Hovering a span in the waterfall or an arrow in the sequence shows how long it ran, its start and end offsets, its kind and share of its parent, and its error. | Should | Verified | AC-THEME |
| DASHR-VIEW-009 | The viewer's Export menu copies a Markdown report or saves it, an HTML report or JSON, for the selected flow or trace; masked unless the human ticks "include raw values". | Must | Verified | AC-EXPORT |
| DASHR-VIEW-006 | Clicking a span opens its file in a VS Code-style editor (Monaco, loaded from jsDelivr; a plain editor when offline) at the span's line, found by the function's name when no line is given. The human edits and saves (Save or Ctrl+S); a file changed on disk since it was opened is not overwritten (409) and an unmodified editor follows changes on disk; "Open in VS Code" opens the same line in the desktop editor. | Must | Verified | `saves_are_atomic_and_refuse_stale_versions`, `functions_are_found_at_their_definition`; AC-SPANS |

### 8.6 Agent integration (AGENT)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-AGENT-001 | `dashr wait` waits for a session (by pane id) and prints its endpoints and the `OTEL_*` environment; `dashr env` prints that environment for sh, PowerShell, cmd or JSON. | Must | Verified | AC-SESSION, AC-HERDR |
| DASHR-AGENT-002 | Every command answers JSON (the trace sequence as text), errors on stderr, with documented exit codes (0, 1 failed, 2 usage, 4 timeout, 5 error). | Must | Verified | `the_command_line_parses`; AC-FLOW |
| DASHR-AGENT-004 | `dashr spans set <catalog.json>` records the spans the agent added (service, span, kind, file, line or function, why, attribute keys) and makes the directory it runs in (or the catalog's `root`) the code root; `dashr flow set` sets the code root too until a catalog names one. `dashr spans` lists the inventory with span names and attribute values masked. `dashr status` lists the files the human saved in the viewer (`human_edits`). | Must | Verified | `invalid_catalogs_are_refused`; AC-SPANS |
| DASHR-AGENT-005 | `dashr export [--flow] [--trace] [--format md\|html\|json] [-o file]` writes a report to attach to a pull request: the flow's verdict and a table of its steps (with code locations, problems and durations), the trace's summary, the sequence as a Mermaid diagram GitHub draws and as text; HTML is a standalone page; JSON adds the spans. The agent's export is always masked. | Must | Verified | `mermaid_draws_calls_returns_and_errors_masked`, `markdown_and_html_carry_the_verdict`, `labels_cannot_break_the_diagram`; AC-EXPORT |
| DASHR-AGENT-003 | The agent skill, installed for every coding agent by the plugin's build step, teaches the workflow — open the pane, check prerequisites and ask the human for what is missing, instrument, list the spans with their code locations, write the flow, connect sources, arm, run, judge, and respect the human's edits — with references on instrumenting per language and platform, flows, and pull-source recipes for X-Ray, Application Insights, Log Analytics, Cloud Trace, Jaeger, Tempo and Zipkin. | Must | Implemented | `.agents/skills/herdr-dashr/SKILL.md`, `reference/instrumenting.md`, `reference/flows.md`, `reference/sources.md` |

### 8.7 Herdr integration (HERDR)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-HERDR-001 | `herdr-plugin.toml` is generated from code and checked against the generator in CI. | Must | Verified | `checked_in_manifest_matches_the_generator` |
| DASHR-HERDR-002 | The plugin declares the `traces` pane (a split the agent opens beside itself, narrowed to a column), the `doctor` popup, the `open` action (a trace pane beside the focused pane), `doctor`, the startup hook and the `pane.closed` event. | Must | Verified | `renders_every_entrypoint`; AC-HERDR |
| DASHR-HERDR-003 | The pane reports itself idle to Herdr once Jaeger runs (nothing waits on the human), and notifies once per decided verdict. | Should | Implemented | `crates/dashr-cli/src/pane.rs` |
| DASHR-HERDR-004 | Installing needs no Rust toolchain: the build step installs `herdr-dashr@<version>` from npm into the plugin checkout (`--prefix .`, through `cmd /c` on Windows), puts `dashr` on PATH best-effort, and installs the agent skill; entry points run `node node_modules/herdr-dashr/bin.js`. | Must | Verified | `installs_from_npm_on_every_platform_including_windows`; AC-LAUNCHER |
| DASHR-HERDR-005 | A pane whose Herdr pane is gone without a signal (Windows) stops itself after three missed checks. | Should | Implemented | `PANE_GONE_AFTER` |

### 8.8 Technology and governance (TECH, GOV)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-TECH-001 | Rust 2024 workspace, `unsafe` forbidden, clippy `all` denied, rustfmt; no async runtime. | Must | Verified | `Cargo.toml` |
| DASHR-TECH-002 | CI builds and tests on Linux, macOS and Windows and runs the end-to-end suite on Linux. | Must | Verified | `.github/workflows/build-and-test.yml`, `.github/workflows/end-to-end.yml` |
| DASHR-TECH-003 | The end-to-end suite drives the real binary, a real Jaeger, services instrumented with the real OpenTelemetry SDK (OTLP over HTTP and gRPC), a real Herdr with the plugin linked, and a real Chrome. | Must | Verified | `scripts/e2e/run.sh` |
| DASHR-TECH-004 | Releases publish checksummed archives for five targets and npm packages per platform; the npm launcher, the packaging script, the installer and the release matrix list the same platforms. | Must | Verified | `release_packaging_shim_and_installer_list_the_same_platforms` |
| DASHR-GOV-001 | `docs/PRD.md` is the specification; every pull request updates it. | Must | Verified | `.github/workflows/prd-traceability.yml` |
| DASHR-GOV-002 | A requirement may claim `Verified` only with evidence a reader can open. | Must | Verified | `.github/scripts/check-prd-traceability.ps1` |

## 9. Security requirements

| ID | Requirement | Status | Evidence |
|---|---|---|---|
| DASHR-PRIV-001 | Everything the agent receives that came from a span — trace lists, sequences, spans, verdicts, trial reports, source errors — passes through `dashr_core::privacy::Masker`: emails, card numbers (Luhn), IBANs (mod-97), phone numbers, IP addresses, JWTs, AWS keys, bearer tokens and credential pairs become stable pseudonyms; attributes named personal (`customer.email`, `user.name`) or secret (`auth.token`) are replaced whole; OpenTelemetry semantic-convention attributes are scanned, not replaced by name. | Verified | `personal_and_secret_attributes_become_pseudonyms`, `semantic_conventions_are_not_names`; AC-MASK |
| DASHR-PRIV-002 | Masking can be switched off (`[masking] enabled = false`) only in the human's configuration, for synthetic data. | Verified | `masking_can_be_switched_off_for_synthetic_data` |
| DASHR-SEC-001 | The session API and viewer listen on loopback only; each needs its own random token (the agent's 128-bit, the viewer's 48-bit so its link fits the pane, DEC-055), compared in constant time; the viewer token travels in the URL fragment, never to a server log or referrer. | Verified | `durations_and_tokens`; AC-SESSION |
| DASHR-SEC-002 | The session record holding the agent token is owner-only (0600). | Verified | AC-SESSION |
| DASHR-SEC-003 | The Jaeger container is read-only, without capabilities, without a logging driver, with no-new-privileges, and published on loopback by default. | Verified | AC-SESSION |
| DASHR-SEC-004 | Pull-source commands run as the human, with only the window variables added; dashr stores no cloud credentials. | Verified | `runs_with_environment_and_reports_failures` |
| DASHR-SEC-005 | Nothing the session held survives it: spans live only in the container's memory and the pane's process. | Verified | AC-HERDR |
| DASHR-SEC-006 | The viewer's editor reads and writes only regular UTF-8 files of at most 2 MB under the canonical code root: `..`, absolute paths and symlinks that lead outside are refused (403); saves are atomic (a temporary file renamed over the original, permissions kept). File contents go only to the viewer, never to the agent's API. | Verified | `nothing_outside_the_root`, `files_are_read_relative_to_the_root`; AC-SPANS |

## 10. Non-functional requirements

| Area | Requirement |
|---|---|
| Startup | A session is ready within seconds once the image is present (≈3 s measured); the first start pulls a 173 MB image. |
| Freshness | Local spans appear within about two seconds (batch delay 500 ms, poll 1.5 s); pulled spans within a source's interval plus the backend's own delay. |
| Footprint | Jaeger idles at about 12 MB; the store keeps at most 2,000 traces of 10,000 spans. |
| Platforms | Linux, macOS, Windows (x64); Docker required. |

## 11. Testing strategy

- Unit tests in every crate: converters against realistic documents of each
  format, the protobuf decoder against hand-encoded requests (including every
  truncation), flows and verdicts, sequences, masking, the HTTP server, the
  Docker arguments, session records.
- The end-to-end suite (`scripts/e2e/run.sh`) for every acceptance criterion
  in section 15, in CI on every pull request.

## 12. Decisions

DEC-001 to DEC-044 belong to the 1.x dashboard product; they are in this
file's history before 2.0.

| ID | Decision |
|---|---|
| DEC-045 | Rewrite as a trace-driven end-to-end testing tool (2.0). The dashboard product was not useful to its owner; what he did by hand — deploying to an ephemeral environment and having an agent tail logs while he triggered the feature — is what dashr now does with traces. Names (repository, npm package, plugin id, `dashr`) stay, so installs and links keep working. |
| DEC-046 | The pane owns a Jaeger container, as 1.x's pane owned Grafana: Jaeger takes OTLP over gRPC and HTTP (no protocol left to the SDKs' defaults), stores traces, and gives the human a second trace UI. dashr reads Jaeger's query API into its own store rather than storing spans itself. |
| DEC-047 | dashr knows formats, not clouds. Remote traces arrive through pull sources: commands the agent writes around the human's own CLIs. Converters exist for the formats those CLIs and backends print (X-Ray, Application Insights, Cloud Trace, Jaeger, Tempo, Zipkin, OTLP). The human's credentials stay with the CLIs. |
| DEC-048 | Pulled spans are sent on to Jaeger, and Jaeger's echo never replaces the pulled copy, so the source name stays on the span and Jaeger's UI shows the joined trace. |
| DEC-049 | Flows unify the instrumentation plan and the test expectation: what the human reviews (`code`, `why`, attributes) is exactly what the run is judged against. A changed flow needs review again. |
| DEC-050 | The sequence diagram is drawn from the span tree, not from span kinds alone: a client span whose callee sent spans draws nothing (the callee's span draws the call), so a call is drawn once whether or not both sides are instrumented; a span whose parent never arrived is drawn from `?`, making broken context propagation visible. |
| DEC-051 | Masking stays the privacy boundary of 1.x, reshaped for spans: attribute names under OpenTelemetry semantic-convention namespaces are never treated as personal (`service.name`, `db.name`), so the agent keeps the structure it needs. |
| DEC-052 | OTLP/protobuf is decoded by hand (about a hundred lines) for `dashr ingest` and sources, rather than a code generator and its build step. |
| DEC-054 | No review step (2.0.1). Approving a flow before a run added a wait the human did not want: they would rather see the spans and the code behind them, and change it themselves. A flow is the agent's expectation, used as soon as it is set; DEC-049's link between plan and test stays (the flow's `code` and `why` show in the Flow and Spans tabs). |
| DEC-055 | The viewer token is 12 hex characters, so the pane's link (`http://127.0.0.1:<port>/#<token>`, 36 characters) fits on one line of the narrow pane: a wrapped link was only half clickable. The port is loopback-only and the token is fresh per session; guessing 48 bits over loopback is impractical. The agent token stays 128-bit. |
| DEC-056 | The viewer's editor is Monaco from jsDelivr, not bundled: it keeps the binary small and the viewer a single file. Offline, a plain text editor edits and saves the same way. The code root comes from the agent (the catalog's `root` or the directory it runs dashr in), because only it knows which repository the feature lives in. |
| DEC-057 | Reports for pull requests are Markdown with a Mermaid sequence diagram: GitHub renders it in descriptions and comments, so the PR shows the run without an attachment or an image host. The HTML report draws the same Mermaid source (from jsDelivr) and falls back to the text sequence offline. Reports are built in `dashr-core` so the agent's and the viewer's exports are the same document; only masking differs (DASHR-PRIV-001). |
| DEC-053 | Jaeger's self-tracing is switched off (`OTEL_TRACES_SAMPLER=always_off`): it traced every poll of its query API. The poller also ignores a `jaeger` service. Found while testing against a real Jaeger. |

## 13. Open questions and risks

| ID | Question or risk |
|---|---|
| OQ-101 | X-Ray and Application Insights index with a delay of seconds to minutes; flows need a generous `settle_secs`. A source could report the backend's lag. |
| OQ-102 | Step Functions passes trace context to Lambda tasks, but not to every integration (ECS `RunTask`, some SDK integrations); the skill tells the agent to check, and the sequence shows `?` where it breaks. |
| OQ-103 | Services in containers reach Jaeger on the host only with `[jaeger] bind = "0.0.0.0"`, which also exposes it to the network. A Docker-network-only binding would be safer. |

## 14. Delivery ledger

| Date | Change | Requirements |
|---|---|---|
| 2026-09-26 | M0/M1: PRD, CI, governance; library crates `dashr-core`, `dashr-grafana`, `dashr-docker`, `dashr-herdr`, `dashr-aws`, `dashr-runtime`, `dashr-mcp` with unit tests. | GOV-001..003, TECH-001..002, GRAF-*, DS-*, PRIV-*, AWS-*, MCP-*, ALERT-*, PROMO-*, SEC-* |
| 2026-09-26 | M2: `dashr` binary (Herdr actions, dashboard pane, chat pane, hooks, standalone commands, doctor), generated `herdr-plugin.toml`, `scripts/install.sh`, agent skill, end-to-end suite in CI; fixed a masking gap on non-personal datasources (DEC-017). | HERDR-001..007/009, GRAF-001..007, VIEW-001/003, CHAT-001..003, MCP-002..005/009/010, PRIV-005, AWS-004, ALERT-001/002, TECH-003, SEC-001/004 |
| 2026-09-26 | M3: release workflow publishing checksummed Linux/macOS archives for `scripts/install.sh`; dependency majors with the Grafana client ported to ureq 3. | TECH-004 |
| 2026-09-26 | End-to-end suite extended: promote against a second real Grafana, watch removal back to idle, `get_dashboard`, `open_for_pipeline`, exported AWS credentials in the container, doctor, custom image with Infinity. Fixed `remove_watch` leaving the pane blocked (DEC-022). | HERDR-009, GRAF-007, DS-004, MCP-006..008, ALERT-004, PROMO-001..003 |
| 2026-09-26 | Release workflow can create its own tag from a manual run; Intel macOS build on `macos-15-intel`; evidence recorded for governance, AWS, alert and skill rows; terminal-browser limitation recorded (OQ-007). | TECH-004, CHAT-002, AWS-005/006, ALERT-003, GOV-001..003, TECH-002, SEC-005/007, VIEW-001/002/004 |
| 2026-09-26 | v0.1.0 released (4 targets, checksummed). Verified a GitHub install with no Rust toolchain. Only the terminal-browser rows remain `Implemented`, pending a manual check in a kitty-graphics terminal (OQ-007). | HERDR-006, TECH-004 |
| 2026-09-26 | Browser pane verified end to end: a pty that answers kitty graphics queries lets CI run terminal-browser against the real Grafana. Reload and screenshot moved to direct CDP (DEC-024); fixed the Grafana crash under the C locale (DEC-025) and per-user runtime dirs (DEC-026). | VIEW-001/002/004, MCP-010, GRAF-004, SEC-005 |
| 2026-09-26 | v0.2.0: dashboard-building agent skill (loop, privacy rules, dashboard JSON, per-datasource query models, recipes), installed by the plugin build step and refreshed by the pane, served as MCP resources; examples validated by tests and on a real Grafana. | SKILL-001..004 |
| 2026-09-26 | npm distribution ported from herdr-remote-channel: `herdr-dashr` and four platform packages, verified packaging, publish workflow after Release; `install.sh` prefers npm and falls back to the GitHub release. | TECH-005, HERDR-006 |
| 2026-09-26 | Publish to npm takes its packaging tools from the workflow commit and the version from the released commit, so tags cut before the tools (v0.2.0) can be published; the first dry run had checked out the tag and found no packaging script. `setup-node` moved off the deprecated Node 20 runtime. | none (fix to TECH-005 pipeline) |
| 2026-09-26 | v0.3.0: OpenTelemetry mode — one `grafana/otel-lgtm` container per pane with an OTLP endpoint, announced to the human and the agent — plus `dashr tail`, and live log checks: expectation tiles and a highlighted live log trail on top of the dashboard, notifications as messages arrive, a counts-only verdict (`expect_logs`, `log_expectations`, `clear_log_expectations`, `session_info`, `dashr expect`). Watches gain a severity and hold their state through a panel with no data. Skill reference for OTel data and log checks. OpenTelemetry telemetry kept on disk in anonymous volumes deleted with the container, and traces searchable in about 2 s instead of 30 s (DEC-033). | OTEL-001..006, LOGX-001..005, ALERT-005, SKILL-005, GRAF-002, SEC-004 |
| 2026-09-26 | Fixed the OpenTelemetry dashboard not refreshing by itself (DEC-034); the e2e suite now checks self-refresh and uploads screenshots of the browser pane. | VIEW-001 |
| 2026-09-26 | Saved dashboards: name the current dashboard and reload it in a later pane (`save_dashboard`, `list_saved_dashboards`, `load_dashboard`, `delete_saved_dashboard`, `dashr dashboards`, `dashr session start --load`); the agent is told which are saved. Fixed a datasource uid clash when OpenTelemetry mode is switched on at run time (DEC-036). | LIB-001..004, OTEL-002 |
| 2026-09-26 | Versions consolidated into a single public v0.1.0 (DEC-037): version reset to 0.1.0, `Delete release` workflow to remove the earlier v0.1.0 and v0.2.0 releases before re-releasing; `NPM_TOKEN` set, so v0.1.0 is the first npm publish. The end-to-end metric check now probes with an instant query: a range query shows a point sent "now" only after the next step boundary, which made the 10 s check pass or fail by clock alignment. | TECH-005 |
| 2026-09-26 | v0.1.0 released on GitHub (4 checksummed archives). Publish to npm was refused (E404 for a new package: the token may not create packages); it now checks the token first and fails fast on permission errors. The dashboard pane retries opening the chat pane for a few seconds (twice, on the first open after a fresh Herdr server, the split was refused and the agent never started), and the e2e suite prints the pane when that check fails. | TECH-005 |
| 2026-09-26 | v0.1.0 published to npm (`herdr-dashr` + four platform packages); `npx herdr-dashr@0.1.0` runs the release binary. Every PRD requirement is Verified. | TECH-005 |
| 2026-09-27 | Windows support, installed as herdr-remote-channel installs (DEC-038): npm build step and `node` launcher on every platform, a Windows release target and npm package, PowerShell quoting for the chat pane, Windows unit tests in CI, and `.gitattributes` keeping LF on Windows checkouts (the manifest test compares bytes). Version 0.1.1. | HERDR-006, HERDR-008, TECH-004, TECH-005 |
| 2026-09-27 | npm publish retries survive registry lag: a version the registry refuses as already published counts as published, so re-running a half-finished publish completes it. | TECH-005 |
| 2026-09-27 | A chat pane that fails to open, or whose id fails to save, is reported on the text view: the view clears the screen, which had hidden the reason when AC-OPEN intermittently found no chat pane. | HERDR-002 |
| 2026-09-27 | The fake-CLI unit tests create their scripts with `cp`, so a write handle can no longer leak into a parallel test's fork and fail the run with ETXTBSY ("Text file busy"), as it did on Linux CI. | none (test robustness) |
| 2026-09-27 | The Windows npm package is published as `@czinegeroland/herdr-dashr-win32-x64`: npm's spam filter refused `herdr-dashr-win32-x64` on every attempt. The launcher maps each platform to its full package name. The v0.1.1 release is unchanged, and its publish is re-run with the new packaging. | TECH-005 |
| 2026-09-27 | The human only talks to their AI session (DEC-039, mirroring herdr-remote-channel). The skill is installed with `npx skills add`. It opens the dashboard pane beside the session (for example for a pasted CodePipeline link), `dashr wait` hands over the session and briefing, and `dashr tool` builds with the masked tools. Panes opened this way have no chat pane. State moved to dashr's own directory, with Windows-aware defaults. New AC-AGENT end-to-end scenario. Version 0.1.2. | HERDR-010, GRAF-009, MCP-011, SKILL-002, SKILL-006 |
| 2026-09-27 | Plain one-line descriptions: the npm package and the plugin manifest say "Live Grafana dashboards in a Herdr pane.", and the platform packages say "The dashr executable for <os> <cpu>.", as herdr-remote-channel's do. Version 0.1.3, because npm cannot change a published version's description. | none (wording) |
| 2026-09-27 | The pane shows a link, not a browser (DEC-040). It is a narrow column that sizes itself, with the link, panel health, alerts and the OTLP endpoint. terminal-browser is dropped from the pane, the doctor and CI. The e2e browser scenarios now open the pane's link in a real Chrome and check that the tab follows the agent's changes without reloading. Version 0.1.4. | VIEW-001, VIEW-002, VIEW-003, VIEW-004, SEC-005 |
| 2026-09-27 | Pane watchdog: the pane process checks every monitor tick that its pane still exists. After two misses it stops Grafana, deletes its files and exits. On Windows an orphaned `dashr.exe` had kept the container running and made `herdr plugin uninstall` fail with OS error 32. Version 0.1.5. | GRAF-010 |
| 2026-09-27 | Dashboard-only page (DEC-041). The pane's link opens a loopback page showing Grafana's shared view of the dashboard, with no Grafana UI, not even after Esc. It reloads the dashboard within about 2 s of every change. The e2e Chrome scenario checks that no Grafana menu word is visible. Released with 0.1.5. | VIEW-001, VIEW-002, VIEW-004, VIEW-005 |
| 2026-09-27 | The plugin install also puts `dashr` on the PATH with a new build step (`dashr global install --best-effort` → `npm install -g herdr-dashr@<version>`), so the AI session needs no manual `npm install -g`. A failure is reported and never fails the install. Released as 1.0.0. | HERDR-011 |
| 2026-09-27 | 1.0.0 (DEC-042): a short README, one description everywhere, a tsk-style manifest (checked with Herdr 0.9.1), the skill for every detected coding agent. | SKILL-002 |
| 2026-09-27 | Removed the `Unpublish from npm` workflow: npm refuses unpublishing with a token that bypasses 2FA, so the owner removes the 0.1.x test versions by hand. | none (workflow removal) |
| 2026-09-27 | Live collectors (DEC-043). New `dashr discover`, and `dashr collect` with docker, host, process, logs, stream, exec, scrape, postgres, mysql and redis, run by the pane. The skill now leads with discover → collect → system dashboard for any environment, with new `reference/collectors.md` and `reference/environments.md`. The OTLP address left the pane, and the pane logs its chat-pane decisions (`pane.log`) to diagnose AC-OPEN's intermittent missing chat pane. Version 1.0.1. | COLL-001, COLL-002, COLL-003, COLL-004, COLL-005, COLL-006 |
| 2026-09-27 | Database query performance (DEC-044): new `dashr db add/dashboard/list/remove/plan` for PostgreSQL and SQL Server, local or in any cloud, including through a password command and a tunnel command. It brings a PostgreSQL dashboard, a SQL Server dashboard rewriting the human's two procedures, a database collector run by the pane, the skill's `reference/databases.md`, and the AC-DB and AC-DB-MSSQL end-to-end scenarios. Version 1.1.0. | DB-001, DB-002, DB-003, DB-004, DB-005, DB-006 |
| 2026-09-28 | Windows install fix: `herdr plugin install` failed at `bin.js global install` with MODULE_NOT_FOUND when a parent directory of the plugin checkout (the user's home) had a `package.json`, because npm installed herdr-dashr there. The build step now passes `--prefix .`. | HERDR-006 |
| 2026-09-28 | Stopping a database's tunnel signalled every process on the machine when the tunnel's pid started with 1: procps-ng 4.0.4's `kill -TERM -<pid>` reads the pid as an option. It was found when the end-to-end suite's `db remove` shut down the GitHub runner. The group is now signalled as `kill -TERM -- -<pid>`, and never for a pid of 0 or 1. Before release. | DB-002 |
| 2026-09-28 | Database datasources keep no idle connections. When `db add` handed its relay to the pane (and whenever a tunnel restarts), SQL Server's driver returned the pooled connections that died with the old path as "failed to connect to server". Reproduced against SQL Server 2022 and Grafana 12.1.1 by swapping the relay: two failures every time, none with `maxIdleConns: 0`. | DB-002 |
| 2026-10-01 | 2.0: the rewrite (DEC-045). Removed the Grafana, Docker-dashboard, AWS, MCP and database code of 1.x. New: a Jaeger session per pane, the trace store fed by Jaeger and by pull sources, converters for OTLP (JSON and protobuf), X-Ray, Zipkin, Jaeger, Application Insights, Log Analytics and Cloud Trace, flows with the human's review and step-by-step verdicts, the live sequence-diagram viewer, the agent's commands, the new skill, and an end-to-end suite with real OpenTelemetry services, an X-Ray Step Functions run joined to them, a real Herdr and Chrome. | SESSION-001..006, TRACE-001..004, SOURCE-001..003, FLOW-001..005, VIEW-001..004, AGENT-001..003, HERDR-001..005, TECH-001..004, GOV-001..002, PRIV-001..002, SEC-001..005 |

| 2026-10-01 | 2.0.1: the pane's viewer link fits on one line and is clickable whole (DEC-055); the approve / request changes review is removed from the viewer, the API, `flow wait` and the pane (DEC-054); new Spans tab: every span the code has, planned and observed, opening its code in an editor the human can save from, confined to the code root (DEC-056); `dashr spans [set]`; human edits reported to the agent. | FLOW-002, VIEW-001, VIEW-005, VIEW-006, AGENT-002, AGENT-003, AGENT-004, HERDR-003, SEC-001, SEC-006 |

| 2026-10-02 | 2.0.2: a light / dark / system theme switch and a redesigned viewer (DASHR-VIEW-007); span times on hover in the waterfall and sequence (DASHR-VIEW-008); reports for pull requests from `dashr export` and the viewer's Export menu — Markdown with a Mermaid diagram, standalone HTML, JSON — masked for the agent and by default (DEC-057). The end-to-end suite now waits for `status: running` from Herdr: `herdr status server` exits 0 when the server is not running, which made AC-HERDR race the server's start. | VIEW-007, VIEW-008, VIEW-009, AGENT-005 |

### Requirement completion summary

| Area | Total | Verified | Implemented | Other |
|---|---|---|---|---|
| SESSION | 6 | 6 | 0 | 0 |
| TRACE | 4 | 4 | 0 | 0 |
| SOURCE | 3 | 3 | 0 | 0 |
| FLOW | 5 | 4 | 0 | 1 rejected |
| VIEW | 9 | 9 | 0 | 0 |
| AGENT | 5 | 4 | 1 | 0 |
| HERDR | 5 | 3 | 2 | 0 |
| TECH/GOV | 6 | 6 | 0 | 0 |
| PRIV/SEC | 8 | 8 | 0 | 0 |
| **All** | 51 | 47 | 3 | 1 |

## 15. Acceptance criteria

Each is a scenario of `scripts/e2e/run.sh`.

- **AC-SESSION** `dashr serve` starts Jaeger read-only, without capabilities or logs, with its ports on loopback and self-tracing off; the session record is owner-only; the agent API and the viewer refuse requests without their token.
- **AC-OTLP** A service exporting over OTLP/HTTP (protobuf), one over OTLP/gRPC and a driver make one trace of three services with no orphans; the agent's sequence shows the calls, the internal step, the database and the queue.
- **AC-MASK** The agent's trace list and trace show a pseudonym where the customer's email is; the viewer shows the real address.
- **AC-FLOW** A flow is used as soon as it is set (no review state, `flow wait` before a run times out with exit 4 and `--review` is not an option); a changed flow is taken and armed again; resending the same flow keeps its arming.
- **AC-SPANS** `dashr spans set` from the repository makes it the code root and the agent's `dashr spans` lists planned spans (one not seen yet) with names masked; in Chrome, the Spans tab lists them by service, opens a span's code at its function, the human edits and saves it with Ctrl+S, the file on disk changes and `dashr status` reports the edit; a stale save is refused with 409; paths outside the root (`..`, absolute, a symlink) are refused; the viewer token is required.
- **AC-EXPORT** `dashr export` writes the failing flow as Markdown (verdict heading, step table, Mermaid diagram), HTML and JSON, with no email or card number in any; the viewer's export is masked by default and raw with `raw=1`, needs the viewer token, and its Export menu saves the HTML report.
- **AC-THEME** In Chrome, the theme switch makes the page dark and light and remembers the choice; hovering a waterfall bar shows the span's duration, start and end.
- **AC-VIEW** In Chrome, a correct run passes 5/5 with the sequence; a broken run (too many items, a card number recorded) fails with each step's reason and no card number in the output; the viewer shows the failure and draws the sequence.
- **AC-SOURCE** A pull source printing `batch-get-traces` output for a Step Functions run (ten Lambdas, an ECS task, DynamoDB) joins the local trace under the event that started it; the trial reports counts only; the failing Lambda's error is masked; the spans reach Jaeger; a failing source is refused with its reason, `--keep-on-error` keeps one, `rm` removes them.
- **AC-FORMATS** Zipkin, Jaeger, Application Insights and Cloud Trace exports import with `dashr ingest`, recognised by shape; an unknown document is refused.
- **AC-REAP** After a session is killed with SIGKILL, the Herdr startup hook removes its container and record.
- **AC-HERDR** The AI session opens the trace pane with `herdr plugin pane open`; `dashr wait --session <pane>` finds it; the pane shows the viewer link whole on one line, the Jaeger link and the flow's state; closing the pane removes Jaeger and the record.
- **AC-DOCTOR** `dashr doctor` passes the required checks and reports the optional ones.
- **AC-LAUNCHER** The npm launcher (`npm/dashr/bin.js`) runs the build under test.
