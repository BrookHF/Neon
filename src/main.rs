//! CLI: TUI by default, plain REPL with `--plain`, print mode with `-p` or a bare task.

use std::io::{Write, stdin, stdout};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use neon::VERSION;
use neon::agent::{Agent, Controls, StdioSink};
use neon::config::{Config, default_path, ensure_template};
use neon::llm::{Adapter, FixtureAdapter, GeminiAdapter};
use neon::messages::{Message, estimate_tokens};
use neon::plugin::PluginHost;
use neon::profile;
use neon::session::{Session, human_age, list as list_sessions, replay, resolve};
use neon::tools::{BashTool, PluginRow, Registry};
use neon::ui;

const HELP: &str = r#"neon - a minimal coding agent

  neon                      TUI in the current folder
  neon "task"               run one task and exit, like -p
  neon -p "task"            run one task and exit
  neon --plain              plain REPL, for pipes and dumb terminals
  neon --fixture f.jsonl    replay a recorded model stream, no key and no network
  neon --sessions           list saved sessions, newest first
  neon --resume             continue the newest session (--resume <id> for a specific one)
  neon --model <name>       override the model from config
  neon --profile <name>     plugin rows from ~/.config/neon/profiles/<name>.yaml
  neon --patch <file>       patch layer over the profile, last layer wins
  neon --config <path>      use another config file
  neon --help               this text · neon --version prints the version

TUI keys: Enter sends or steers · PgUp/PgDown or mouse scroll the transcript · Tab completes a
command · Left/Right/Home/End move the cursor · Up/Down recall history · Backspace/Delete edit at
the cursor · Ctrl+C stops a run, quits when idle · Ctrl+D quits
TUI commands: /help /quit /tools /model <name> /hist /compact /sessions /resume <id>|last /new
  /plugin list|load|unload|reload queues and applies at the next turn
Plain REPL commands: the same verbs with a colon: :help :quit :tools :model :hist :compact
  :sessions :resume :new :plugin <verb>
Config: ~/.config/neon/config.yaml - api_key lives there, never in the environment. First run
  writes a commented starter file there. No key yet: run with --fixture <file>.
"#;

#[derive(Debug)]
struct Args {
    task: Option<String>,
    fixture: Option<PathBuf>,
    resume: Option<String>,
    sessions: bool,
    model: Option<String>,
    config: Option<PathBuf>,
    profile: Option<String>,
    patches: Vec<PathBuf>,
    plain: bool,
    help: bool,
    version: bool,
}

const FLAGS: &[&str] = &[
    "-p",
    "--print",
    "--plain",
    "--fixture",
    "--sessions",
    "--resume",
    "--model",
    "--profile",
    "--patch",
    "--config",
    "--help",
    "--version",
];

/// The value that follows a flag. The message says what the flag needs.
fn value(raw: &[String], i: usize, name: &str) -> Result<String, String> {
    raw.get(i + 1).cloned().ok_or_else(|| {
        format!(
            "{name} needs a value (neon {name} <value>), or run it alone for the newest session"
        )
    })
}

/// A flag's value only when the next argument is not another flag.
fn optional(raw: &[String], i: usize) -> Option<String> {
    raw.get(i + 1).filter(|v| !v.starts_with('-')).cloned()
}

/// The closest known flag to a mistyped one, or None.
fn suggest(arg: &str) -> Option<&'static str> {
    neon::closest(arg, FLAGS.iter().copied())
}

