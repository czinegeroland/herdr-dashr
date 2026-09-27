//! The page the dashboard pane links to (DEC-041).
//!
//! The human sees one thing: the dashboard. Grafana's shared ("public")
//! dashboard page renders it with nothing of Grafana around it — no menus,
//! no edit, no way out, not even with Esc — but it does not follow changes
//! the way the normal page does. So the pane serves this page on a loopback
//! port: the shared dashboard filling the window, reloaded within about two
//! seconds of every save (`/version` is the saved dashboard version). A
//! reload also picks up a time range the agent set, such as log checks'
//! "since arming".

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Serves the page until `stop`; returns the port.
pub fn serve(
    grafana: String,
    uid: String,
    token: String,
    stop: Arc<AtomicBool>,
) -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let page = page(&format!("{grafana}/public-dashboards/{token}"));
    std::thread::spawn(move || {
        let client = dashr_grafana::Client::local(&grafana);
        while !stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = answer(stream, &page, || {
                        client.dashboard_version(&uid).ok().map(|v| v.to_string())
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(200)),
            }
        }
    });
    Ok(port)
}

/// Answers one request: `/` is the page, `/version` the dashboard version.
fn answer(
    mut stream: TcpStream,
    page: &str,
    version: impl FnOnce() -> Option<String>,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let path = line.split_whitespace().nth(1).unwrap_or("/");
    let (status, kind, body) = match path.split('?').next().unwrap_or("/") {
        "/" => ("200 OK", "text/html; charset=utf-8", page.to_owned()),
        "/version" => match version() {
            Some(version) => ("200 OK", "text/plain", version),
            None => ("503 Service Unavailable", "text/plain", String::new()),
        },
        _ => ("404 Not Found", "text/plain", String::new()),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

/// The page: the shared dashboard, reloaded when the saved version changes.
pub fn page(src: &str) -> String {
    // A JSON string, with `</` broken up so it cannot end the script.
    let src = serde_json::to_string(src)
        .unwrap_or_else(|_| "\"\"".into())
        .replace("</", "<\\/");
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>dashr</title>
<style>html,body{{margin:0;height:100%;background:#111217;overflow:hidden}}iframe{{border:0;width:100%;height:100%;display:block}}</style>
</head><body><iframe id="dashboard"></iframe>
<script>
const src = {src};
const frame = document.getElementById("dashboard");
frame.src = src;
let seen = null;
window.__dashrReloads = 0;
async function watch() {{
  try {{
    const answer = await fetch("/version", {{cache: "no-store"}});
    if (answer.ok) {{
      const version = await answer.text();
      if (seen !== null && version !== seen) {{
        frame.src = src + "?reload=" + Date.now();
        window.__dashrReloads += 1;
      }}
      seen = version;
    }}
  }} catch (error) {{}}
  setTimeout(watch, 2000);
}}
watch();
</script></body></html>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn get(port: u16, path: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn serves_the_page_and_nothing_else() {
        let stop = Arc::new(AtomicBool::new(false));
        // No Grafana behind it: `/version` is unavailable, the page is not.
        let port = serve(
            "http://127.0.0.1:9".into(),
            "d".into(),
            "tok".into(),
            Arc::clone(&stop),
        )
        .unwrap();
        let page = get(port, "/");
        assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
        assert!(page.contains(r#"const src = "http://127.0.0.1:9/public-dashboards/tok";"#));
        assert!(get(port, "/version").starts_with("HTTP/1.1 503"));
        assert!(get(port, "/api/admin/users").starts_with("HTTP/1.1 404"));
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn the_page_escapes_its_source() {
        let page = page("a\"</script>");
        assert!(page.contains(r#"const src = "a\"<\/script>";"#), "{page}");
    }
}
