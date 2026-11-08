//! The loop against recorded fixtures: no key, no network.

use neon::agent::Agent;
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

#[test]
fn replays_a_tool_turn() {
    let dir = workdir("turn");
    let adapter = FixtureAdapter::from_file(&fixture("demo.jsonl")).unwrap();
    let mut registry = Registry::new();
    registry.add(Box::new(BashTool));
    let mut cfg = Config::default();
    cfg.spill_dir = dir.join("spill");

    let mut printed = Vec::new();
    let mut history = vec![Message::user("say hello")];
    {
        let mut agent = Agent {
            adapter: &adapter,
            registry: &registry,
            cfg: &cfg,
            cwd: dir.clone(),
            out: &mut printed,
        };
        let outcome = agent.run_turn(&mut history).unwrap();
        assert_eq!(outcome.steps, 2);
        assert!(!outcome.stopped_at_limit);
        assert_eq!(outcome.text, "It printed hello from neon. Nothing else to do.");
    }

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

    let text = String::from_utf8(printed).unwrap();
    assert!(text.contains("-> bash"));
    assert!(text.contains("<- exit: 0"));

    let raw = std::fs::read(dir.join("spill/call_0.out")).unwrap();
    assert_eq!(String::from_utf8_lossy(&raw), "hello from neon\n");
}

#[test]
fn stops_at_the_step_limit() {
    let dir = workdir("limit");
    let adapter = FixtureAdapter::from_file(&fixture("limit.jsonl")).unwrap();
    let mut registry = Registry::new();
    registry.add(Box::new(BashTool));
    let mut cfg = Config::default();
    cfg.spill_dir = dir.join("spill");
    cfg.max_steps = 3;

    let mut printed = Vec::new();
    let mut history = vec![Message::user("keep going")];
    let mut agent = Agent {
        adapter: &adapter,
        registry: &registry,
        cfg: &cfg,
        cwd: dir.clone(),
        out: &mut printed,
    };
    let outcome = agent.run_turn(&mut history).unwrap();
    assert!(outcome.stopped_at_limit);
    assert_eq!(outcome.steps, 3);
    assert_eq!(history.len(), 7);
}
