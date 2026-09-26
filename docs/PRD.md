# Herdr Dashr Product Requirements Document

## Document control

| Field | Value |
|---|---|
| Product | Herdr Dashr |
| Command | `dashr` |
| Herdr plugin id | `herdr-dashr` |
| Repository | `czinegeroland/herdr-dashr` |
| Document status | Draft |
| PRD version | 0.1.0 |
| Delivery phase | M1 - Libraries |
| Last updated | 2026-09-26T00:40:00Z |
| Product owner | @czinegeroland |
| Source handoff | `docs/DESIGN.md` |

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

Herdr Dashr is a Herdr plugin that opens an **agent-built, live Grafana
dashboard inside a Herdr pane** for debugging deployed work. One action — or
one Ctrl-click on an AWS CodePipeline URL — opens a tab with the real Grafana
UI on top (rendered by terminal-browser) and a coding agent underneath. The
agent designs dashboards for the problem at hand through an MCP server; Grafana
refreshes the data itself, so **no model sits in the data path**, and a
masking layer guarantees the agent designs from schemas and masked samples
while only the human sees real values.

The Grafana is **pane-owned and stores nothing**: an in-memory container on a
loopback port, started with the pane and removed when it closes.

## 2. Problem statement

Debugging an ephemeral environment deployed from a pipeline today means
pasting a CodePipeline link to an agent and watching it tail logs, poll SQS
queues and follow Step Functions executions as text. The agent sits in the
data path, burns tokens re-reading the same logs, and sees every value —
including personal data in application logs. The Herdr marketplace (~1,300
plugins) has no observability dashboard plugin, and Herdr accepts no
unsolicited core pull requests, so a plugin is the only delivery path.

## 3. Goals

- **G-001 One delegation, one dashboard.** A single action opens a dashboard
  built for the problem, with a chat underneath to reshape it.
- **G-002 No model in the data path.** Grafana refreshes data; the agent only
  changes the dashboard.
- **G-003 Privacy by construction.** The agent never receives unmasked
  datasource values.
- **G-004 Disposable.** Nothing survives the pane: no container, no
  database, no browser profile, no runtime file.
- **G-005 Useful before the agent speaks.** A pipeline URL yields a first
  dashboard with no model involved.
- **G-006 Living specification.** This PRD tracks delivery, enforced by CI.

## 4. Non-goals

