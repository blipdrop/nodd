//! `nodd mcp`。stdioのMCPサーバーで、通知と質問をエージェントから使えるようにする。
//! 回答はモデルが無視できるので、実行を止めたいときは `nodd hook` を使う。

use crate::{outcome, Outcome};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

const PROTOCOL: &str = "2025-06-18";
const DEFAULT_WAIT_SECONDS: u64 = 50;
const MAX_WAIT_SECONDS: u64 = 300;
const POLL_SECONDS: u64 = 5;

pub trait Backend {
    fn send(&self, body: &Value) -> Result<Value, String>;
    fn state(&self, id: &str) -> Result<Value, String>;
}

fn image_schema() -> Value {
    json!({
        "type": "object",
        "description": "A JPEG or PNG up to 40 KB, as Base64.",
        "required": ["data"],
        "properties": {"data": {"type": "string"}, "alt": {"type": "string", "maxLength": 120}},
    })
}

fn tools() -> Value {
    json!([
        {
            "name": "notify",
            "description": "Send a notification to a person's iPhone. Does not wait for a response.",
            "inputSchema": {
                "type": "object",
                "required": ["title"],
                "properties": {
                    "title": {"type": "string", "maxLength": 120},
                    "body": {"type": "string", "maxLength": 4000, "description": "Up to 1,000 characters, or 4,000 with format markdown."},
                    "format": {"type": "string", "enum": ["plain", "markdown"]},
                    "image": image_schema(),
                    "level": {"type": "string", "enum": ["low", "normal", "high"]},
                },
            },
        },
        {
            "name": "ask",
            "description": "Ask a person to approve or choose. Returns pending if there is no answer in time; use check_answer to look it up.",
            "inputSchema": {
                "type": "object",
                "required": ["title"],
                "properties": {
                    "title": {"type": "string", "maxLength": 120},
                    "body": {"type": "string", "maxLength": 4000, "description": "Up to 1,000 characters, or 4,000 with format markdown."},
                    "format": {"type": "string", "enum": ["plain", "markdown"]},
                    "image": image_schema(),
                    "options": {
                        "type": "array",
                        "description": "2 to 4 options to choose from. Omit to ask for approve or reject.",
                        "items": {
                            "type": "object",
                            "required": ["id", "label"],
                            "properties": {"id": {"type": "string"}, "label": {"type": "string"}},
                        },
                    },
                    "input": {
                        "type": "object",
                        "description": "Let the person add up to 200 characters of text, returned as note. Use it only when a short instruction helps.",
                        "required": ["prompt"],
                        "properties": {
                            "prompt": {"type": "string", "maxLength": 40},
                            "required": {"type": "boolean"},
                        },
                    },
                    "expires_in": {"type": "integer", "minimum": 60},
                    "wait_seconds": {"type": "integer", "minimum": 0, "maximum": MAX_WAIT_SECONDS},
                },
            },
        },
        {
            "name": "check_answer",
            "description": "Look up the answer for an id returned by ask.",
            "inputSchema": {
                "type": "object",
                "required": ["id"],
                "properties": {"id": {"type": "string"}},
            },
        },
    ])
}

fn answer(id: &str, state: &Value) -> Value {
    let mut result = match outcome(state) {
        None => json!({"id": id, "status": "pending"}),
        Some(Outcome::Approved) => json!({"id": id, "status": "approved"}),
        Some(Outcome::Rejected) => json!({"id": id, "status": "rejected"}),
        Some(Outcome::Chosen(option)) => json!({"id": id, "status": "answered", "option_id": option}),
        Some(Outcome::Closed(state)) => json!({"id": id, "status": state}),
    };
    if let Some(note) = state["response"]["note"].as_str() {
        result["note"] = note.into();
    }
    result
}

fn copy_fields(arguments: &Value, names: &[&str], body: &mut Value) {
    for name in names {
        if !arguments[name].is_null() {
            body[name] = arguments[name].clone();
        }
    }
}

fn ask(backend: &dyn Backend, arguments: &Value, pause: Duration) -> Result<Value, String> {
    let kind = if arguments["options"].is_array() { "choice" } else { "approval" };
    let mut body = json!({"kind": kind, "expires_in": crate::DEFAULT_ASK_SECONDS});
    copy_fields(arguments, &["title", "body", "format", "image", "options", "input", "expires_in"], &mut body);
    let accepted = backend.send(&body)?;
    let id = accepted["id"].as_str().ok_or("Unexpected response format")?.to_string();
    let wait = arguments["wait_seconds"].as_u64().unwrap_or(DEFAULT_WAIT_SECONDS).min(MAX_WAIT_SECONDS);
    let limit = Instant::now() + Duration::from_secs(wait);
    loop {
        let result = answer(&id, &backend.state(&id)?);
        if result["status"] != "pending" || Instant::now() >= limit {
            return Ok(result);
        }
        std::thread::sleep(pause);
    }
}

fn call(backend: &dyn Backend, name: &str, arguments: &Value, pause: Duration) -> Result<Value, String> {
    match name {
        "notify" => {
            let mut body = json!({"kind": "notice"});
            copy_fields(arguments, &["title", "body", "format", "image", "level"], &mut body);
            let accepted = backend.send(&body)?;
            Ok(json!({"id": accepted["id"], "status": "sent"}))
        }
        "ask" => ask(backend, arguments, pause),
        "check_answer" => {
            let id = crate::api::segment(arguments["id"].as_str().unwrap_or_default())?;
            Ok(answer(id, &backend.state(id)?))
        }
        other => Err(format!("{other} is not available")),
    }
}

