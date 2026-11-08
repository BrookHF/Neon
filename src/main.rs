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
use neon::session::{replay, resolve, Session};
use neon::tools::{BashTool, PluginRow, Registry};
use neon::ui;
use neon::VERSION;

const HELP: &str = r#"neon - a minimal coding agent

  neon                    TUI in the current folder
  neon --plain            plain REPL, for pipes and dumb terminals
  neon -p "task"          run one task and exit
  neon --fixture f.jsonl  replay a recorded model stream, no key and no network
  neon --resume <id>|last continue a saved session (~/.local/state/neon/sessions)
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
    resume: Option<String>,
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
        resume: None,
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
            "--resume" => {
                let v = raw.get(i + 1).ok_or("--resume needs a session id or last")?;
                a.resume = Some(v.clone());
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

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Session log: a new file, or resume an existing one. A resumed session keeps its
    // model unless --model overrides it; an override is recorded as a model_change entry.
    let mut history: Vec<Message> = Vec::new();
    let resumed: Option<usize>;
    let log: Arc<Session> = match &args.resume {
        Some(target) => {
            let id = match resolve(&cfg.sessions_dir, target) {
                Ok(id) => id,
                Err(e) => {
                    eprintln!("neon: {e}");
                    return ExitCode::from(8);
                }
            };
            let (msgs, meta) = match replay(&cfg.sessions_dir.join(format!("{id}.jsonl"))) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("neon: {e}");
                    return ExitCode::from(8);
                }
            };
            let sess = Session::existing(&id, &cfg.sessions_dir);
            let meta_model = meta
                .as_ref()
                .and_then(|m| m.get("model"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if args.model.is_none() {
                if let Some(m) = &meta_model {
                    cfg.model = m.clone();
                }
            }
            if meta_model.as_deref() != Some(cfg.model.as_str()) {
                if let Err(e) = sess.record_model_change(&cfg.model) {
                    eprintln!("neon: {e}");
                    return ExitCode::from(8);
                }
            }
            resumed = Some(msgs.len());
            history = msgs;
            Arc::new(sess)
        }
        None => {
            resumed = None;
            match Session::create(&cfg.sessions_dir, &cfg.model, &cwd) {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    eprintln!("neon: {e}");
                    return ExitCode::from(8);
                }
            }
        }
    };

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
    let controls = Arc::new(Controls::default());

    let plugins = Arc::new(Mutex::new(PluginHost::new(
        Arc::new(neon::tools::ToolCtx {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            timeout_secs: cfg.bash_timeout_secs,
            excerpt_bytes: cfg.bash_excerpt_bytes,
            spill_dir: cfg.spill_dir.clone(),
            abort: controls.abort_flag(),
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

    let history = Arc::new(Mutex::new(history));
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
            session: Some(log.clone()),
        };
        let msg = Message::user(&task);
        if let Err(e) = log.record_message(&msg) {
            eprintln!("neon: session log failed: {e}");
        }
        let mut history = history.lock().expect("history lock");
        history.push(msg);
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
            history,
            log,
            resumed,
            cwd,
            &config_path,
        );
    }

    match ui::run(adapter, Arc::new(registry), plugins, cfg, history, controls, cwd, log, resumed) {
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
    history: Arc<Mutex<Vec<Message>>>,
    log: Arc<Session>,
    resumed: Option<usize>,
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
        session: Some(log.clone()),
    };

    println!("neon {VERSION} · model {} · folder {} · session {}", cfg.model, cwd.display(), log.id);
    if let Some(n) = resumed {
        println!("resumed {n} messages");
    }
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
                let hist = history.lock().expect("session lock");
                println!(
                    "{} messages, about {} tokens, logged in {}",
                    hist.len(),
                    estimate_tokens(&hist),
                    log.path.display()
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
        let msg = Message::user(&input);
        if let Err(e) = log.record_message(&msg) {
            eprintln!("[session log failed: {e}]");
        }
        {
            let mut hist = history.lock().expect("session lock");
            hist.push(msg);
            if let Err(e) = agent.run_turn(&mut hist) {
                eprintln!("[error: {e}]");
            }
        }
    }
}