- A terminal-native chart renderer. terminal-browser shows the real Grafana.
- A persistent Grafana. `promote` copies to one the team already runs.
- Features the Herdr plugin API cannot support today: plugin items in
  right-click menus (#1722), plugin-owned sidebar sections (#1609),
  triggering built-in actions (#1624), theme colours in plugin panes (#1796).
- Windows. terminal-browser supports Linux and macOS only.

## 5. Personas and use cases

- **P-001 Developer** debugging an ephemeral environment in Herdr.
- **P-002 Coding agent** (Claude Code by default) in the chat pane.
- **UC-001** Open a live dashboard and ask the agent for a view of a system.
- **UC-002** Ctrl-click a CodePipeline URL; get a dashboard of the stacks it
  deployed (log groups, queues and DLQs, state machines, Lambdas).
- **UC-003** Ask the agent to alert when a panel crosses a threshold; get a
  Herdr notification and a blocked pane without the agent seeing values.
- **UC-004** Promote a useful dashboard to the team's Grafana.

## 6. Architecture

```
┌─ dashboard pane (plugin pane, placement=tab) ────────┐
│  dashr herdr pane dashboard                          │
│   ├─ owns: Grafana container (tmpfs, loopback)       │
│   ├─ runs: terminal-browser open <kiosk url>         │
│   │        (or the text status view)                 │
│   └─ loop: panel health → $dashr token; watches      │
├─ chat pane (split below) ────────────────────────────┤
│  claude --mcp-config <runtime>/mcp.json              │
│   └─ dashr mcp --session <id>  (masking boundary)    │
└──────────────────────────────────────────────────────┘
```

Crates:

| Crate | Responsibility |
|---|---|
| `dashr-core` | Configuration, provisioning, masking, dashboard validation, frames, watches, session files. Pure. |
| `dashr-grafana` | Blocking client for Grafana's HTTP API. |
| `dashr-docker` | Container argv, lifecycle and orphan detection via the `docker` CLI. |
| `dashr-herdr` | Manifest generation, plugin environment, `herdr` CLI wrapper. |
| `dashr-aws` | CodePipeline URL parsing, discovery via the `aws` CLI, first-dashboard proposal. |
| `dashr-runtime` | Session start/stop, apply, panel status, browser control, monitor tick, promote. |
| `dashr-mcp` | MCP stdio server and the agent's tools. |
| `dashr-cli` | The `dashr` binary: Herdr entrypoints and standalone commands. |

## 7. Scope and milestones

| Milestone | Content |
|---|---|
| M0 | PRD, CI, governance. |
| M1 | Libraries: core, Grafana client, Docker lifecycle, Herdr wrapper, AWS bootstrap, runtime, MCP server. |
| M2 | `dashr` binary and Herdr wiring: manifest, actions, panes, hooks, install. |
| M3 | End-to-end suite against real Herdr and Grafana in CI; releases. |

## 8. Functional requirements

### 8.1 Herdr plugin (HERDR)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-HERDR-001 | `herdr-plugin.toml` is generated from code and a test fails when the checked-in file differs. | Must | Implemented | `crates/dashr-herdr/src/manifest.rs` renders it; checked-in file and comparison test land in M2. |
| DASHR-HERDR-002 | Action `open` opens a new tab: dashboard pane on top, chat pane split below. | Must | Approved | Pending |
| DASHR-HERDR-003 | A link handler routes Ctrl-clicked CodePipeline console URLs to action `pipeline`, which opens a bootstrapped dashboard. | Must | Implemented | Pattern tested in `manifest::tests::link_pattern_matches_both_consoles`; action lands in M2. |
| DASHR-HERDR-004 | A startup hook stops dashr containers of this Herdr server whose pane no longer exists. | Must | Implemented | `dashr_docker::orphans`, tested in `orphans_are_this_servers_containers_without_a_live_pane`; hook lands in M2. |
| DASHR-HERDR-005 | A `pane.closed` event hook stops the closed pane's container and deletes its runtime files. | Must | Approved | Event shape tested in `env::tests::reads_observed_herdr_shapes`. |
| DASHR-HERDR-006 | Installing needs no Rust toolchain: the build step downloads a checksum-verified release binary, falling back to `cargo` only when no release exists. | Must | Approved | Pending |
| DASHR-HERDR-007 | The dashboard pane reports a `$dashr` sidebar token summarising panel health (e.g. `6 ok · 1 err`). | Should | Implemented | `dashr_runtime::monitor::report`, `status::Summary::token`. |
| DASHR-HERDR-008 | The plugin declares and supports Linux and macOS. | Must | Implemented | Manifest `platforms`; CI matrix in `.github/workflows/build-and-test.yml`. |
| DASHR-HERDR-009 | A `doctor` action checks Docker, Herdr, terminal-browser, the agent CLI and the AWS CLI and says what is missing. | Should | Approved | Pending |

### 8.2 Grafana lifecycle (GRAF)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-GRAF-001 | One container per pane, named `herdr-grafana-<session>`, labelled with owner, pane, socket hash and session, bound to a random loopback port. | Must | Implemented | `dashr_docker::RunSpec::args`, `run_args_store_nothing_and_bind_loopback`. |
| DASHR-GRAF-002 | The container stores nothing: `--read-only`, tmpfs for `/var/lib/grafana`, `/tmp`, `/var/log/grafana`, `--log-driver none`, `--memory-swap` equal to `--memory`, all capabilities dropped. | Must | Implemented | Same test; flags verified against `grafana/grafana:12.1.1` during development. |
| DASHR-GRAF-003 | Anonymous admin, login form disabled, analytics, update checks and news disabled. | Must | Implemented | `RunSpec::grafana_env`. |
| DASHR-GRAF-004 | Runtime files live in a 0700 directory on a memory-backed file system where one exists, and are deleted on stop. | Must | Implemented | `dashr_runtime::paths`, `runtime_dir_is_private_and_removable`. |
| DASHR-GRAF-005 | Start waits for `/api/health` with a bounded timeout and cleans up on failure; a missing Docker is a clear error. | Must | Implemented | `session::start`, `start_fails_cleanly_without_docker`, `unreachable_and_timeout`. |
| DASHR-GRAF-006 | The pane stops its container on normal exit and on SIGINT, SIGTERM and SIGHUP (Herdr sends SIGHUP on pane close). | Must | Approved | SIGHUP on close observed against Herdr 0.9.1. |
| DASHR-GRAF-007 | The image is pinned (`grafana/grafana:12.1.1`); `dashr image build` produces a custom image with Infinity and Zabbix plugins installed outside the tmpfs path. | Must | Implemented | `config::DEFAULT_IMAGE`, `dockerfile_keeps_plugins_out_of_the_tmpfs`; command lands in M2. |
| DASHR-GRAF-008 | A session record (ids, port, uid, datasource policies) is written to the plugin state directory, holds no secret or data value, and is removed on stop. | Must | Verified | `session::tests::records_hold_no_secret_shaped_fields`, `round_trips_lists_and_removes`. |

### 8.3 Datasources (DS)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-DS-001 | A TestData datasource is always provisioned and flagged non-personal. | Must | Verified | `provisioning::tests::default_config_provisions_only_testdata`. |
| DASHR-DS-002 | Prometheus, Loki, Tempo, CloudWatch, SQL Server, Azure Monitor, Zabbix and Seq (via Infinity) are provisioned from `dashr.toml`. | Must | Verified | `provisioning::tests::every_kind_maps_to_its_plugin`. |
| DASHR-DS-003 | Secrets are `$__env{NAME}` references in provisioning and reach the container as `-e NAME`; values never appear in files or argv, and configuration rejects values in place of names. | Must | Verified | `secrets_are_env_references_never_values`, `secrets_are_passed_by_name_only`, `secret_env_must_be_a_name_not_a_value`. |
| DASHR-DS-004 | CloudWatch receives short-lived credentials exported by `aws configure export-credentials`, falling back to credentials already in the environment. | Must | Implemented | `session::resolve_secrets`, `credentials_keep_only_known_variables`. |
| DASHR-DS-005 | Every datasource carries a `personal` flag, defaulting to true. | Must | Verified | `config::tests::example_configuration_parses`. |
| DASHR-DS-006 | Loopback datasource URLs are rewritten to `host.docker.internal`, with a host-gateway mapping on Linux. | Must | Verified | `loopback_urls_are_rewritten_and_others_kept`, `run_args_store_nothing_and_bind_loopback`. |

### 8.4 Browser pane (VIEW)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-VIEW-001 | The dashboard pane shows the Grafana kiosk URL (`?kiosk&refresh=<n>`) in terminal-browser. | Must | Implemented | `SessionRecord::kiosk_url`, `Browser::open_command`; pane lands in M2. |
| DASHR-VIEW-002 | The browser profile lives in the session runtime directory via `TERMINAL_BROWSER_APPDATA` and is deleted with it. | Must | Implemented | `open_command_points_the_profile_at_the_runtime_dir`. |
| DASHR-VIEW-003 | Without terminal-browser (or with `browser.enabled = false`) the pane shows a text status view: the URL and per-panel state. | Must | Approved | Pending |
| DASHR-VIEW-004 | Applying a dashboard reloads the browser showing this session. | Should | Implemented | `apply::apply`, `finds_the_browser_showing_the_session`. |

### 8.5 Chat pane (CHAT)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-CHAT-001 | The chat pane runs the configured agent command with a generated MCP config naming `dashr mcp --session <id>`; `mcp-grafana` is added only when opted in. | Must | Approved | Pending |
| DASHR-CHAT-002 | The agent receives the privacy rules as MCP server instructions and an agent skill. | Must | In progress | `dashr_mcp::tools::INSTRUCTIONS`; skill lands in M2. |
| DASHR-CHAT-003 | The agent command is typed into the pane's shell with every argument POSIX-quoted. | Must | Implemented | `dashr_core::shell`, `everything_else_is_single_quoted`. |

### 8.6 MCP server (MCP)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-MCP-001 | Stdio MCP server: `initialize` (version negotiation), `ping`, `tools/list`, `tools/call`, JSON-RPC errors. | Must | Verified | `protocol::tests::full_handshake_and_call`, `protocol::tests::errors`. |
| DASHR-MCP-002 | `list_datasources` returns uid, name, type and personal flag only. | Must | Implemented | `DatasourcePolicy`. |
| DASHR-MCP-003 | `apply_dashboard` validates structure and datasource references, pins uid/refresh/tag, saves and reloads. | Must | Implemented | `dashboard::tests::*`, `apply::apply`. |
| DASHR-MCP-004 | `panel_status` reports per-panel state, rows, fields and masked errors, never values. | Must | Implemented | `summaries_carry_no_values_and_mask_errors`, `status::tests::classification`. |
| DASHR-MCP-005 | `panel_data_sample` and `probe_query` return masked rows with real field names and types, capped by `masking.max_rows`. | Must | Implemented | `DashrTools::masked_results`. |
| DASHR-MCP-006 | `get_dashboard` returns the current dashboard JSON. | Should | Implemented | `DashrTools::get_dashboard`. |
| DASHR-MCP-007 | `open_for_pipeline` bootstraps from a CodePipeline URL, or opens a new dashboard tab when the region lacks CloudWatch. | Should | Implemented | `DashrTools::open_for_pipeline`. |
| DASHR-MCP-008 | `promote` copies the dashboard to a configured persistent Grafana. | Should | Implemented | `promote::promote`. |
| DASHR-MCP-009 | `watch_panel`, `list_watches` and `remove_watch` manage local alert rules. | Should | Implemented | `watches_are_managed_through_the_store`. |
| DASHR-MCP-010 | `screenshot` is refused unless every datasource the dashboard uses is non-personal. | Must | Verified | `tools::tests::screenshots_only_for_non_personal_dashboards`. |

### 8.7 Privacy and masking (PRIV)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-PRIV-001 | Fields whose names contain personal tokens are redacted; values in other string fields pass through detectors for email, IBAN (mod-97), card (Luhn), phone, IPv4/IPv6, JWT, AWS keys, bearer tokens and credential assignments. | Must | Verified | `masking::tests::detectors_scrub_free_text`, `personal_fields_are_redacted_with_stable_pseudonyms`. |
| DASHR-PRIV-002 | Replacements are stable pseudonyms within one response. | Must | Verified | `personal_fields_are_redacted_with_stable_pseudonyms`. |
| DASHR-PRIV-003 | Global and per-datasource allow-lists pass named fields; secret-named fields are never allow-listed. | Must | Verified | `allow_lists_apply_but_never_to_secrets`. |
| DASHR-PRIV-004 | Numbers, booleans and timestamps pass unless the field is redacted; ids and dates are not mistaken for phones. | Must | Verified | `ordinary_numbers_in_text_are_not_phones`. |
| DASHR-PRIV-005 | Non-personal datasources skip personal detectors but keep secret detectors. | Must | Verified | `secrets_are_masked_even_on_non_personal_datasources`. |
| DASHR-PRIV-006 | Strings are truncated, rows capped, labels and nested values masked, and values without a field description replaced. | Must | Verified | `rows_are_capped_and_long_strings_truncated`, `labels_are_masked_by_key_and_value`, `values_without_field_descriptions_are_not_trusted`. |
| DASHR-PRIV-007 | Users add patterns and deny tokens in configuration; invalid patterns are rejected. | Should | Verified | `extra_patterns_apply`, `config::Config::validate`. |

### 8.8 CodePipeline bootstrap (AWS)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-AWS-001 | New-console, execution and old-console URLs parse to region, pipeline and execution; anything else is refused. | Must | Verified | `url::tests::*`. |
| DASHR-AWS-002 | CloudFormation deploy actions yield stack names (once per stack and region); other deploy providers produce a warning. | Must | Verified | `finds_cloudformation_stacks_once_each`. |
| DASHR-AWS-003 | Stack resources classify into log groups, queues (DLQ detection), state machines and Lambdas, following nested stacks. | Must | Verified | `classifies_resources`, `discover_follows_nested_stacks_through_a_fake_cli`. |
| DASHR-AWS-004 | A first dashboard is proposed: stage table, error logs, queue depth/age, DLQ stat, Step Functions and Lambda metrics; it validates and has no overlapping panels. | Must | Verified | `every_resource_kind_gets_panels_with_valid_queries`, `many_log_groups_are_chunked`. |
| DASHR-AWS-005 | Resource names are infrastructure metadata and are returned unmasked; error text from AWS is masked. | Should | Implemented | `session::start` masks bootstrap errors. |
| DASHR-AWS-006 | AWS is reached only through the `aws` CLI; dashr holds no AWS credentials of its own beyond passing exported ones to the container. | Must | Implemented | `dashr_aws::cli`. |

### 8.9 Alerts (ALERT)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-ALERT-001 | Watch rules are evaluated by the dashboard pane against real values; values never leave the pane. | Must | Implemented | `watch::evaluate`, `monitor::tick`. |
| DASHR-ALERT-002 | A new breach marks the dashboard pane blocked with the rule wording (no values). | Must | Implemented | `a_new_breach_blocks_and_notifies_once`. |
| DASHR-ALERT-003 | A new breach raises one Herdr notification when `monitor.notify` is on. | Should | Implemented | Same test. |
| DASHR-ALERT-004 | When every breach clears, the pane returns to idle. | Should | Implemented | `clearing_the_last_breach_returns_to_idle`. |

### 8.10 Promote (PROMO)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-PROMO-001 | Promotion saves into a folder (created when missing) without overwriting. | Should | Implemented | `promote::promote`, `token_goes_in_a_header_and_never_in_debug_output`. |
| DASHR-PROMO-002 | Datasources are remapped by name; missing ones are listed and nothing is saved. | Should | Implemented | `remaps_every_reference_and_leaves_others`. |
| DASHR-PROMO-003 | The token comes from the environment variable named in configuration, travels only in a header and never appears in debug output. | Must | Verified | `token_goes_in_a_header_and_never_in_debug_output`, `unconfigured_or_tokenless_promotion_is_refused_before_any_request`. |

### 8.11 Governance and implementation (GOV, TECH)

| ID | Requirement | Priority | Status | Evidence |
|---|---|---|---|---|
| DASHR-GOV-001 | Every pull request updates this PRD and its ledger, enforced by `PRD traceability`. | Must | Implemented | `.github/workflows/prd-traceability.yml`. |
| DASHR-GOV-002 | Pull requests follow `.github/pull_request_template.md`. | Must | Implemented | `.github/pull_request_template.md`. |
| DASHR-GOV-003 | A `Verified` row must cite evidence. | Must | Implemented | `.github/scripts/check-prd-traceability.ps1`. |
| DASHR-TECH-001 | Rust workspace, edition 2024, pinned toolchain, `unsafe_code` forbidden, clippy denied. | Must | Verified | `Cargo.toml`, `rust-toolchain.toml`. |
| DASHR-TECH-002 | CI runs format, clippy and tests on Linux and macOS behind one `Build and test` check. | Must | Implemented | `.github/workflows/build-and-test.yml`. |
| DASHR-TECH-003 | CI runs an end-to-end suite against a real Herdr server and a real Grafana container. | Must | Approved | Pending |
| DASHR-TECH-004 | Tagged releases publish Linux and macOS binaries with SHA-256 checksums. | Must | Approved | Pending |

## 9. Security requirements

| ID | Requirement | Status | Evidence |
|---|---|---|---|
| DASHR-SEC-001 | No unmasked datasource value is returned by any MCP tool. | Implemented | `dashr_mcp::tools` routes every datasource value through `Masker`. |
| DASHR-SEC-002 | Grafana listens on loopback only. | Verified | `run_args_store_nothing_and_bind_loopback`. |
| DASHR-SEC-003 | No secret is written to a file or passed in argv. | Verified | `secrets_are_passed_by_name_only`, `secrets_are_env_references_never_values`. |
| DASHR-SEC-004 | The container cannot persist data or escalate: read-only root, tmpfs, no swap, no capabilities, no-new-privileges. | Verified | `run_args_store_nothing_and_bind_loopback`. |
| DASHR-SEC-005 | The browser profile is ephemeral. | Implemented | `open_command_points_the_profile_at_the_runtime_dir`. |
| DASHR-SEC-006 | Screenshots of dashboards touching personal datasources are refused. | Verified | `screenshots_only_for_non_personal_dashboards`. |
| DASHR-SEC-007 | Herdr pane state (messages, tokens) carries counts and rule wording only, because Herdr's socket has no caller authentication (#514). | Implemented | `monitor::report`. |
| DASHR-SEC-008 | The reaper only stops containers labelled with this Herdr server's socket hash. | Verified | `orphans_are_this_servers_containers_without_a_live_pane`, `session_ids_differ_between_servers_for_the_same_pane`. |

## 10. Non-functional requirements

- Dashboard pane ready (Grafana healthy, first dashboard applied) within the
  configured timeout, 90 s by default; typically under 15 s with a pulled image.
- Memory: Grafana limited to `grafana.memory` (768 MiB by default).
- The binary is a single executable with no runtime dependency beyond
  `docker` and, optionally, `terminal-browser`, the agent CLI and `aws`.

## 11. Testing strategy

- **Unit tests** per crate for every pure function, including the masking
  corpus and every argv builder.
- **HTTP tests** against an in-process server for the Grafana client.
- **Fake CLIs** (shell scripts) for `herdr`, `aws` and error paths.
- **End-to-end** (M3): real Herdr server, real Grafana container, plugin
  linked, action invoked, MCP driven over stdio, pane closed, container gone.

## 12. Decisions

| ID | Decision |
|---|---|
| DEC-001 | Rust instead of the .NET stack in the handoff, at the owner's request. |
| DEC-002 | Plugin id `herdr-dashr`, binary `dashr`, requirement prefix `DASHR-`. |
| DEC-003 | Traceability runs on `pull_request_target` from the trusted base; build and test on `pull_request` with a read-only token. |
| DEC-004 | Minimum Herdr 0.9.0; development and CI verified against 0.9.1. |
| DEC-005 | Herdr is driven through its CLI (`HERDR_BIN_PATH`), not the raw socket. |
| DEC-006 | Toolchain pinned to 1.94.1. |
| DEC-007 | Blocking HTTP (`ureq`); no async runtime. |
| DEC-008 | Custom image installs plugins to `/usr/share/grafana/plugins-dashr`, because `/var/lib/grafana` is a tmpfs. |
| DEC-009 | Session ids and container labels include a hash of the Herdr socket path; pane ids repeat across Herdr sessions. |
| DEC-010 | Provisioning is emitted as JSON in a `.yaml` file (JSON is YAML). |
| DEC-011 | `mcp-grafana` is opt-in: its query tools return raw rows and bypass masking. dashr provides `probe_query` for masked discovery. |
| DEC-012 | Seq is read through the Infinity datasource with an `X-Seq-ApiKey` header. |
| DEC-013 | `/api/ds/query` is called once per query: one unknown datasource fails a whole request. |
| DEC-014 | AWS through the `aws` CLI (SSO, profiles, MFA as configured), not an SDK. |
| DEC-015 | Watches are evaluated by the pane, not by Grafana alerting, so values stay local and no alerting state is persisted. |
| DEC-016 | The MCP protocol is hand-rolled (four methods) rather than an SDK dependency. |

## 13. Open questions and risks

| ID | Question or risk |
|---|---|
| OQ-001 | Kitty-graphics rendering varies by terminal (Herdr #3018 WezTerm, #3941 iTerm2, #3697/#3676). The text view is the fallback. |
| OQ-002 | macOS has no user tmpfs; runtime files use the private per-user temp directory and are deleted on stop. |
| OQ-003 | terminal-browser runs a shared daemon; whether `TERMINAL_BROWSER_APPDATA` applies when the daemon is already running is unverified. |
| OQ-004 | Anonymous admin on loopback: any local process can reach the Grafana while the pane lives. Accepted for a single-user workstation. |
| OQ-005 | macOS input helper of terminal-browser may need accessibility permission on managed machines. |
| OQ-006 | CloudWatch Live Tail and Loki tail are not Grafana-native streams; panels refresh on an interval instead. |

## 14. Delivery ledger

| Date | Change | Requirements |
|---|---|---|
| 2026-09-26 | M0/M1: PRD, CI, governance; library crates `dashr-core`, `dashr-grafana`, `dashr-docker`, `dashr-herdr`, `dashr-aws`, `dashr-runtime`, `dashr-mcp` with unit tests. | GOV-001..003, TECH-001..002, GRAF-*, DS-*, PRIV-*, AWS-*, MCP-*, ALERT-*, PROMO-*, SEC-* |

### Requirement completion summary

| Area | Total | Verified | Implemented | Other |
|---|---|---|---|---|
| HERDR | 9 | 0 | 5 | 4 |
| GRAF | 8 | 1 | 6 | 1 |
| DS | 6 | 5 | 1 | 0 |
| VIEW | 4 | 0 | 3 | 1 |
| CHAT | 3 | 0 | 1 | 2 |
| MCP | 10 | 2 | 8 | 0 |
| PRIV | 7 | 7 | 0 | 0 |
| AWS | 6 | 4 | 2 | 0 |
| ALERT | 4 | 0 | 4 | 0 |
| PROMO | 3 | 1 | 2 | 0 |
| GOV/TECH | 7 | 1 | 4 | 2 |
| SEC | 8 | 5 | 3 | 0 |

## 15. Acceptance criteria

- **AC-OPEN** Invoking `open` in Herdr yields a tab with a dashboard pane
  and a chat pane, a healthy Grafana on loopback, and the welcome dashboard.
- **AC-CLOSE** Closing the dashboard pane leaves no container, runtime
  directory or session file.
- **AC-MASK** No MCP tool output contains a value planted in TestData CSV
  personal fields.
- **AC-PIPELINE** A CodePipeline URL yields the proposed dashboard.
- **AC-ALERT** A breached watch marks the pane blocked and notifies once.
