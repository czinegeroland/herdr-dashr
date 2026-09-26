//! JSON-RPC 2.0 over stdio, as MCP specifies it.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

/// Protocol revisions this server speaks, newest first.
pub const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// What a tool call produced.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutput {
    /// Structured JSON, sent as text content and `structuredContent`.
    Json(Value),
    /// An image, base64 encoded.
    Image { data: String, mime_type: String },
    /// A tool-level error the agent should read and act on.
    Error(String),
}

/// The tools a server exposes, and optional read-only text resources.
pub trait Tools {
    fn definitions(&self) -> Vec<Value>;
    fn call(&mut self, name: &str, arguments: &Value) -> ToolOutput;

    /// Resource descriptors (`uri`, `name`, `mimeType`, ...). None by default.
    fn resources(&self) -> Vec<Value> {
        Vec::new()
    }

    /// The text of a resource, with its MIME type.
    fn read_resource(&self, _uri: &str) -> Option<(String, String)> {
        None
    }
}

pub struct Server<T: Tools> {
    tools: T,
    name: String,
    version: String,
    instructions: String,
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

impl<T: Tools> Server<T> {
    pub fn new(tools: T, name: &str, version: &str, instructions: &str) -> Self {
        Self {
            tools,
            name: name.to_owned(),
            version: version.to_owned(),
            instructions: instructions.to_owned(),
        }
    }

    pub fn tools_mut(&mut self) -> &mut T {
        &mut self.tools
    }

    fn tool_result(output: ToolOutput) -> Value {
        match output {
            ToolOutput::Json(value) => json!({
                "content": [{"type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default()}],
                "structuredContent": if value.is_object() { value.clone() } else { json!({"result": value}) },
                "isError": false
            }),
            ToolOutput::Image { data, mime_type } => json!({
                "content": [{"type": "image", "data": data, "mimeType": mime_type}],
                "isError": false
            }),
            ToolOutput::Error(message) => json!({
                "content": [{"type": "text", "text": message}],
                "isError": true
            }),
        }
    }

    /// Handles one message; `None` for notifications, which get no answer.
    pub fn handle(&mut self, message: &Value) -> Option<Value> {
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str);
        let Some(method) = method else {
            // A response or garbage; a response to us needs no answer.
            return id.map(|id| error(&id, -32600, "invalid request"));
        };
        let Some(id) = id else {
            // Notifications: `notifications/initialized`, cancellations.
            return None;
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        Some(match method {
            "initialize" => {
                let requested = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(SUPPORTED_VERSIONS[0]);
                let version = if SUPPORTED_VERSIONS.contains(&requested) {
                    requested
                } else {
                    SUPPORTED_VERSIONS[0]
                };
                let mut capabilities = json!({"tools": {"listChanged": false}});
                if !self.tools.resources().is_empty() {
                    capabilities["resources"] = json!({"listChanged": false, "subscribe": false});
                }
                result(
                    &id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": capabilities,
                        "serverInfo": {"name": self.name, "version": self.version},
                        "instructions": self.instructions
                    }),
                )
            }
            "ping" => result(&id, json!({})),
            "tools/list" => result(&id, json!({"tools": self.tools.definitions()})),
            "tools/call" => {
                let Some(name) = params.get("name").and_then(Value::as_str) else {
                    return Some(error(&id, -32602, "tools/call needs a name"));
                };
                let known = self
                    .tools
                    .definitions()
                    .iter()
                    .any(|tool| tool.get("name").and_then(Value::as_str) == Some(name));
                if !known {
                    return Some(error(&id, -32602, &format!("unknown tool {name}")));
                }
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let output = self.tools.call(name, &arguments);
                result(&id, Self::tool_result(output))
            }
            "resources/list" => result(&id, json!({"resources": self.tools.resources()})),
            "resources/templates/list" => result(&id, json!({"resourceTemplates": []})),
            "resources/read" => {
                let Some(uri) = params.get("uri").and_then(Value::as_str) else {
                    return Some(error(&id, -32602, "resources/read needs a uri"));
                };
                match self.tools.read_resource(uri) {
                    Some((mime_type, text)) => result(
                        &id,
                        json!({"contents": [{"uri": uri, "mimeType": mime_type, "text": text}]}),
                    ),
                    None => error(&id, -32002, &format!("resource not found: {uri}")),
                }
            }
            "prompts/list" => result(&id, json!({"prompts": []})),
            other => error(&id, -32601, &format!("method not found: {other}")),
        })
    }

