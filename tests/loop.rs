//! The loop against recorded fixtures: no key, no network.

use std::sync::Arc;

use neon::agent::{Agent, Controls, VecSink};
use neon::config::Config;
use neon::llm::FixtureAdapter;
use neon::messages::{Message, Part, Role, estimate_tokens};
use neon::plugin::PluginHost;
use neon::session::Session;
use neon::tools::{BashTool, PluginRow, Registry, ToolCtx};

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

fn workdir(name: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from("/tmp").join(format!("neon-it-{name}"));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn harness(
    name: &str,
    fixture_name: &str,
    max_steps: usize,
    budget: usize,
) -> (
    Agent,
    Arc<VecSink>,
    Arc<Controls>,
    std::path::PathBuf,
    Arc<Session>,
) {
    let dir = workdir(name);
    let cfg = Config {
        spill_dir: dir.join("spill"),
        max_steps,
        context_budget_tokens: budget,
        sessions_dir: dir.join("sessions"),
        ..Config::default()
    };
    let registry = Registry::new();
    registry.add(Box::new(BashTool), PluginRow::Core);
    let sink = Arc::new(VecSink::default());
    let controls = Arc::new(Controls::default());
    let plugins = Arc::new(std::sync::Mutex::new(PluginHost::new(
        Arc::new(ToolCtx {
            cwd: dir.clone(),
            timeout_secs: cfg.bash_timeout_secs,
            excerpt_bytes: cfg.bash_excerpt_bytes,
            spill_dir: cfg.spill_dir.clone(),
            abort: controls.abort_flag(),
        }),
        Arc::new(|_line: &str| {}),
    )));
    let session = Arc::new(Session::create(&cfg.sessions_dir, &cfg.model(), &dir).unwrap());
    let agent = Agent {
        adapter: Arc::new(FixtureAdapter::from_file(&fixture(fixture_name)).unwrap()),
        registry: Arc::new(registry),
        plugins,
        cfg: Arc::new(cfg),
        sink: sink.clone(),
        controls: controls.clone(),
        cwd: dir.clone(),
        session: Some(session.clone()),
    };
    (agent, sink, controls, dir, session)
}

#[test]
fn replays_a_tool_turn() {
    let (agent, sink, _controls, dir, _log) = harness("turn", "demo.jsonl", 24, 200_000);
    let mut history = vec![Message::user("say hello")];
    let outcome = agent.run_turn(&mut history).unwrap();

    assert_eq!(outcome.steps, 2);
    assert!(!outcome.stopped_at_limit);
    assert!(!outcome.aborted);
    assert_eq!(
        outcome.text,
        "It printed hello from neon. Nothing else to do."
    );

    assert_eq!(history.len(), 4);
    assert_eq!(history[1].role, Role::Assistant);
    match &history[2].parts[0] {
        Part::Result {
            name,
            content,
            is_error,
            ..
        } => {
            assert_eq!(name, "bash");
            assert!(!is_error);
            assert!(content.contains("hello from neon"));
        }
        other => panic!("expected a tool result, got {other:?}"),
    }

    let printed = sink.dump();
    assert!(printed.contains("-> bash"));
    assert!(printed.contains("<- exit: 0"));

    let raw = std::fs::read(dir.join("spill/call_0.out")).unwrap();
    assert_eq!(String::from_utf8_lossy(&raw), "hello from neon\n");
}

#[test]
fn steering_lands_after_the_tool_and_drops_the_rest() {
    let (agent, _sink, controls, _dir, _log) = harness("steer", "steer.jsonl", 24, 200_000);
    controls.steer("stop after one");
    let mut history = vec![Message::user("run three commands")];
    let outcome = agent.run_turn(&mut history).unwrap();

    assert_eq!(outcome.text, "Stopped after the first one.");
    assert_eq!(history.len(), 4);

    // The assistant message keeps only the call that actually ran.
    let calls: Vec<&Part> = history[1]
        .parts
        .iter()
        .filter(|p| matches!(p, Part::Call(_)))
        .collect();
    assert_eq!(calls.len(), 1);

    // Results plus the steering text arrive as one user message.
    assert!(matches!(history[2].parts[0], Part::Result { .. }));
    assert!(matches!(history[2].parts[1], Part::Text(_)));
    assert_eq!(history[2].text(), "stop after one");
}

#[test]
fn abort_before_a_step_ends_the_turn() {
    let (agent, _sink, controls, _dir, _log) = harness("abort", "limit.jsonl", 24, 200_000);
    controls.abort();
    let mut history = vec![Message::user("keep going")];
    let outcome = agent.run_turn(&mut history).unwrap();

    assert!(outcome.aborted);
    assert_eq!(outcome.steps, 0);
    assert_eq!(history.len(), 1);
}

#[test]
fn stops_at_the_step_limit() {
    let (agent, _sink, _controls, _dir, _log) = harness("limit", "limit.jsonl", 3, 200_000);
    let mut history = vec![Message::user("keep going")];
    let outcome = agent.run_turn(&mut history).unwrap();

    assert!(outcome.stopped_at_limit);
    assert_eq!(outcome.steps, 3);
    assert_eq!(history.len(), 7);
}

#[test]
fn abort_mid_tool_kills_the_child_and_ends_the_turn() {
    // One long bash call, then a step that must never be requested.
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("midabort-tmp.jsonl");
    std::fs::write(
        &path,
        serde_json::to_string(&serde_json::json!({
            "candidates": [{ "content": { "parts": [
                { "functionCall": { "name": "bash", "args": { "command": "sleep 4 && echo late" } } }
            ] }, "finish_reason": "STOP" }] 
        }))
        .unwrap()
        + "\n" + &serde_json::to_string(&serde_json::json!({
            "candidates": [{ "content": { "parts": [{ "text": "never reached" }] }, "finish_reason": "STOP" }] 
        }))
        .unwrap() + "\n",
    )
    .unwrap();

    let (agent, sink, controls, _dir, _log) =
        harness("midabort", "midabort-tmp.jsonl", 24, 200_000);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        controls.abort();
    });
    let started = std::time::Instant::now();
    let mut history = vec![Message::user("take your time")];
    let outcome = agent.run_turn(&mut history).unwrap();
    let elapsed = started.elapsed();
    std::fs::remove_file(&path).ok();

    assert!(outcome.aborted);
    assert_eq!(outcome.steps, 1);
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "child was not killed: {elapsed:?}"
    );
    assert!(sink.dump().contains("[aborted]"));

    // The result carries the kill note, and the child never printed "late".
    let content = match &history[2].parts[0] {
        Part::Result { content, .. } => content.clone(),
        other => panic!("expected a tool result, got {other:?}"),
    };
    assert!(content.contains("killed by user"));
    assert!(!content.contains("late"));
}

