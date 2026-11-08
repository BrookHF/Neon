//! CLI: interactive REPL, print mode, and a fixture mode that needs no key.

use std::io::{Write, stdin, stdout};
use std::path::PathBuf;
use std::process::ExitCode;

use neon::agent::Agent;
use neon::config::{Config, default_path};
use neon::llm::{Adapter, FixtureAdapter, GeminiAdapter};
use neon::messages::{Message, estimate_tokens};
use neon::tools::{BashTool, Registry};
use neon::VERSION;

const HELP: &str = r#"neon - a minimal coding agent

  neon                    interactive REPL in the current folder
  neon -p "task"          run one task and exit
  neon --fixture f.jsonl  replay a recorded model stream, no key and no network
  neon --model <name>     override the model from config
  neon --config <path>    use another config file

In the REPL: :tools, :model, :hist, :quit
Config: ~/.config/neon/config.yaml (api_key there, never in the environment)
"#;

struct Args {
    task: Option<String>,
    fixture: Option<PathBuf>,
    model: Option<String>,
    config: Option<PathBuf>,
    help: bool,
    version: bool,
}

fn parse_args(raw: Vec<String>) -> Result<Args, String> {
    let mut a = Args {
        task: None,
        fixture: None,
        model: None,
        config: None,
        help: false,
        version: false,
    };
    let mut i = 1;
    while i < raw.len() {
        match raw[i].as_str() {
            "-p" | "--print" => {
                let v = raw.get(i + 1).ok_or("-p needs a task")?;
                a.task = Some(v.clone());
                i += 2;
            }
            "--fixture" => {
                let v = raw.get(i + 1).ok_or("--fixture needs a file")?;
                a.fixture = Some(PathBuf::from(v));
                i += 2;
            }
            "--model" => {
                let v = raw.get(i + 1).ok_or("--model needs a name")?;
                a.model = Some(v.clone());
                i += 2;
            }
            "--config" => {
                let v = raw.get(i + 1).ok_or("--config needs a path")?;
                a.config = Some(PathBuf::from(v));
                i += 2;
            }
            "-h" | "--help" => {
                a.help = true;
                i += 1;
            }
            "-V" | "--version" => {
                a.version = true;
                i += 1;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(a)
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().collect()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("neon: {e}\n\n{HELP}");
            return ExitCode::from(2);
        }
    };
    if args.help {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if args.version {
        println!("neon {VERSION}");
        return ExitCode::SUCCESS;
    }

    let config_path = args.config.clone().unwrap_or_else(default_path);
    let mut cfg = match Config::load(&config_path) {
        Ok(c) => c,
        Err(_e) if args.fixture.is_some() => Config::default(),
        Err(e) => {
            eprintln!("neon: {e}");
            return ExitCode::from(3);
        }
    };
    if let Some(m) = args.model.clone() {
        cfg.model = m;
    }

    let adapter: Box<dyn Adapter> = match &args.fixture {
        Some(path) => match FixtureAdapter::from_file(path) {
            Ok(f) => Box::new(f),
            Err(e) => {
                eprintln!("neon: {e}");
                return ExitCode::from(4);
            }
        },
        None => Box::new(GeminiAdapter::new(cfg.clone())),
    };

    let mut registry = Registry::new();
    registry.add(Box::new(BashTool));

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut history: Vec<Message> = Vec::new();
    let stdout_handle = stdout();
    let mut out = stdout_handle.lock();

    let mut agent = Agent {
        adapter: adapter.as_ref(),
        registry: &registry,
        cfg: &cfg,
        cwd: cwd.clone(),
        out: &mut out,
    };

    if let Some(task) = args.task.clone() {
        history.push(Message::user(&task));
        match agent.run_turn(&mut history) {
            Ok(_) => return ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("\nneon: {e}");
                return ExitCode::from(5);
            }
        }
    }

    println!("neon {VERSION} · model {} · folder {}", cfg.model, cwd.display());
    println!("Type a task. :quit to leave. Ctrl+C leaves too.");
    let stdin_handle = stdin();
    loop {
        print!("> ");
        let _ = stdout().flush();
        let mut line = String::new();
        if stdin_handle.read_line(&mut line).unwrap_or(0) == 0 {
            println!();
            return ExitCode::SUCCESS;
        }
        let input = line.trim().to_string();
        if input.is_empty() {
            continue;
        }
        match input.as_str() {
            ":quit" | ":q" => return ExitCode::SUCCESS,
            ":tools" => {
                for s in registry.schemas() {
                    println!("- {}", s.name);
                }
                continue;
            }
            ":model" => {
                println!("{} (config {})", cfg.model, config_path.display());
                continue;
            }
            ":hist" => {
                println!(
                    "{} messages, about {} tokens",
                    history.len(),
                    estimate_tokens(&history)
                );
                continue;
            }
            other if other.starts_with(':') => {
                println!("unknown command: {other}");
                continue;
            }
            _ => {}
        }
        history.push(Message::user(&input));
        if let Err(e) = agent.run_turn(&mut history) {
            eprintln!("[error: {e}]");
        }
    }
}
