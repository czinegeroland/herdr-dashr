use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// A recorded request: method, path, headers (lowercased names), body.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// A one-thread HTTP server answering from a routing function.
fn serve(
    route: impl Fn(&str, &str, &str) -> (u16, String) + Send + 'static,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("").to_owned();
            let path = parts.next().unwrap_or("").to_owned();
            let mut headers = Vec::new();
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end().to_owned();
                if header.is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':') {
                    let name = name.trim().to_ascii_lowercase();
                    let value = value.trim().to_owned();
                    if name == "content-length" {
                        length = value.parse().unwrap_or(0);
                    }
                    headers.push((name, value));
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body = String::from_utf8_lossy(&body).into_owned();
            let (status, answer) = route(&method, &path, &body);
            log.lock().unwrap().push(Seen {
                method,
                path,
                headers,
                body,
            });
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                answer.len()
            );
        }
    });
    (base, seen)
}

#[test]
fn health_and_datasources() {
    let (base, _) = serve(|_, path, _| {
        match path {
        "/api/health" => (200, r#"{"database":"ok"}"#.into()),
        "/api/datasources" => (
            200,
            r#"[{"uid":"dashr-testdata","name":"TestData","type":"grafana-testdata-datasource","url":"x"}]"#
                .into(),
        ),
        _ => (404, r#"{"message":"not found"}"#.into()),
    }
    });
    let client = Client::local(&base);
    assert!(client.healthy());
    client.wait_healthy(Duration::from_secs(2)).unwrap();
    let datasources = client.datasources().unwrap();
    assert_eq!(datasources[0].uid, "dashr-testdata");
}

#[test]
fn unreachable_and_timeout() {
    let client = Client::local("http://127.0.0.1:1");
    assert!(!client.healthy());
    assert!(matches!(
        client.wait_healthy(Duration::from_millis(600)),
        Err(GrafanaError::Timeout(_))
    ));
    assert!(matches!(
        client.datasources(),
        Err(GrafanaError::Unreachable { .. })
    ));
}

#[test]
fn save_dashboard_sends_overwrite_and_folder_and_reports_errors() {
    let (base, seen) = serve(|_, path, body| {
        if path == "/api/dashboards/db" && body.contains("\"bad\"") {
            (412, r#"{"message":"version-mismatch"}"#.into())
        } else {
            (
                200,
                r#"{"uid":"u1","url":"/d/u1/x","version":2,"status":"success"}"#.into(),
            )
        }
    });
    let client = Client::local(&base);
    let saved = client
        .save_dashboard(&json!({"title": "t"}), Some("f1"), true, "m")
        .unwrap();
    assert_eq!(saved.uid, "u1");
    let body: Value = serde_json::from_str(&seen.lock().unwrap()[0].body).unwrap();
    assert_eq!(body["overwrite"], true);
    assert_eq!(body["folderUid"], "f1");
    let error = client
        .save_dashboard(&json!({"title": "bad"}), None, false, "m")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("412") && error.contains("version-mismatch"),
        "{error}"
    );
}

#[test]
fn queries_run_one_request_each_and_isolate_failures() {
    let (base, seen) = serve(|_, _, body| {
        if body.contains("\"nope\"") {
            (400, r#"{"message":"Data source not found"}"#.into())
        } else {
            (
                200,
                r#"{"results":{"A":{"status":200,"frames":[{"schema":{"fields":[{"name":"v","type":"number"}]},"data":{"values":[[1,2]]}}]}}}"#
                    .into(),
            )
        }
    });
    let client = Client::local(&base);
    let results = client.query(
        &[
            json!({"refId": "A", "datasource": {"uid": "dashr-testdata"}}),
            json!({"refId": "B", "datasource": {"uid": "nope"}}),
            json!({"refId": "C", "datasource": {"uid": "${ds}"}}),
        ],
        "now-1h",
        "now",
    );
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].row_count(), 2);
    assert_eq!(results[1].error.as_deref(), Some("Data source not found"));
    assert!(results[2].error.as_deref().unwrap().contains("variable"));
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2, "variable queries are not sent");
    let first: Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(first["from"], "now-1h");
    assert_eq!(first["queries"][0]["maxDataPoints"], 500);
}

#[test]
fn token_goes_in_a_header_and_never_in_debug_output() {
    let (base, seen) = serve(|_, path, _| {
        if path.starts_with("/api/folders") && path.contains("limit") {
            (200, r#"[{"uid":"existing","title":"other"}]"#.into())
        } else if path == "/api/folders" {
            (200, r#"{"uid":"new-folder","title":"dashr"}"#.into())
        } else {
            (404, "{}".into())
        }
    });
    let client = Client::with_token(&base, "glsa_secret_value".into());
    assert!(!format!("{client:?}").contains("glsa_secret_value"));
    assert_eq!(client.ensure_folder("dashr").unwrap(), "new-folder");
    let requests = seen.lock().unwrap();
    assert!(requests.iter().all(|request| {
        request
            .headers
            .iter()
            .any(|(name, value)| name == "authorization" && value == "Bearer glsa_secret_value")
            && !request.path.contains("glsa")
    }));
    assert_eq!(requests[1].method, "POST");
}

#[test]
fn helpers() {
    assert_eq!(
        encode_path("CloudWatch (eu-west-1)"),
        "CloudWatch%20%28eu-west-1%29"
    );
    assert_eq!(
        time_range(&json!({"time": {"from": "now-6h", "to": "now-1h"}})),
        ("now-6h".to_owned(), "now-1h".to_owned())
    );
    assert_eq!(
        time_range(&json!({})),
        ("now-1h".to_owned(), "now".to_owned())
    );
}