#[test]
fn compaction_summarizes_the_oldest_span_and_keeps_originals_in_the_log() {
    let (agent, sink, _controls, dir, log) = harness("compact", "compact.jsonl", 24, 100);
    log.record_message(&Message::user("print a long list"))
        .unwrap();
    let mut history = vec![Message::user("print a long list")];
    let outcome = agent.run_turn(&mut history).unwrap();

    assert_eq!(outcome.steps, 2);
    assert!(!outcome.aborted);
    assert_eq!(outcome.text, "Done, the numbers were summarized.");

    // Model-facing history: summary message plus the final answer. The big tool
    // result is gone from the context and under the budget.
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].role, Role::User);
    assert!(history[0].text().contains("compacted"));
    assert!(history[0].text().contains("seq 1 2000"));
    assert!(estimate_tokens(&history) <= 100);
    assert!(sink.dump().contains("[context compacted]"));

    // The log keeps every original entry plus the compaction entry.
    let log_path = std::fs::read_dir(dir.join("sessions"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let raw = std::fs::read_to_string(&log_path).unwrap();
    assert!(raw.contains("\"k\":\"meta\""));
    assert!(raw.contains("\"k\":\"user\""));
    assert!(raw.contains("\"k\":\"assistant\""));
    assert!(raw.contains("\"k\":\"tool_result\""));
    assert!(raw.contains("\"k\":\"compaction\""));
    assert!(raw.contains("1 through 2000"));
}

#[test]
fn resumed_session_appends_after_replay() {
    let (agent, _sink, _controls, dir, log) = harness("resume", "demo.jsonl", 24, 200_000);
    log.record_message(&Message::user("say hello")).unwrap();
    let mut history = vec![Message::user("say hello")];
    agent.run_turn(&mut history).unwrap();
    let session_file = std::fs::read_dir(dir.join("sessions"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let id = session_file
        .file_name()
        .unwrap()
        .to_string_lossy()
        .strip_suffix(".jsonl")
        .unwrap()
        .to_string();

    // Replay the finished session; it rebuilds the four-message history.
    let (msgs, meta) = neon::session::replay(&session_file).unwrap();
    assert_eq!(meta.as_ref().unwrap()["k"], "meta");
    assert_eq!(msgs.len(), 4);

    // A resumed handle appends to the same file; replay shows both turns.
    let sess = neon::session::Session::existing(&id, &dir.join("sessions"));
    sess.record_message(&Message::user("and again")).unwrap();
    let (msgs2, _) = neon::session::replay(&session_file).unwrap();
    assert_eq!(msgs2.len(), 5);
    assert_eq!(msgs2[4].text(), "and again");
}
