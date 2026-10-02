//! Reports to attach to a pull request (DASHR-EXPORT-001).
//!
//! A report is one trace and, when a flow judged it, the flow's verdict:
//! Markdown for a PR description (GitHub draws the Mermaid sequence
//! diagram), a standalone HTML page, or JSON. Whoever builds the report
//! decides about masking: the agent's export is always masked, the human
//! chooses in the viewer.

use std::collections::HashMap;

use serde::Serialize;

use crate::flow::{Flow, StepStatus, Verdict, VerdictStatus};
use crate::model::Span;
use crate::privacy::{Masker, Pseudonyms};
use crate::sequence::{self, MessageKind};
use crate::store::Summary;

/// Messages beyond this are left out of the diagram (the text keeps all).
pub const MAX_DIAGRAM_MESSAGES: usize = 150;

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub title: String,
    pub generated: String,
    pub version: String,
    pub masked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<Summary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow: Option<Flow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mermaid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<String>,
    /// For JSON: the trace's spans (masked when `masked`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spans: Option<serde_json::Value>,
}

/// Masks text when there is a masker, keeping pseudonyms consistent.
pub struct Mask<'a> {
    masker: Option<&'a Masker>,
    pseudonyms: Pseudonyms,
}

impl<'a> Mask<'a> {
    pub fn new(masker: Option<&'a Masker>) -> Self {
        Self {
            masker,
            pseudonyms: Pseudonyms::default(),
        }
    }

    pub fn text(&mut self, text: &str) -> String {
        match self.masker {
            Some(masker) => masker.text(text, &mut self.pseudonyms),
            None => text.to_owned(),
        }
    }
}

fn mermaid_text(text: &str) -> String {
    let short: String = if text.chars().count() > 80 {
        format!("{}…", text.chars().take(79).collect::<String>())
    } else {
        text.to_owned()
    };
    let mut clean = String::new();
    for c in short.chars() {
        match c {
            '#' => clean.push_str("#35;"),
            ';' => clean.push_str("#59;"),
            '<' => clean.push_str("#lt;"),
            '>' => clean.push_str("#gt;"),
            c if c.is_control() => clean.push(' '),
            c => clean.push(c),
        }
    }
    clean
}

fn ms(ns: u64) -> String {
    let v = ns as f64 / 1e6;
    if v >= 1000.0 {
        format!("{:.2} s", v / 1000.0)
    } else if v >= 10.0 {
        format!("{v:.0} ms")
    } else {
        format!("{v:.1} ms")
    }
}

/// The trace as a Mermaid sequence diagram: calls, their returns with the
/// duration, asynchronous messages, self-calls, errors marked.
pub fn mermaid(spans: &[Span], mask: &mut Mask) -> String {
    let seq = sequence::build(spans);
    let by_id: HashMap<&str, &Span> = spans.iter().map(|s| (s.span_id.as_str(), s)).collect();
    let alias: HashMap<&str, String> = seq
        .participants
        .iter()
        .enumerate()
        .map(|(i, p)| (p.as_str(), format!("P{i}")))
        .collect();
    let mut out = String::from("sequenceDiagram\n");
    for p in &seq.participants {
        let kind = if p == sequence::CALLER {
            "actor"
        } else {
            "participant"
        };
        out.push_str(&format!(
            "    {kind} {} as {}\n",
            alias[p.as_str()],
            mermaid_text(p)
        ));
    }
    // Calls, then their returns, in time order (as the viewer draws them).
    let mut events: Vec<(u64, u8, usize)> = Vec::new();
    for (i, m) in seq.messages.iter().enumerate() {
        events.push((m.start_ns, 0, i));
        if m.kind == MessageKind::Call && m.from != m.to {
            events.push((m.end_ns, 1, i));
        }
    }
    events.sort();
    let total = events.len();
    for (shown, (_, kind, i)) in events.into_iter().enumerate() {
        if shown >= MAX_DIAGRAM_MESSAGES {
            out.push_str(&format!(
                "    Note over {}: {} more messages, see the text sequence\n",
                alias[seq.participants[0].as_str()],
                total - shown
            ));
            break;
        }
        let m = &seq.messages[i];
        let (from, to) = (&alias[m.from.as_str()], &alias[m.to.as_str()]);
        let duration = ms(m.end_ns.saturating_sub(m.start_ns));
        let mark = if m.error { "❌ " } else { "" };
        if kind == 1 {
            let reason = if m.error {
                by_id
                    .get(m.span_id.as_str())
                    .and_then(|s| s.status_message.as_deref())
                    .map(|r| format!("❌ {} · ", mask.text(r)))
                    .unwrap_or_else(|| "❌ ".into())
            } else {
                String::new()
            };
            out.push_str(&format!(
                "    {to}-->>{from}: {}\n",
                mermaid_text(&format!("{reason}{duration}"))
            ));
            continue;
        }
        let label = mermaid_text(&format!("{mark}{}", mask.text(&m.label)));
        match m.kind {
            MessageKind::Call if m.from != m.to => {
                out.push_str(&format!("    {from}->>{to}: {label}\n"))
            }
            MessageKind::Async => out.push_str(&format!("    {from}-){to}: {label}\n")),
            _ => out.push_str(&format!("    {from}->>{from}: {label} ({duration})\n")),
        }
    }
    out
}