fn parse_args(raw: Vec<String>) -> Result<Args, String> {
    let mut a = Args {
        task: None,
        fixture: None,
        resume: None,
        sessions: false,
        model: None,
        config: None,
        profile: None,
        patches: Vec::new(),
        plain: false,
        help: false,
        version: false,
    };
    let mut i = 1usize;
    while i < raw.len() {
        let flag = raw[i].as_str();
        match flag {
            "-p" | "--print" => {
                a.task = Some(value(&raw, i, "-p")?);
                i += 2;
            }
            "--fixture" => {
                a.fixture = Some(PathBuf::from(value(&raw, i, "--fixture")?));
                i += 2;
            }
            "--resume" => match optional(&raw, i) {
                Some(id) => {
                    a.resume = Some(id);
                    i += 2;
                }
                None => {
                    a.resume = Some("last".to_string());
                    i += 1;
                }
            },
            "--sessions" => {
                a.sessions = true;
                i += 1;
            }
            "--model" => {
                a.model = Some(value(&raw, i, "--model")?);
                i += 2;
            }
            "--config" => {
                a.config = Some(PathBuf::from(value(&raw, i, "--config")?));
                i += 2;
            }
            "--profile" => {
                a.profile = Some(value(&raw, i, "--profile")?);
                i += 2;
            }
            "--patch" => {
                a.patches.push(PathBuf::from(value(&raw, i, "--patch")?));
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
            other if other.starts_with('-') => {
                return Err(match suggest(other) {
                    Some(f) => format!("unknown argument: {other} (did you mean {f}?)"),
                    None => format!("unknown argument: {other} (neon --help lists them)"),
                });
            }
            // A bare word is the task, so `neon "list files"` needs no flag.
            task => {
                if a.task.is_some() {
                    return Err(format!(
                        "a task is already set; extra argument: {task} (give one task, or drop --plain)"
                    ));
                }
                a.task = Some(task.to_string());
                i += 1;
            }
        }
    }
    Ok(a)
}

fn print_sessions(dir: &std::path::Path) -> ExitCode {
    match list_sessions(dir, 20) {
        Ok(rows) => {
            if rows.is_empty() {
                println!("no sessions in {} yet", dir.display());
                return ExitCode::SUCCESS;
            }
            println!("sessions in {} · newest first", dir.display());
            for r in rows {
                println!(
                    "  {} · {} · {} entries · {} · {}",
                    r.id,
                    if r.model.is_empty() { "?" } else { &r.model },
                    r.entries,
                    human_age(r.secs),
                    r.cwd
                );
            }
            println!("continue one with: neon --resume <id>");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("neon: {e}");
            ExitCode::from(8)
        }
    }
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
    let using_default_config = args.config.is_none();

    // First run: write a commented starter file so a person has something to edit,
    // instead of an error that names nothing. Fixture runs never touch the real config.
    if using_default_config && args.fixture.is_none() && !config_path.exists() {
        match ensure_template(&config_path) {
            Ok(()) => println!(
                "neon: created {} (mode 0600) - put your key in api_key to talk to the model",
                config_path.display()
            ),
            Err(e) => eprintln!("neon: could not create {}: {e}", config_path.display()),
        }
    }

    let mut cfg = match Config::load(&config_path) {
        Ok(c) => c,
        Err(_e) if args.fixture.is_some() => Config::default(),
        Err(e) => {
            eprintln!("neon: {e}");
            return ExitCode::from(3);
        }
    };
    if let Some(m) = args.model.clone() {
        cfg.set_model(&m);
    }
    if let Some(p) = args.profile.clone() {
        cfg.profile = p;
    }

    if args.sessions {
        return print_sessions(&cfg.sessions_dir);
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
                    eprintln!("neon: {e}\nrun neon --sessions to see what is saved.");
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
            if args.model.is_none()
                && let Some(m) = &meta_model
            {
                cfg.set_model(m);
            }
            if meta_model.as_deref() != Some(cfg.model().as_str())
                && let Err(e) = sess.record_model_change(&cfg.model())
            {
                eprintln!("neon: {e}");
                return ExitCode::from(8);
            }
            resumed = Some(msgs.len());
            history = msgs;
            Arc::new(sess)
        }
        None => {
            resumed = None;
            match Session::create(&cfg.sessions_dir, &cfg.model(), &cwd) {
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

    // Print mode: say where you are, once, and never ask for input.
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
            Ok(o) => {
                if o.stopped_at_limit {
                    eprintln!(
                        "\nneon: stopped at {} steps - raise max_steps in config.yaml",
                        o.steps
                    );
                }
                return ExitCode::SUCCESS;
            }
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

    match ui::run(
        adapter,
        Arc::new(registry),
        plugins,
        cfg,
        history,
        controls,
        cwd,
        log,
        resumed,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("neon: the terminal failed: {e}");
            eprintln!("try `neon --plain` for a plain REPL.");
            ExitCode::from(6)
        }
    }
}

/// Run one plain-REPL command. Some(code) ends the REPL; None means it printed and stays.
// The plain REPL needs every object a command can touch.
#[allow(clippy::too_many_arguments)]
fn plain_command(
    cmd: ui::Command,
    registry: &Registry,
    plugins: &Mutex<PluginHost>,
    cfg: &Config,
    agent: &Agent,
    history: &Mutex<Vec<Message>>,
    log: &Session,
    cwd: &std::path::Path,
    config_path: &std::path::Path,
) -> Option<ExitCode> {
    match cmd {
        ui::Command::Quit => Some(ExitCode::SUCCESS),
        ui::Command::Help => {
            for l in ui::help_lines() {
                println!("{l}");
            }
            None
        }
        ui::Command::Tools => {
            for schema in registry.schemas() {
                let row = if schema.plugin_row.is_empty() {
                    "core".to_string()
                } else {
                    schema.plugin_row.clone()
                };
                println!("- {} [{}] {}", schema.name, row, schema.description);
            }
            None
        }
        ui::Command::ShowModel => {
            println!("{} (config {})", cfg.model(), config_path.display());
            None
        }
        ui::Command::SetModel(name) => {
            let old = cfg.model();
            cfg.set_model(&name);
            if let Err(e) = log.record_model_change(&name) {
                println!("session log failed: {e}");
            }
            println!("model {old} -> {name}");
            None
        }
        ui::Command::Hist => {
            let hist = history.lock().expect("session lock");
            let tokens = estimate_tokens(&hist);
            let budget = cfg.context_budget_tokens.max(1);
            println!(
                "{} messages · {} tok of {} budget ({}%) · logged in {}",
                hist.len(),
                tokens,
                budget,
                (tokens * 100) / budget,
                log.path.display()
            );
            None
        }
        ui::Command::Compact => {
            let mut hist = history.lock().expect("session lock");
            match agent.compact_now(&mut hist) {
                Ok(replaced) => println!(
                    "{}",
                    if replaced == 0 {
                        "nothing to compact - the history already fits the budget".to_string()
                    } else {
                        format!(
                            "compacted {replaced} messages into a summary · now {} tok",
                            estimate_tokens(&hist)
                        )
                    }
                ),
                Err(e) => println!("compact failed: {e}"),
            }
            None
        }
        ui::Command::Sessions => {
            match list_sessions(&cfg.sessions_dir, 20) {
                Ok(rows) => {
                    if rows.is_empty() {
                        println!("no sessions in {} yet", cfg.sessions_dir.display());
                    } else {
                        for r in rows {
                            println!(
                                "- {} · {} · {} entries · {} · {}",
                                r.id,
                                if r.model.is_empty() { "?" } else { &r.model },
                                r.entries,
                                human_age(r.secs),
                                r.cwd
                            );
                        }
                    }
                }
                Err(e) => println!("sessions: {e}"),
            }
            None
        }
        ui::Command::Resume(target) => {
            match resolve(&cfg.sessions_dir, &target).and_then(|id| {
                replay(&cfg.sessions_dir.join(format!("{id}.jsonl")))
                    .map(|(msgs, meta)| (id, msgs, meta))
            }) {
                Ok((id, msgs, meta)) => {
                    if let Some(m) = meta
                        .as_ref()
                        .and_then(|m| m.get("model"))
                        .and_then(|v| v.as_str())
                    {
                        cfg.set_model(m);
                    }
                    let mut hist = history.lock().expect("session lock");
                    *hist = msgs;
                    println!(
                        "resumed session {} · {} messages · model {}",
                        id,
                        hist.len(),
                        cfg.model()
                    );
                }
                Err(e) => println!("resume: {e}"),
            }
            None
        }
        ui::Command::New => {
            match Session::create(&cfg.sessions_dir, &cfg.model(), cwd) {
                Ok(sess) => {
                    *history.lock().expect("session lock") = Vec::new();
                    println!("new session {} · the model starts empty", sess.id);
                }
                Err(e) => println!("new session: {e}"),
            }
            None
        }
        ui::Command::Plugin(rest) => {
            match plugins.lock().map(|mut host| host.handle_command(&rest)) {
                Ok(Ok(note)) => println!("{note}"),
                Ok(Err(e)) => println!("plugin: {e}"),
                Err(_) => println!("plugin host is busy"),
            }
            None
        }
        ui::Command::Unknown(name) => {
            let near = ui::suggested_command(&name);
            println!(
                "unknown command: {name}{} · :help lists them",
                match near {
                    Some(c) => format!(" (did you mean /{c}?)"),
                    None => String::new(),
                }
            );
            None
        }
    }
}

// The plain REPL needs every object a turn touches.
#[allow(clippy::too_many_arguments)]
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

    println!(
        "neon {VERSION} · model {} · folder {} · session {}",
        cfg.model(),
        cwd.display(),
        log.id
    );
    if let Some(n) = resumed {
        println!("resumed {n} messages");
    }
    if cfg.api_key.trim().is_empty() {
        println!(
            "no api_key yet · add it to {} · or run with --fixture <file>",
            default_path().display()
        );
    }
    println!(
        "commands: :help :quit :tools :model <name> :hist :compact :sessions :resume <id>|last :new :plugin <verb>"
    );

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
        // A leading colon or slash is a command; anything else is a task for the model.
        if input.starts_with(':') || input.starts_with('/') {
            if let Some(code) = plain_command(
                ui::parse_command(&input),
                &registry,
                &plugins,
                &cfg,
                &agent,
                &history,
                &log,
                &cwd,
                config_path,
            ) {
                return code;
            }
            continue;
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

#[cfg(test)]
mod tests {
    use super::{parse_args, suggest};

    #[test]
    fn flags_and_a_bare_task_parse() {
        let a = parse_args(vec![
            "neon".to_string(),
            "--fixture".to_string(),
            "f.jsonl".to_string(),
            "--resume".to_string(),
        ])
        .unwrap();
        assert_eq!(a.resume.as_deref(), Some("last")); // --resume alone means newest
        assert!(a.fixture.is_some());

        let a = parse_args(vec!["neon".to_string(), "list files".to_string()]).unwrap();
        assert_eq!(a.task.as_deref(), Some("list files"));

        let a = parse_args(vec![
            "neon".to_string(),
            "-p".to_string(),
            "go".to_string(),
            "--sessions".to_string(),
        ])
        .unwrap();
        assert!(a.sessions);
        assert_eq!(a.task.as_deref(), Some("go"));

        assert!(parse_args(vec!["neon".to_string(), "-p".to_string()]).is_err());
        assert!(
            parse_args(vec![
                "neon".to_string(),
                "-p".to_string(),
                "one".to_string(),
                "two".to_string()
            ])
            .is_err()
        );
    }

    #[test]
    fn a_typo_names_the_flag_it_meant() {
        assert_eq!(suggest("--restar"), Some("--resume"));
        assert_eq!(suggest("--fixtur"), Some("--fixture"));
        assert_eq!(suggest("--plains"), Some("--plain"));
        assert_eq!(suggest("--totally-different"), None);
        let err = parse_args(vec!["neon".to_string(), "--restar".to_string()]).unwrap_err();
        assert!(err.contains("did you mean --resume?"));
    }

    #[test]
    fn a_flag_before_another_flag_takes_no_value() {
        let a = parse_args(vec![
            "neon".to_string(),
            "--resume".to_string(),
            "--plain".to_string(),
        ])
        .unwrap();
        assert_eq!(a.resume.as_deref(), Some("last"));
        assert!(a.plain);
    }
}
