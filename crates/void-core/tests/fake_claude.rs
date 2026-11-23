//! Drives the fake `claude` fixture through the real parser. No tokens, no network.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use void_core::claude_code::{
    CcEvent, control_response_allow, initialize_request, parse_events, user_message,
};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

fn spawn(scenario: &str, dir: &std::path::Path) -> std::process::Child {
    spawn_with(scenario, dir, &["-p", "--verbose"])
}

fn spawn_with(scenario: &str, dir: &std::path::Path, args: &[&str]) -> std::process::Child {
    Command::new(root().join("fake-claude"))
        .args(args)
        .env(
            "FAKE_CLAUDE_SCENARIO",
            root().join("fake-claude-scenarios").join(scenario),
        )
        .env("FAKE_CLAUDE_ARGS_FILE", dir.join("args"))
        .env("FAKE_CLAUDE_STDIN_FILE", dir.join("stdin"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn fake-claude")
}

#[test]
fn text_turn_replays_and_records_args() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = spawn("text-turn.ndjson", dir.path());
    let mut stdin = c.stdin.take().unwrap();
    writeln!(stdin, "{}", initialize_request("req_1")).unwrap();
    writeln!(stdin, "{}", user_message("hi")).unwrap();
    let lines: Vec<String> = BufReader::new(c.stdout.take().unwrap())
        .lines()
        .map(|l| l.unwrap())
        .collect();
    let evs: Vec<CcEvent> = lines.iter().flat_map(|l| parse_events(l)).collect();
    assert!(
        matches!(&evs[0], CcEvent::ControlResponse { request_id, ok: true, .. } if request_id == "req_1")
    );
    assert!(matches!(&evs[1], CcEvent::Init { session, .. } if session == "sess-1"));
    assert_eq!(
        evs[2],
        CcEvent::TextDelta {
            text: "Hello".into(),
            parent: None
        }
    );
    assert!(matches!(
        evs.last().unwrap(),
        CcEvent::Result {
            is_error: false,
            ..
        }
    ));
    assert!(c.wait().unwrap().success());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("args")).unwrap(),
        "-p\n--verbose\n"
    );
}

#[test]
fn permission_scenario_waits_for_answer() {
    let dir = tempfile::tempdir().unwrap();
    let args = ["-p", "--permission-prompt-tool", "stdio"];
    let mut c = spawn_with("permission.ndjson", dir.path(), &args);
    let mut stdin = c.stdin.take().unwrap();
    let mut out = BufReader::new(c.stdout.take().unwrap()).lines();
    writeln!(stdin, "{}", user_message("go")).unwrap();
    let mut req = None;
    for l in out.by_ref() {
        if let CcEvent::PermissionRequest {
            request_id, input, ..
        } = parse_events(&l.unwrap()).remove(0)
        {
            req = Some((request_id, input));
            break;
        }
    }
    let (id, input) = req.expect("permission request");
    writeln!(stdin, "{}", control_response_allow(&id, &input, None)).unwrap();
    let rest: Vec<CcEvent> = out.flat_map(|l| parse_events(&l.unwrap())).collect();
    assert!(matches!(rest[0], CcEvent::ToolResult { .. }));
    assert!(matches!(rest.last().unwrap(), CcEvent::Result { .. }));
    assert!(c.wait().unwrap().success());
}

/// Like the real CLI: without `--permission-prompt-tool stdio` nobody is asked; the request
/// becomes a `system/permission_denied` frame and the turn goes on.
#[test]
fn permission_denied_without_stdio_prompt_tool() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = spawn("permission.ndjson", dir.path());
    let mut stdin = c.stdin.take().unwrap();
    writeln!(stdin, "{}", user_message("go")).unwrap();
    let lines: Vec<String> = BufReader::new(c.stdout.take().unwrap())
        .lines()
        .map(|l| l.unwrap())
        .collect();
    assert!(
        !lines.iter().any(|l| l.contains("can_use_tool")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains(r#""subtype": "permission_denied""#)),
        "{lines:?}"
    );
    assert!(c.wait().unwrap().success());
}

#[test]
fn tool_use_scenario_parses() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = spawn("tool-use.ndjson", dir.path());
    let mut stdin = c.stdin.take().unwrap();
    writeln!(stdin, "{}", user_message("ls")).unwrap();
    let evs: Vec<CcEvent> = BufReader::new(c.stdout.take().unwrap())
        .lines()
        .flat_map(|l| parse_events(&l.unwrap()))
        .collect();
    assert!(
        evs.iter()
            .any(|e| matches!(e, CcEvent::ToolResult { call_id, .. } if call_id == "tu_1"))
    );
    c.wait().unwrap();
}

#[test]
fn parallel_tool_results_in_one_frame_are_all_parsed() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = spawn("parallel-tools.ndjson", dir.path());
    let mut stdin = c.stdin.take().unwrap();
    writeln!(stdin, "{}", user_message("go")).unwrap();
    let evs: Vec<CcEvent> = BufReader::new(c.stdout.take().unwrap())
        .lines()
        .flat_map(|l| parse_events(&l.unwrap()))
        .collect();
    let ids: Vec<&str> = evs
        .iter()
        .filter_map(|e| match e {
            CcEvent::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, ["p1", "p2"]);
    c.wait().unwrap();
}