fn step_icon(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Ok => "✅",
        StepStatus::Skipped => "⏭️",
        StepStatus::Missing => "⬜",
        _ => "❌",
    }
}

fn status_name(status: StepStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.replace('_', " ")))
        .unwrap_or_default()
}

fn verdict_head(verdict: &Verdict) -> (&'static str, &'static str) {
    match verdict.status {
        VerdictStatus::Pass => ("✅", "passed"),
        VerdictStatus::Fail => ("❌", "failed"),
        VerdictStatus::Running => ("⏳", "running"),
        VerdictStatus::Waiting => ("⬜", "waiting for a run"),
    }
}

fn md_cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

/// Markdown for a pull request description or comment.
pub fn markdown(report: &Report) -> String {
    let mut out = String::new();
    match (&report.flow, &report.verdict) {
        (Some(flow), Some(verdict)) => {
            let (icon, word) = verdict_head(verdict);
            out.push_str(&format!("## {icon} dashr: `{}` {word}\n\n", flow.name));
            if let Some(description) = &flow.description {
                out.push_str(&format!("{description}\n\n"));
            }
            out.push_str(&format!("**{}**\n\n", md_cell(&verdict.summary)));
            let code: HashMap<&str, &str> = flow
                .steps
                .iter()
                .filter_map(|s| Some((s.id.as_str(), s.code.as_deref()?)))
                .collect();
            out.push_str(
                "| | Step | Service | Span | Duration | Notes |\n|---|---|---|---|---:|---|\n",
            );
            for step in &verdict.steps {
                let mut notes = step.problems.join("; ");
                if let Some(code) = code.get(step.id.as_str()) {
                    if !notes.is_empty() {
                        notes.push_str(" · ");
                    }
                    notes.push_str(&format!("`{code}`"));
                }
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {} |\n",
                    step_icon(step.status),
                    md_cell(&step.id),
                    md_cell(&step.service),
                    md_cell(&step.span),
                    step.duration_ms
                        .map(|d| format!("{d:.1} ms"))
                        .unwrap_or_default(),
                    if notes.is_empty() {
                        status_name(step.status)
                    } else {
                        md_cell(&notes)
                    }
                ));
            }
            out.push('\n');
            for problem in &verdict.unexpected_errors {
                out.push_str(&format!(
                    "- ❌ unexpected error in `{}` · {}: {}\n",
                    problem.service,
                    md_cell(&problem.span),
                    md_cell(&problem.message)
                ));
            }
            for problem in &verdict.forbidden {
                out.push_str(&format!(
                    "- ⛔ forbidden span `{}` · {}\n",
                    problem.service,
                    md_cell(&problem.span)
                ));
            }
            if !verdict.unexpected_errors.is_empty() || !verdict.forbidden.is_empty() {
                out.push('\n');
            }
        }
        _ => out.push_str(&format!("## 🔎 dashr: {}\n\n", md_cell(&report.title))),
    }
    if let Some(s) = &report.summary {
        out.push_str(&format!(
            "Trace `{}` · {} spans · {} services ({}) · {:.1} ms · {} errors{}\n\n",
            s.trace_id,
            s.spans,
            s.services.len(),
            s.services.join(", "),
            s.duration_ms,
            s.errors,
            if s.orphans > 0 {
                format!(" · {} spans lost their parent", s.orphans)
            } else {
                String::new()
            }
        ));
    }
    if let Some(mermaid) = &report.mermaid {
        out.push_str("<details open><summary>Sequence diagram</summary>\n\n```mermaid\n");
        out.push_str(mermaid);
        out.push_str("```\n\n</details>\n\n");
    }
    if let Some(sequence) = &report.sequence {
        out.push_str("<details><summary>Sequence as text</summary>\n\n```text\n");
        out.push_str(sequence);
        out.push_str("```\n\n</details>\n\n");
    }
    out.push_str(&format!(
        "<sub>Exported by [dashr](https://github.com/czinegeroland/herdr-dashr) {} at {}{}</sub>\n",
        report.version,
        report.generated,
        if report.masked {
            " · personal data masked"
        } else {
            " · raw values"
        }
    ));
    out
}

