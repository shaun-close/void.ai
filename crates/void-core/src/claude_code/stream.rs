//! stream-json stdout parsing and the stdin / control-protocol lines void writes.

use crate::model::{Part, Usage};
use crate::sandbox::cap_output;
use serde_json::{Value, json};

/// One parsed stdout line.
#[derive(Clone, Debug, PartialEq)]
pub enum CcEvent {
    Init {
        session: String,
        model: String,
    },
    /// `parent` is the subagent tool_use id (None = main thread).
    TextDelta {
        text: String,
        parent: Option<String>,
    },
    ThinkingDelta {
        text: String,
        parent: Option<String>,
    },
    /// A complete assistant message; `parent` is the subagent tool_use id (None = main thread).
    /// `error` is the raw API error code (`authentication_failed`, `rate_limit`, ...).
    /// `usage` is this single API call's usage (prompt = input + cache read + cache creation):
    /// use it for the context meter. `Result.usage` is cumulative across the whole turn.
    Assistant {
        parts: Vec<Part>,
        parent: Option<String>,
        error: Option<String>,
        usage: Option<Usage>,
    },
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
        parent: Option<String>,
    },
    PermissionRequest {
        request_id: String,
        tool: String,
        input: Value,
        suggestions: Option<Value>,
        /// Claude Code's own wording for the prompt, and the path that made it ask.
        title: Option<String>,
        description: Option<String>,
        blocked_path: Option<String>,
    },
    Result {
        usage: Usage,
        is_error: bool,
        message: Option<String>,
        cost_usd: Option<f64>,
        subtype: String,
    },
    ControlResponse {
        request_id: String,
        ok: bool,
        error: Option<String>,
    },
    ControlCancel {
        request_id: String,
    },
    /// hook_callback / mcp_message / unknown: answer with the error envelope.
    UnsupportedControl {
        request_id: String,
        subtype: String,
    },
    SessionState(String),
    ApiRetry {
        attempt: u32,
        error: String,
    },
    CompactBoundary,
    Other,
}