/// 通知には返事をしない。
pub fn handle(backend: &dyn Backend, message: &Value, pause: Duration) -> Option<Value> {
    let id = message.get("id")?.clone();
    let result = match message["method"].as_str().unwrap_or_default() {
        "initialize" => json!({
            "protocolVersion": message["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "nodd", "version": env!("CARGO_PKG_VERSION")},
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let params = &message["params"];
            let name = params["name"].as_str().unwrap_or_default();
            match call(backend, name, &params["arguments"], pause) {
                Ok(value) => json!({"content": [{"type": "text", "text": value.to_string()}]}),
                Err(error) => json!({"content": [{"type": "text", "text": error}], "isError": true}),
            }
        }
        other => {
            return Some(json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": format!("unknown method {other}")},
            }));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

pub fn serve(backend: &dyn Backend, input: impl BufRead, mut output: impl Write) -> Result<(), String> {
    for line in input.lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle(backend, &message, Duration::from_secs(POLL_SECONDS)),
            Err(error) => Some(json!({
                "jsonrpc": "2.0", "id": null,
                "error": {"code": -32700, "message": error.to_string()},
            })),
        };
        if let Some(reply) = reply {
            writeln!(output, "{reply}").map_err(|e| e.to_string())?;
            output.flush().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake {
        sent: RefCell<Vec<Value>>,
        states: RefCell<Vec<Value>>,
    }

    impl Backend for Fake {
        fn send(&self, body: &Value) -> Result<Value, String> {
            self.sent.borrow_mut().push(body.clone());
            Ok(json!({"id": "msg_1"}))
        }
        fn state(&self, _: &str) -> Result<Value, String> {
            let mut states = self.states.borrow_mut();
            Ok(if states.len() > 1 { states.remove(0) } else { states[0].clone() })
        }
    }

    fn fake(states: Vec<Value>) -> Fake {
        Fake { sent: RefCell::default(), states: RefCell::new(states) }
    }

    fn tool(backend: &Fake, name: &str, arguments: Value) -> Value {
        let message = json!({"id": 1, "method": "tools/call", "params": {"name": name, "arguments": arguments}});
        let reply = handle(backend, &message, Duration::ZERO).unwrap();
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap_or(Value::Null)
    }

    #[test]
    fn rejection_note_is_returned() {
        let state = json!({"kind": "approval", "response": {"option_id": "reject", "note": "Use staging"}});
        assert_eq!(answer("msg_1", &state), json!({"id": "msg_1", "status": "rejected", "note": "Use staging"}));
    }

    #[test]
    fn notifications_get_no_reply() {
        let message = json!({"method": "notifications/initialized"});
        assert!(handle(&fake(vec![]), &message, Duration::ZERO).is_none());
    }

    #[test]
    fn initialize_and_list() {
        let backend = fake(vec![]);
        let reply = handle(&backend, &json!({"id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05"}}), Duration::ZERO).unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2024-11-05");
        let list = handle(&backend, &json!({"id": 2, "method": "tools/list"}), Duration::ZERO).unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 3);
        assert!(handle(&backend, &json!({"id": 3, "method": "nope"}), Duration::ZERO).unwrap()["error"].is_object());
    }

    #[test]
    fn ask_waits_for_the_answer() {
        let open = json!({"kind": "approval", "action_state": "open"});
        let done = json!({"kind": "approval", "response": {"option_id": "approve"}});
        let backend = fake(vec![open, done]);
        let result = tool(&backend, "ask", json!({"title": "t", "wait_seconds": 30}));
        assert_eq!(result["status"], "approved");
        assert_eq!(backend.sent.borrow()[0]["kind"], "approval");
    }

    #[test]
    fn unanswered_ask_stays_pending() {
        let backend = fake(vec![json!({"kind": "approval", "action_state": "open"})]);
        assert_eq!(tool(&backend, "ask", json!({"title": "t", "wait_seconds": 0}))["status"], "pending");
    }

    #[test]
    fn input_is_passed_to_the_message() {
        let backend = fake(vec![json!({"kind": "approval", "action_state": "open"})]);
        tool(&backend, "ask", json!({"title": "t", "wait_seconds": 0, "input": {"prompt": "How?", "required": true}}));
        assert_eq!(backend.sent.borrow()[0]["input"]["prompt"], "How?");
    }

    #[test]
    fn notify_keeps_format_and_image() {
        let backend = fake(vec![]);
        let image = json!({"data": "aGk=", "alt": "graph"});
        tool(&backend, "notify", json!({"title": "t", "format": "markdown", "image": image}));
        assert_eq!(backend.sent.borrow()[0]["format"], "markdown");
        assert_eq!(backend.sent.borrow()[0]["image"]["alt"], "graph");
    }

    #[test]
    fn options_make_a_choice() {
        let backend = fake(vec![json!({"kind": "choice", "response": {"option_id": "b"}})]);
        let options = json!([{"id": "a", "label": "A"}, {"id": "b", "label": "B"}]);
        let result = tool(&backend, "ask", json!({"title": "t", "options": options}));
        assert_eq!(result, json!({"id": "msg_1", "status": "answered", "option_id": "b"}));
        assert_eq!(backend.sent.borrow()[0]["kind"], "choice");
    }

    #[test]
    fn bad_id_is_a_tool_error() {
        let message = json!({"id": 1, "method": "tools/call", "params": {"name": "check_answer", "arguments": {"id": "../x"}}});
        assert_eq!(handle(&fake(vec![]), &message, Duration::ZERO).unwrap()["result"]["isError"], true);
    }

    #[test]
    fn serve_answers_each_line() {
        let input = b"{\"id\":1,\"method\":\"ping\"}\nnot json\n{\"method\":\"notifications/initialized\"}\n";
        let mut output = vec![];
        serve(&fake(vec![]), &input[..], &mut output).unwrap();
        let lines: Vec<Value> = String::from_utf8(output).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["error"]["code"], -32700);
    }
}
