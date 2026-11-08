//! The loop against recorded fixtures: no key, no network.

use std::sync::Arc;

use neon::agent::{Agent, Controls, VecSink};
use neon::config::Config;
use neon::llm::FixtureAdapter;
use neon::messages::{Message, Part, Role};
use neon::tools::{BashTool, Registry};

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
) -> (Agent, Arc<VecSink>, Arc<Controls>, std::path::PathBuf) {
    let dir = workdir(name);
    let mut cfg = Config::default();
    cfg.spill_dir = dir.join("spill");
    cfg.max_steps = max_steps;
    let mut registry = Registry::new();
    registry.add(Box::new(BashTool));
    let sink = Arc::new(VecSink::default());
    let controls = Arc::new(Controls::default());
    let agent = Agent {
        adapter: Arc::new(FixtureAdapter::from_file(&fixture(fixture_name)).unwrap()),
        registry: Arc::new(registry),
        cfg: Arc::new(cfg),
        sink: sink.clone(),
        controls: controls.clone(),
        cwd: dir.clone(),
    };
    (agent, sink, controls, dir)
}

#[test]
fn replays_a_tool_turn() {
    let (agent, sink, _controls, dir) = harness("turn", "demo.jsonl", 24);
    let mut history = vec![Message::user("say hello")];
    let outcome = agent.run_turn(&mut history).unwrap();

    assert_eq!(outcome.steps, 2);
    assert!(!outcome.stopped_at_limit);
    assert!(!outcome.aborted);
    assert_eq!(outcome.text, "It printed hello from neon. Nothing else to do.");

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
    let (agent, _sink, controls, _dir) = harness("steer", "steer.jsonl", 24);
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
    let (agent, _sink, controls, _dir) = harness("abort", "limit.jsonl", 24);
    controls.abort();
    let mut history = vec![Message::user("keep going")];
    let outcome = agent.run_turn(&mut history).unwrap();

    assert!(outcome.aborted);
    assert_eq!(outcome.steps, 0);
    assert_eq!(history.len(), 1);
}

#[test]
fn stops_at_the_step_limit() {
    let (agent, _sink, _controls, _dir) = harness("limit", "limit.jsonl", 3);
    let mut history = vec![Message::user("keep going")];
    let outcome = agent.run_turn(&mut history).unwrap();

    assert!(outcome.stopped_at_limit);
    assert_eq!(outcome.steps, 3);
    assert_eq!(history.len(), 7);
}