    /// Serves until the input closes.
    pub fn serve(&mut self, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let answer = match serde_json::from_str::<Value>(&line) {
                Ok(Value::Array(batch)) => {
                    let answers: Vec<Value> = batch.iter().filter_map(|m| self.handle(m)).collect();
                    (!answers.is_empty()).then_some(Value::Array(answers))
                }
                Ok(message) => self.handle(&message),
                Err(_) => Some(error(&Value::Null, -32700, "parse error")),
            };
            if let Some(answer) = answer {
                writeln!(output, "{answer}")?;
                output.flush()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    impl Tools for Echo {
        fn definitions(&self) -> Vec<Value> {
            vec![json!({"name": "echo", "description": "echo", "inputSchema": {"type": "object"}})]
        }
        fn resources(&self) -> Vec<Value> {
            vec![json!({"uri": "echo://guide", "name": "guide", "mimeType": "text/markdown"})]
        }
        fn read_resource(&self, uri: &str) -> Option<(String, String)> {
            (uri == "echo://guide").then(|| ("text/markdown".to_owned(), "# Guide".to_owned()))
        }
        fn call(&mut self, _name: &str, arguments: &Value) -> ToolOutput {
            if arguments.get("fail").is_some() {
                ToolOutput::Error("asked to fail".into())
            } else {
                ToolOutput::Json(arguments.clone())
            }
        }
    }

    fn run(lines: &[&str]) -> Vec<Value> {
        let mut server = Server::new(Echo, "t", "0.0.0", "be nice");
        let input = lines.join("\n");
        let mut output = Vec::new();
        server.serve(input.as_bytes(), &mut output).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn full_handshake_and_call() {
        let answers = run(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"a":1}}}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"echo","arguments":{"fail":true}}}"#,
            r#"{"jsonrpc":"2.0","id":5,"method":"ping"}"#,
        ]);
        assert_eq!(answers.len(), 5, "the notification gets no answer");
        assert_eq!(answers[0]["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(answers[0]["result"]["instructions"], "be nice");
        assert_eq!(answers[1]["result"]["tools"][0]["name"], "echo");
        assert_eq!(answers[2]["result"]["structuredContent"]["a"], 1);
        assert_eq!(answers[2]["result"]["isError"], false);
        assert_eq!(answers[3]["result"]["isError"], true);
        assert_eq!(answers[4]["id"], 5);
    }

    #[test]
    fn resources_are_listed_and_read() {
        let answers = run(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"resources/list"}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"resources/read","params":{"uri":"echo://guide"}}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"uri":"echo://nope"}}"#,
        ]);
        assert!(answers[0]["result"]["capabilities"]["resources"].is_object());
        assert_eq!(answers[1]["result"]["resources"][0]["uri"], "echo://guide");
        assert_eq!(answers[2]["result"]["contents"][0]["text"], "# Guide");
        assert_eq!(answers[3]["error"]["code"], -32002);
    }

    #[test]
    fn errors() {
        let answers = run(&[
            "{nope",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"bogus"}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"missing"}}"#,
            r#"[{"jsonrpc":"2.0","id":4,"method":"ping"},{"jsonrpc":"2.0","method":"x"}]"#,
        ]);
        assert_eq!(answers[0]["error"]["code"], -32700);
        assert_eq!(
            answers[1]["result"]["protocolVersion"],
            SUPPORTED_VERSIONS[0]
        );
        assert_eq!(answers[2]["error"]["code"], -32601);
        assert_eq!(answers[3]["error"]["code"], -32602);
        assert_eq!(answers[4].as_array().unwrap().len(), 1);
    }
}