/// Parse one stdout line into every event it carries (the only public parser). A `user` frame with parallel tool
/// results yields one `ToolResult` per block. Empty for unrecognised lines.
pub fn parse_events(line: &str) -> Vec<CcEvent> {
    let line = line.trim();
    if !line.starts_with('{') {
        return Vec::new();
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    events_from(&v)
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

fn parent_of(v: &Value) -> Option<String> {
    s(v, "parent_tool_use_id")
}

fn events_from(v: &Value) -> Vec<CcEvent> {
    let one = |e: Option<CcEvent>| e.into_iter().collect::<Vec<_>>();
    match v.get("type").and_then(Value::as_str) {
        Some("system") => one(system_event(v)),
        Some("stream_event") => one(delta_event(v)),
        Some("assistant") => one(assistant_event(v)),
        Some("user") => tool_results(v),
        Some("result") => one(Some(result_event(v))),
        Some("control_request") => one(control_request(v)),
        Some("control_response") => one(control_response(v)),
        Some("control_cancel_request") => {
            one(s(v, "request_id").map(|request_id| CcEvent::ControlCancel { request_id }))
        }
        _ => Vec::new(),
    }
}

fn system_event(v: &Value) -> Option<CcEvent> {
    match v.get("subtype").and_then(Value::as_str)? {
        "init" => Some(CcEvent::Init {
            session: s(v, "session_id")?,
            model: s(v, "model").unwrap_or_default(),
        }),
        "session_state_changed" => s(v, "state").map(CcEvent::SessionState),
        "api_retry" => Some(CcEvent::ApiRetry {
            attempt: v.get("attempt").and_then(Value::as_u64).unwrap_or(0) as u32,
            error: s(v, "error").unwrap_or_default(),
        }),
        "compact_boundary" => Some(CcEvent::CompactBoundary),
        _ => None,
    }
}

fn delta_event(v: &Value) -> Option<CcEvent> {
    let ev = v.get("event")?;
    if ev.get("type")?.as_str()? != "content_block_delta" {
        return None;
    }
    let d = ev.get("delta")?;
    let parent = parent_of(v);
    match d.get("type")?.as_str()? {
        "text_delta" => s(d, "text").map(|text| CcEvent::TextDelta { text, parent }),
        "thinking_delta" => s(d, "thinking").map(|text| CcEvent::ThinkingDelta { text, parent }),
        _ => None,
    }
}

fn assistant_event(v: &Value) -> Option<CcEvent> {
    let blocks = v.get("message")?.get("content")?.as_array()?;
    let mut parts = Vec::new();
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = s(b, "text") {
                    parts.push(Part::Text(t));
                }
            }
            Some("thinking") => {
                if let Some(t) = s(b, "thinking") {
                    parts.push(Part::Reasoning(t));
                }
            }
            Some("tool_use") => {
                if let (Some(id), Some(name)) = (s(b, "id"), s(b, "name")) {
                    let input = b.get("input").cloned().unwrap_or(Value::Null);
                    parts.push(Part::ToolCall {
                        id,
                        name,
                        arguments: input.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    Some(CcEvent::Assistant {
        parts,
        parent: parent_of(v),
        error: s(v, "error").or_else(|| s(v.get("message")?, "error")),
        usage: v.get("message")?.get("usage").map(usage_of),
    })
}

fn tool_results(v: &Value) -> Vec<CcEvent> {
    let Some(blocks) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let parent = parent_of(v);
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|b| {
            let call_id = s(b, "tool_use_id")?;
            let text = match b.get("content") {
                Some(Value::String(t)) => t.clone(),
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|i| match i.get("type").and_then(Value::as_str) {
                        Some("text") => s(i, "text"),
                        Some("image") => Some("[image]".to_string()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            let (mut content, cut) = cap_output(text.as_bytes());
            if cut > 0 {
                content.insert_str(0, &format!("[… {cut} bytes omitted]\n"));
            }
            Some(CcEvent::ToolResult {
                call_id,
                content,
                is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                parent: parent.clone(),
            })
        })
        .collect()
}

/// prompt = input + cache read + cache creation; missing fields count as 0.
fn usage_of(u: &Value) -> Usage {
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        prompt_tokens: (n("input_tokens")
            + n("cache_read_input_tokens")
            + n("cache_creation_input_tokens"))
        .min(u32::MAX as u64) as u32,
        completion_tokens: n("output_tokens").min(u32::MAX as u64) as u32,
    }
}

fn result_event(v: &Value) -> CcEvent {
    let usage = v.get("usage").map(usage_of).unwrap_or_default();
    let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    let errors = v.get("errors").and_then(Value::as_array).map(|a| {
        a.iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    });
    let message = if is_error {
        errors
            .filter(|e| !e.is_empty())
            .or_else(|| s(v, "result").filter(|r| !r.is_empty()))
            .or_else(|| s(v, "subtype").filter(|r| !r.is_empty()))
            .or_else(|| Some("error".to_string()))
    } else {
        None
    };
    CcEvent::Result {
        usage,
        is_error,
        message,
        cost_usd: v.get("total_cost_usd").and_then(Value::as_f64),
        subtype: s(v, "subtype").unwrap_or_default(),
    }
}

fn control_request(v: &Value) -> Option<CcEvent> {
    let request_id = s(v, "request_id")?;
    let req = v.get("request");
    let subtype = req.and_then(|r| s(r, "subtype")).unwrap_or_default();
    // Anything not fully parseable still gets an event so the caller can answer it;
    // otherwise the CLI blocks waiting for a reply.
    if let (Some(req), "can_use_tool") = (req, subtype.as_str())
        && let Some(tool) = s(req, "tool_name")
    {
        return Some(CcEvent::PermissionRequest {
            request_id,
            tool,
            input: req.get("input").cloned().unwrap_or(Value::Null),
            suggestions: req
                .get("permission_suggestions")
                .filter(|x| !x.is_null())
                .cloned(),
            title: s(req, "title"),
            description: s(req, "description"),
            blocked_path: s(req, "blocked_path"),
        });
    }
    Some(CcEvent::UnsupportedControl {
        request_id,
        subtype,
    })
}

fn control_response(v: &Value) -> Option<CcEvent> {
    let r = v.get("response")?;
    Some(CcEvent::ControlResponse {
        request_id: s(r, "request_id")?,
        ok: r.get("subtype").and_then(Value::as_str) == Some("success"),
        error: s(r, "error"),
    })
}

/// Serialise to one stdin line. U+2028/U+2029 are escaped so no line splitter can break it.
fn line(v: Value) -> String {
    v.to_string()
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// One stdin line carrying a user message.
pub fn user_message(text: &str) -> String {
    line(json!({
        "type": "user",
        "session_id": "",
        "message": {"role": "user", "content": text},
        "parent_tool_use_id": null,
    }))
}

fn control_request_line(request_id: &str, subtype: &str) -> String {
    let mut request = json!({"subtype": subtype});
    if subtype == "initialize" {
        request["hooks"] = Value::Null;
    }
    line(json!({"type": "control_request", "request_id": request_id, "request": request}))
}

pub fn initialize_request(request_id: &str) -> String {
    control_request_line(request_id, "initialize")
}

pub fn interrupt_request(request_id: &str) -> String {
    control_request_line(request_id, "interrupt")
}

fn success(request_id: &str, response: Value) -> String {
    line(json!({"type": "control_response", "response": {
        "subtype": "success", "request_id": request_id, "response": response}}))
}

/// Allow a tool call. `input` is echoed as `updatedInput`; `updated_permissions` is the
/// chosen `permission_suggestions` entries for "always allow".
pub fn control_response_allow(
    request_id: &str,
    input: &Value,
    updated_permissions: Option<&Value>,
) -> String {
    let mut r = json!({"behavior": "allow", "updatedInput": input});
    if let Some(p) = updated_permissions {
        r["updatedPermissions"] = p.clone();
    }
    success(request_id, r)
}

pub fn control_response_deny(request_id: &str, message: &str, interrupt: bool) -> String {
    let mut r = json!({"behavior": "deny", "message": message});
    if interrupt {
        r["interrupt"] = Value::Bool(true);
    }
    success(request_id, r)
}

pub fn control_response_error(request_id: &str, msg: &str) -> String {
    line(json!({"type": "control_response", "response": {
        "subtype": "error", "request_id": request_id, "error": msg}}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// First event of a line, or `Other` when it carries none.
    fn parse_line(l: &str) -> CcEvent {
        parse_events(l).into_iter().next().unwrap_or(CcEvent::Other)
    }

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn parse_init_text_thinking_tool_use_result() {
        let init = r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-opus","tools":[],"cwd":"/x"}"#;
        assert_eq!(
            parse_line(init),
            CcEvent::Init {
                session: "s1".into(),
                model: "claude-opus".into()
            }
        );
        let td = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}},"parent_tool_use_id":null}"#;
        assert_eq!(
            parse_line(td),
            CcEvent::TextDelta {
                text: "hi".into(),
                parent: None
            }
        );
        let th = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}}"#;
        assert_eq!(
            parse_line(th),
            CcEvent::ThinkingDelta {
                text: "hmm".into(),
                parent: None
            }
        );
        let ij = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\""}}}"#;
        assert_eq!(parse_line(ij), CcEvent::Other);
        let ms = r#"{"type":"stream_event","event":{"type":"message_start"}}"#;
        assert_eq!(parse_line(ms), CcEvent::Other);

        let asst = r#"{"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[{"type":"thinking","thinking":"t"},{"type":"text","text":"ok"},{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"ls"}}]}}"#;
        match parse_line(asst) {
            CcEvent::Assistant { parts, parent, .. } => {
                assert_eq!(parent, None);
                assert_eq!(parts.len(), 3);
                assert_eq!(parts[0], Part::Reasoning("t".into()));
                assert_eq!(parts[1], Part::Text("ok".into()));
                match &parts[2] {
                    Part::ToolCall {
                        id,
                        name,
                        arguments,
                    } => {
                        assert_eq!((id.as_str(), name.as_str()), ("tu1", "Bash"));
                        assert_eq!(v(arguments), json!({"command":"ls"}));
                    }
                    p => panic!("{p:?}"),
                }
            }
            e => panic!("{e:?}"),
        }

        let tr = r#"{"type":"user","parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu1","content":"out","is_error":false}]}}"#;
        assert_eq!(
            parse_line(tr),
            CcEvent::ToolResult {
                call_id: "tu1".into(),
                content: "out".into(),
                is_error: false,
                parent: None
            }
        );
        let trb = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu2","is_error":true,"content":[{"type":"text","text":"a"},{"type":"image","source":{}},{"type":"text","text":"b"}]}]}}"#;
        assert_eq!(
            parse_line(trb),
            CcEvent::ToolResult {
                call_id: "tu2".into(),
                content: "a\n[image]\nb".into(),
                is_error: true,
                parent: None
            }
        );

        let res = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","total_cost_usd":0.5,"usage":{"input_tokens":10,"output_tokens":7,"cache_read_input_tokens":100,"cache_creation_input_tokens":5}}"#;
        assert_eq!(
            parse_line(res),
            CcEvent::Result {
                usage: Usage {
                    prompt_tokens: 115,
                    completion_tokens: 7
                },
                is_error: false,
                message: None,
                cost_usd: Some(0.5),
                subtype: "success".into(),
            }
        );
        let err = r#"{"type":"result","subtype":"error_max_turns","is_error":true,"errors":["boom","bang"],"usage":{"output_tokens":2}}"#;
        match parse_line(err) {
            CcEvent::Result {
                usage,
                is_error,
                message,
                cost_usd,
                subtype,
            } => {
                assert_eq!(
                    usage,
                    Usage {
                        prompt_tokens: 0,
                        completion_tokens: 2
                    }
                );
                assert!(is_error);
                assert_eq!(message.as_deref(), Some("boom\nbang"));
                assert_eq!(cost_usd, None);
                assert_eq!(subtype, "error_max_turns");
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn parse_subagent_parent_id() {
        let asst = r#"{"type":"assistant","parent_tool_use_id":"task1","message":{"content":[{"type":"text","text":"sub"}]}}"#;
        match parse_line(asst) {
            CcEvent::Assistant { parent, .. } => assert_eq!(parent.as_deref(), Some("task1")),
            e => panic!("{e:?}"),
        }
        let tr = r#"{"type":"user","parent_tool_use_id":"task1","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"x"}]}}"#;
        match parse_line(tr) {
            CcEvent::ToolResult {
                parent, is_error, ..
            } => {
                assert_eq!(parent.as_deref(), Some("task1"));
                assert!(!is_error);
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn parse_permission_request() {
        let l = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm x"},"permission_suggestions":[{"type":"addRules"}],"tool_use_id":"tu"}}"#;
        assert_eq!(
            parse_line(l),
            CcEvent::PermissionRequest {
                request_id: "r1".into(),
                tool: "Bash".into(),
                input: json!({"command":"rm x"}),
                suggestions: Some(json!([{"type":"addRules"}])),
                title: None,
                description: None,
                blocked_path: None,
            }
        );
        let t = r#"{"type":"control_request","request_id":"r3","request":{"subtype":"can_use_tool","tool_name":"Read","input":{},"title":"Read it?","description":"outside","blocked_path":"/etc/x","display_name":"Read"}}"#;
        match parse_line(t) {
            CcEvent::PermissionRequest {
                title,
                description,
                blocked_path,
                ..
            } => {
                assert_eq!(title.as_deref(), Some("Read it?"));
                assert_eq!(description.as_deref(), Some("outside"));
                assert_eq!(blocked_path.as_deref(), Some("/etc/x"));
            }
            e => panic!("{e:?}"),
        }
        let n = r#"{"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"Read","input":{},"permission_suggestions":null}}"#;
        match parse_line(n) {
            CcEvent::PermissionRequest { suggestions, .. } => assert_eq!(suggestions, None),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn parse_control_misc() {
        assert_eq!(
            parse_line(
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"a","response":{"account":{"x":1}}}}"#
            ),
            CcEvent::ControlResponse {
                request_id: "a".into(),
                ok: true,
                error: None
            }
        );
        assert_eq!(
            parse_line(
                r#"{"type":"control_response","response":{"subtype":"error","request_id":"a","error":"no"}}"#
            ),
            CcEvent::ControlResponse {
                request_id: "a".into(),
                ok: false,
                error: Some("no".into())
            }
        );
        assert_eq!(
            parse_line(r#"{"type":"control_cancel_request","request_id":"c"}"#),
            CcEvent::ControlCancel {
                request_id: "c".into()
            }
        );
        assert_eq!(
            parse_line(
                r#"{"type":"control_request","request_id":"h","request":{"subtype":"hook_callback"}}"#
            ),
            CcEvent::UnsupportedControl {
                request_id: "h".into(),
                subtype: "hook_callback".into()
            }
        );
        assert_eq!(
            parse_line(r#"{"type":"system","subtype":"session_state_changed","state":"idle"}"#),
            CcEvent::SessionState("idle".into())
        );
        assert_eq!(
            parse_line(
                r#"{"type":"system","subtype":"api_retry","attempt":2,"max_retries":5,"error":"overloaded"}"#
            ),
            CcEvent::ApiRetry {
                attempt: 2,
                error: "overloaded".into()
            }
        );
        assert_eq!(
            parse_line(r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{}}"#),
            CcEvent::CompactBoundary
        );
        assert_eq!(
            parse_line(r#"{"type":"system","subtype":"hook_started"}"#),
            CcEvent::Other
        );
    }

    #[test]
    fn parse_unknown_line_is_other() {
        for l in [
            "",
            "   ",
            "not json",
            "{",
            "[]",
            "123",
            r#"{"type":"mystery"}"#,
            r#"{"type":42}"#,
            r#"{"type":"assistant"}"#,
            r#"{"type":"user","message":"str"}"#,
        ] {
            assert_eq!(parse_line(l), CcEvent::Other, "line {l:?}");
        }
    }

    #[test]
    fn huge_tool_result_is_capped() {
        let big = "x".repeat(5 * 1024 * 1024);
        let line = format!(
            r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"t","content":"{big}"}}]}}}}"#
        );
        // The fastest of a few parses (a loaded machine slows some), against a bound for the
        // build: an unoptimised debug build is several times slower.
        let bound =
            std::time::Duration::from_millis(if cfg!(debug_assertions) { 1000 } else { 200 });
        let timed = || {
            let t = std::time::Instant::now();
            let ev = parse_line(&line);
            (t.elapsed(), ev)
        };
        let (mut best, ev) = timed();
        for _ in 0..2 {
            best = best.min(timed().0);
        }
        println!("TIMING parse of a 5 MB tool result: {best:?}");
        assert!(best < bound, "{best:?}");
        match ev {
            CcEvent::ToolResult { content, .. } => {
                assert!(content.len() <= crate::sandbox::OUTPUT_CAP + 64);
                assert!(!content.is_empty());
            }
            e => panic!("{:?}", std::mem::discriminant(&e)),
        }
    }

    #[test]
    fn outgoing_lines() {
        let u = v(&user_message("hi \"there\"\n"));
        assert_eq!(u["type"], "user");
        assert_eq!(u["message"]["role"], "user");
        assert_eq!(u["message"]["content"], "hi \"there\"\n");
        assert!(u["parent_tool_use_id"].is_null());
        assert!(!user_message("a\nb").contains('\n'));

        let i = v(&initialize_request("req_1"));
        assert_eq!(i["type"], "control_request");
        assert_eq!(i["request_id"], "req_1");
        assert_eq!(i["request"]["subtype"], "initialize");
        let i = v(&interrupt_request("req_2"));
        assert_eq!(i["request"]["subtype"], "interrupt");

        let input = json!({"command":"ls"});
        let sug = json!([{"type":"addRules"}]);
        let a = v(&control_response_allow("r1", &input, Some(&sug)));
        assert_eq!(a["type"], "control_response");
        assert_eq!(a["response"]["subtype"], "success");
        assert_eq!(a["response"]["request_id"], "r1");
        assert_eq!(a["response"]["response"]["behavior"], "allow");
        assert_eq!(a["response"]["response"]["updatedInput"], input);
        assert_eq!(a["response"]["response"]["updatedPermissions"], sug);
        let a = v(&control_response_allow("r1", &input, None));
        assert!(
            a["response"]["response"]
                .get("updatedPermissions")
                .is_none()
        );

        let d = v(&control_response_deny("r1", "no", true));
        assert_eq!(d["response"]["response"]["behavior"], "deny");
        assert_eq!(d["response"]["response"]["message"], "no");
        assert_eq!(d["response"]["response"]["interrupt"], true);
        let d = v(&control_response_deny("r1", "no", false));
        assert!(d["response"]["response"].get("interrupt").is_none());

        let e = v(&control_response_error(
            "r9",
            "Unsupported control request subtype: x",
        ));
        assert_eq!(e["response"]["subtype"], "error");
        assert_eq!(e["response"]["request_id"], "r9");
        assert_eq!(
            e["response"]["error"],
            "Unsupported control request subtype: x"
        );
    }

    #[test]
    fn assistant_error_and_per_call_usage() {
        let l = r#"{"type":"assistant","error":"authentication_failed","message":{"content":[{"type":"text","text":"Not signed in"}],"usage":{"input_tokens":4,"output_tokens":9,"cache_read_input_tokens":10,"cache_creation_input_tokens":1}}}"#;
        match parse_line(l) {
            CcEvent::Assistant { error, usage, .. } => {
                assert_eq!(error.as_deref(), Some("authentication_failed"));
                assert_eq!(
                    usage,
                    Some(Usage {
                        prompt_tokens: 15,
                        completion_tokens: 9
                    })
                );
            }
            e => panic!("{e:?}"),
        }
        let l = r#"{"type":"assistant","message":{"content":[]}}"#;
        match parse_line(l) {
            CcEvent::Assistant { error, usage, .. } => {
                assert_eq!((error, usage), (None, None));
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn error_result_without_text_falls_back_to_subtype() {
        let l = r#"{"type":"result","subtype":"error_during_execution","is_error":true}"#;
        match parse_line(l) {
            CcEvent::Result { message, .. } => {
                assert_eq!(message.as_deref(), Some("error_during_execution"))
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn malformed_control_requests_become_unsupported() {
        let cases = [
            (
                r#"{"type":"control_request","request_id":"m1","request":{"subtype":"can_use_tool","input":{}}}"#,
                "can_use_tool",
            ),
            (r#"{"type":"control_request","request_id":"m2"}"#, ""),
            (
                r#"{"type":"control_request","request_id":"m3","request":"x"}"#,
                "",
            ),
        ];
        for (l, sub) in cases {
            match parse_line(l) {
                CcEvent::UnsupportedControl {
                    request_id,
                    subtype,
                } => {
                    assert!(request_id.starts_with('m'));
                    assert_eq!(subtype, sub, "{l}");
                }
                e => panic!("{l}: {e:?}"),
            }
        }
        // No request_id at all: nothing we can answer.
        assert_eq!(
            parse_line(r#"{"type":"control_request","request":{}}"#),
            CcEvent::Other
        );
    }

    #[test]
    fn deltas_carry_subagent_parent() {
        let td = r#"{"type":"stream_event","parent_tool_use_id":"task7","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"a"}}}"#;
        assert_eq!(
            parse_line(td),
            CcEvent::TextDelta {
                text: "a".into(),
                parent: Some("task7".into())
            }
        );
        let th = r#"{"type":"stream_event","parent_tool_use_id":"task7","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"b"}}}"#;
        assert_eq!(
            parse_line(th),
            CcEvent::ThinkingDelta {
                text: "b".into(),
                parent: Some("task7".into())
            }
        );
    }

    #[test]
    fn user_frame_with_several_tool_results_yields_all() {
        let l = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"a","content":"1"},{"type":"tool_result","tool_use_id":"b","content":"2","is_error":true}]}}"#;
        let evs = parse_events(l);
        assert_eq!(evs.len(), 2);
        assert!(matches!(&evs[0], CcEvent::ToolResult { call_id, .. } if call_id == "a"));
        assert!(
            matches!(&evs[1], CcEvent::ToolResult { call_id, is_error: true, .. } if call_id == "b")
        );
    }

    #[test]
    fn capped_result_keeps_tail_and_has_marker() {
        let body = format!("{}{}", "H".repeat(40_000), "TAIL");
        let l = json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":body}]}}).to_string();
        match parse_line(&l) {
            CcEvent::ToolResult { content, .. } => {
                assert!(
                    content.starts_with("[… 10004 bytes omitted]\n"),
                    "{}",
                    &content[..40]
                );
                assert!(content.ends_with("TAIL"));
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn outgoing_lines_escape_line_separators() {
        let u = user_message("a\u{2028}b\u{2029}c");
        assert!(!u.contains('\u{2028}') && !u.contains('\u{2029}'));
        assert!(u.contains("\\u2028") && u.contains("\\u2029"));
        assert_eq!(v(&u)["message"]["content"], "a\u{2028}b\u{2029}c");
        let a = control_response_allow("r", &json!({"c":"x\u{2028}"}), None);
        assert!(!a.contains('\u{2028}'));
        let d = control_response_deny("r", "n\u{2029}", false);
        assert!(!d.contains('\u{2029}'));
        let e = control_response_error("r", "n\u{2028}");
        assert!(!e.contains('\u{2028}'));
        assert!(!initialize_request("r\u{2028}").contains('\u{2028}'));
        assert!(!interrupt_request("r\u{2029}").contains('\u{2029}'));
    }
}