pub fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const HTML_STYLE: &str = r#"
:root { color-scheme: light; --bg: #f4f5f8; --panel: #fff; --ink: #14161c; --muted: #646a78; --line: #e2e5ec;
  --accent: #4f5bd5; --accent2: #7c4dd8; --ok: #138a52; --ok-soft: #e2f5ea; --bad: #d1372b; --bad-soft: #fde8e6; --warn: #b26a00; --warn-soft: #fff1d9; }
@media (prefers-color-scheme: dark) { :root { color-scheme: dark; --bg: #0d0f14; --panel: #161a22; --ink: #e8eaf0; --muted: #9aa1b2; --line: #262b36;
  --accent: #8b95ff; --accent2: #b28cff; --ok: #4cd68e; --ok-soft: #12301f; --bad: #ff7468; --bad-soft: #3a1714; --warn: #f5b95a; --warn-soft: #3a2a10; } }
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--ink); font: 14px/1.55 Inter, system-ui, -apple-system, "Segoe UI", sans-serif; }
main { max-width: 1100px; margin: 0 auto; padding: 32px 20px 60px; }
header { display: flex; align-items: center; gap: 12px; margin-bottom: 22px; }
header .logo { width: 34px; height: 34px; border-radius: 10px; background: linear-gradient(135deg, var(--accent), var(--accent2)); }
h1 { font-size: 22px; margin: 0; letter-spacing: -.3px; } .sub { color: var(--muted); font-size: 13px; }
.card { background: var(--panel); border: 1px solid var(--line); border-radius: 14px; padding: 16px 18px; margin-bottom: 16px; box-shadow: 0 1px 2px rgba(0,0,0,.05), 0 4px 16px rgba(0,0,0,.04); }
.verdict { display: flex; align-items: center; gap: 10px; font-weight: 600; border-radius: 12px; padding: 12px 16px; margin-bottom: 16px; }
.pass { background: var(--ok-soft); color: var(--ok); } .fail { background: var(--bad-soft); color: var(--bad); } .wait { background: var(--panel); color: var(--muted); border: 1px dashed var(--line); } .running { background: var(--warn-soft); color: var(--warn); }
.stats { display: grid; grid-template-columns: repeat(auto-fit, minmax(140px, 1fr)); gap: 10px; margin-bottom: 16px; }
.stat { background: var(--panel); border: 1px solid var(--line); border-radius: 12px; padding: 10px 14px; }
.stat b { display: block; font-size: 20px; } .stat span { font-size: 11px; color: var(--muted); text-transform: uppercase; letter-spacing: .8px; font-weight: 600; }
table { border-collapse: collapse; width: 100%; font-size: 13px; }
th { text-align: left; font-size: 11px; text-transform: uppercase; letter-spacing: .7px; color: var(--muted); padding: 8px 10px; border-bottom: 1px solid var(--line); }
td { padding: 8px 10px; border-top: 1px solid var(--line); vertical-align: top; }
code, pre { font-family: "JetBrains Mono", ui-monospace, Menlo, Consolas, monospace; font-size: 12px; }
pre.text { overflow-x: auto; white-space: pre; margin: 0; }
.problems { color: var(--bad); }
.mermaid { overflow-x: auto; text-align: center; }
details summary { cursor: pointer; font-weight: 600; }
footer { color: var(--muted); font-size: 12px; margin-top: 24px; }
"#;

/// A standalone page: the verdict, the steps, the diagram (Mermaid from
/// jsDelivr; the text sequence when offline).
pub fn html(report: &Report) -> String {
    let e = html_escape;
    let mut body = String::new();
    body.push_str(&format!(
        "<header><div class=\"logo\"></div><div><h1>{}</h1><div class=\"sub\">dashr {} · {}{}</div></div></header>\n",
        e(&report.title),
        e(&report.version),
        e(&report.generated),
        if report.masked { " · personal data masked" } else { " · raw values" }
    ));
    if let (Some(flow), Some(verdict)) = (&report.flow, &report.verdict) {
        let (icon, word) = verdict_head(verdict);
        let class = match verdict.status {
            VerdictStatus::Pass => "pass",
            VerdictStatus::Fail => "fail",
            VerdictStatus::Running => "running",
            VerdictStatus::Waiting => "wait",
        };
        body.push_str(&format!(
            "<div class=\"verdict {class}\">{icon} Flow <code>{}</code> {word} — {}</div>\n",
            e(&flow.name),
            e(&verdict.summary)
        ));
        let code: HashMap<&str, &str> = flow
            .steps
            .iter()
            .filter_map(|s| Some((s.id.as_str(), s.code.as_deref()?)))
            .collect();
        body.push_str("<div class=\"card\"><table><tr><th></th><th>Step</th><th>Service</th><th>Span</th><th>Duration</th><th>Notes</th></tr>\n");
        for step in &verdict.steps {
            body.push_str(&format!(
                "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td><td>{}{}</td></tr>\n",
                step_icon(step.status),
                e(&step.id),
                e(&step.service),
                e(&step.span),
                step.duration_ms.map(|d| format!("{d:.1} ms")).unwrap_or_default(),
                if step.problems.is_empty() {
                    e(&status_name(step.status))
                } else {
                    format!("<span class=\"problems\">{}</span>", e(&step.problems.join("; ")))
                },
                code.get(step.id.as_str())
                    .map(|c| format!(" <code>{}</code>", e(c)))
                    .unwrap_or_default()
            ));
        }
        body.push_str("</table>");
        for problem in &verdict.unexpected_errors {
            body.push_str(&format!(
                "<p class=\"problems\">❌ unexpected error in {} · {}: {}</p>",
                e(&problem.service),
                e(&problem.span),
                e(&problem.message)
            ));
        }
        for problem in &verdict.forbidden {
            body.push_str(&format!(
                "<p class=\"problems\">⛔ forbidden span {} · {}</p>",
                e(&problem.service),
                e(&problem.span)
            ));
        }
        body.push_str("</div>\n");
    }
    if let Some(s) = &report.summary {
        body.push_str(&format!(
            "<div class=\"stats\"><div class=\"stat\"><b>{:.1} ms</b><span>duration</span></div><div class=\"stat\"><b>{}</b><span>spans</span></div><div class=\"stat\"><b>{}</b><span>services</span></div><div class=\"stat\"><b>{}</b><span>errors</span></div><div class=\"stat\"><b>{}</b><span>lost parents</span></div></div>\n<p class=\"sub\">Trace <code>{}</code> · {}</p>\n",
            s.duration_ms, s.spans, s.services.len(), s.errors, s.orphans, e(&s.trace_id), e(&s.services.join(", "))
        ));
    }
    if let Some(mermaid) = &report.mermaid {
        body.push_str(&format!(
            "<div class=\"card\"><pre class=\"mermaid\">{}</pre></div>\n",
            e(mermaid)
        ));
    }
    if let Some(sequence) = &report.sequence {
        body.push_str(&format!(
            "<div class=\"card\"><details><summary>Sequence as text</summary><pre class=\"text\">{}</pre></details></div>\n",
            e(sequence)
        ));
    }
    body.push_str("<footer>Exported by <a href=\"https://github.com/czinegeroland/herdr-dashr\">dashr</a>, end-to-end testing by traces.</footer>\n");
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{}</title>\n<style>{HTML_STYLE}</style>\n</head>\n<body>\n<main>\n{body}</main>\n<script type=\"module\">\ntry {{\n  const {{ default: mermaid }} = await import(\"https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs\");\n  mermaid.initialize({{ startOnLoad: false, theme: matchMedia(\"(prefers-color-scheme: dark)\").matches ? \"dark\" : \"default\", sequence: {{ mirrorActors: false }} }});\n  await mermaid.run({{ querySelector: \".mermaid\" }});\n}} catch {{ for (const el of document.querySelectorAll(\".mermaid\")) el.closest(\".card\").style.display = \"none\"; document.querySelector(\"details\")?.setAttribute(\"open\", \"\"); }}\n</script>\n</body>\n</html>\n",
        e(&format!("dashr · {}", report.title))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SpanKind, Status};
    use crate::store::tests::span;
    use serde_json::json;

    fn spans() -> Vec<Span> {
        let mut root = span(
            "a",
            "1",
            None,
            "orders-api",
            "POST /orders; ann@example.com",
            0,
            100_000_000,
        );
        root.kind = SpanKind::Server;
        let mut call = span(
            "a",
            "2",
            Some("1"),
            "orders-api",
            "PUT /stock",
            1_000_000,
            40_000_000,
        );
        call.kind = SpanKind::Client;
        let mut serve = span(
            "a",
            "3",
            Some("2"),
            "stock-api",
            "PUT /stock",
            2_000_000,
            39_000_000,
        );
        serve.kind = SpanKind::Server;
        serve.status = Status::Error;
        serve.status_message = Some("out of stock for bob@example.com".into());
        vec![root, call, serve]
    }

    #[test]
    fn mermaid_draws_calls_returns_and_errors_masked() {
        let masker = Masker::new(&Default::default());
        let text = mermaid(&spans(), &mut Mask::new(Some(&masker)));
        assert!(
            text.starts_with("sequenceDiagram\n    actor P0 as caller\n"),
            "{text}"
        );
        assert!(text.contains("participant P1 as orders-api"));
        assert!(
            text.contains("P0->>P1: POST /orders#59; #lt;email#35;1#gt;"),
            "{text}"
        );
        assert!(text.contains("P1->>P2: ❌ PUT /stock"), "{text}");
        assert!(
            text.contains("P2-->>P1: ❌ out of stock for #lt;email#35;2#gt; · 37 ms"),
            "{text}"
        );
        assert!(!text.contains("example.com"));
        let raw = mermaid(&spans(), &mut Mask::new(None));
        assert!(
            raw.contains("bob@example.com"),
            "the human may export raw values"
        );
    }

    #[test]
    fn markdown_and_html_carry_the_verdict() {
        let masker = Masker::new(&Default::default());
        let flow = Flow::parse(&json!({"name": "checkout", "steps": [
            {"id": "api", "service": "orders-api", "span": "POST /orders*", "code": "src/orders.py:create"},
            {"id": "stock", "service": "stock-api", "span": "PUT /stock"}
        ]}).to_string()).unwrap();
        let s = spans();
        let candidates = [crate::flow::Candidate {
            spans: &s,
            updated_ms: 0,
        }];
        let verdict = crate::flow::evaluate(&flow, &candidates, 1_000_000, &masker);
        let report = Report {
            title: "checkout".into(),
            generated: "2026-10-01T22:00:00Z".into(),
            version: "2.0.2".into(),
            masked: true,
            summary: None,
            flow: Some(flow),
            verdict: Some(verdict),
            mermaid: Some(mermaid(&s, &mut Mask::new(Some(&masker)))),
            sequence: Some(sequence::text(&s, &masker)),
            spans: None,
        };
        let md = markdown(&report);
        assert!(md.starts_with("## ❌ dashr: `checkout` failed"), "{md}");
        assert!(
            md.contains("| ✅ | api | orders-api | POST /orders* |"),
            "{md}"
        );
        assert!(md.contains("`src/orders.py:create`"));
        assert!(md.contains("```mermaid\nsequenceDiagram"));
        assert!(md.contains("personal data masked"));
        assert!(!md.contains("example.com"), "{md}");
        let page = html(&report);
        assert!(page.starts_with("<!doctype html>"));
        assert!(page.contains("<pre class=\"mermaid\">sequenceDiagram"));
        assert!(page.contains("Flow <code>checkout</code> failed"));
        assert!(!page.contains("example.com"));
    }

    #[test]
    fn labels_cannot_break_the_diagram() {
        assert_eq!(mermaid_text("a;b#c<d>\ne"), "a#59;b#35;c#lt;d#gt; e");
        assert_eq!(mermaid_text(&"x".repeat(100)).chars().count(), 80);
        assert_eq!(
            mermaid_text(&format!("{};", "x".repeat(100))),
            format!("{}…", "x".repeat(79))
        );
    }
}
