//! CLI: TUI by default, plain REPL with `--plain`, print mode with `-p`.

use std::io::{Write, stdin, stdout};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use neon::agent::{Agent, Controls, StdioSink};
use neon::config::{Config, default_path};
use neon::llm::{Adapter, FixtureAdapter, GeminiAdapter};
use neon::messages::{Message, estimate_tokens};
use neon::plugin::PluginHost;
use neon::profile;
use neon::tools::{BashTool, PluginRow, Registry};
use neon::ui;
use neon::VERSION;

const HELP: &str = r#"neon - a minimal coding agent

  neon                    TUI in the current folder
  neon --plain            plain REPL, for pipes and dumb terminals
  neon -p "task"          run one task and exit
  neon --fixture f.jsonl  replay a recorded model stream, no key and no network
  neon --model <name>     override the model from config
  neon --profile <name>   plugin rows from ~/.config/neon/profiles/<name>.yaml
  neon --patch <file>     patch layer over the profile, last layer wins
  neon --config <path>    use another config file

TUI keys: Enter sends or steers · Ctrl+C aborts a run, quits when idle · Ctrl+D quits
TUI commands: /plugin list|load|unload|reload (applies at the next turn)
Config: ~/.config/neon/config.yaml (api_key there, never in the environment)
Plain REPL commands: :tools, :model, :hist, :plugin <verb>, :quit
"#;

struct Args {
    task: Option<String>,
    fixture: Option<PathBuf>,
    model: Option<String>,
    config: Option<PathBuf>,
    profile: Option<String>,
    patches: Vec<PathBuf>,
    plain: bool,
    help: bool,
    version: bool,
}

fn parse_args(raw: Vec<String>) -> Result<Args, String> {
    let mut a = Args {
        task: None,
        fixture: None,
        model: None,
        config: None,
        profile: None,
        patches: Vec::new(),
        plain: false,
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
            "--profile" => {
                let v = raw.get(i + 1).ok_or("--profile needs a name")?;
                a.profile = Some(v.clone());
                i += 2;
            }
            "--patch" => {
                let v = raw.get(i + 1).ok_or("--patch needs a file")?;
                a.patches.push(PathBuf::from(v));
                i += 2;
            }
            "--plain" => {
                a.plain = true;
                i += 1;
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
    if let Some(p) = args.profile.clone() {
        cfg.profile = p;
    }

    let adapter: Arc<dyn Adapter> = match &args.fixture {
        Some(path) => match FixtureAdapter::from_file(path) {
            Ok(f) => Arc::new(f),
            Err(e) => {
                eprintln!("neon: {e}");
                return ExitCode::from(4);
            }
        },
        None => Arc::new(GeminiAdapter::new(cfg.clone())),
    };

    let registry = Registry::new();
    registry.add(Box::new(BashTool), PluginRow::Core);

    let plugins = Arc::new(Mutex::new(PluginHost::new(
        Arc::new(neon::tools::ToolCtx {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            timeout_secs: cfg.bash_timeout_secs,
            excerpt_bytes: cfg.bash_excerpt_bytes,
            spill_dir: cfg.spill_dir.clone(),
        }),
        Arc::new(|line: &str| eprintln!("[plugin] {line}")),
    )));
    match profile::layers(&profile::profile_dir(), &cfg.profile, &args.patches) {
        Ok(rows) => {
            let mut host = plugins.lock().expect("plugin host");
            for row in rows {
                host.queue(row);
            }
        }
        Err(e) => {
            eprintln!("neon: {e}");
            return ExitCode::from(7);
        }
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let controls = Arc::new(Controls::default());
    let session: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));
    let cfg = Arc::new(cfg);

    if let Some(task) = args.task.clone() {
        let agent = Agent {
            adapter,
            registry: Arc::new(registry),
            plugins: plugins.clone(),
            cfg,
            sink: Arc::new(StdioSink),
            controls,
            cwd,
        };
        let mut history = vec![Message::user(&task)];
        match agent.run_turn(&mut history) {
            Ok(_) => return ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("\nneon: {e}");
                return ExitCode::from(5);
            }
        }
    }

    if args.plain {
        return plain_repl(
            adapter,
            Arc::new(registry),
            plugins,
            cfg,
            controls,
            session,
            cwd,
            &config_path,
        );
    }

    match ui::run(adapter, Arc::new(registry), plugins, cfg, session, controls, cwd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("neon: the terminal failed: {e}");
            eprintln!("try `neon --plain` for a plain REPL.");
            ExitCode::from(6)
        }
    }
}

fn plain_repl(
    adapter: Arc<dyn Adapter>,
    registry: Arc<Registry>,
    plugins: Arc<Mutex<PluginHost>>,
    cfg: Arc<Config>,
    controls: Arc<Controls>,
    session: Arc<Mutex<Vec<Message>>>,
    cwd: PathBuf,
    config_path: &std::path::Path,
) -> ExitCode {
    let agent = Agent {
        adapter,
        registry: registry.clone(),
        plugins: plugins.clone(),
        cfg: cfg.clone(),
        sink: Arc::new(StdioSink),
        controls,
        cwd: cwd.clone(),
    };

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
                for schema in registry.schemas() {
                    println!("- {}", schema.name);
                }
                continue;
            }
            ":model" => {
                println!("{} (config {})", cfg.model, config_path.display());
                continue;
            }
            ":hist" => {
                let history = session.lock().expect("session lock");
                println!(
                    "{} messages, about {} tokens",
                    history.len(),
                    estimate_tokens(&history)
                );
                continue;
            }
            other if other == ":plugin" || other.starts_with(":plugin ") => {
                let rest = other.trim_start_matches(":plugin").to_string();
                match plugins.lock().map(|mut host| host.handle_command(&rest)) {
                    Ok(Ok(note)) => println!("{note}"),
                    Ok(Err(e)) => println!("plugin: {e}"),
                    Err(_) => println!("plugin host is busy"),
                }
                continue;
            }
            other if other.starts_with(':') => {
                println!("unknown command: {other}");
                continue;
            }
            _ => {}
        }
        {
            let mut history = session.lock().expect("session lock");
            history.push(Message::user(&input));
            if let Err(e) = agent.run_turn(&mut history) {
                eprintln!("[error: {e}]");
            }
        }
    }
}
