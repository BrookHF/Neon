//! Tool registry, the waterfall pipeline, and the one real tool: bash.

use std::io::Read as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::messages::{ToolCall, ToolSchema};

/// Where a registry row came from: the built-in core, or a plugin row id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginRow {
    Core,
    Plugin(String),
}

pub struct ToolCtx {
    pub cwd: PathBuf,
    pub timeout_secs: u64,
    pub excerpt_bytes: usize,
    pub spill_dir: PathBuf,
    /// Set by Ctrl+C. A tool that waits should kill its child and return early.
    pub abort: Arc<AtomicBool>,
}

pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn run(&self, args: &Value, ctx: &ToolCtx, call_id: &str) -> ToolResult;
}

/// Around-middleware, Cordis `waterfall` style: wrap the call, or short-circuit it.
pub type Interceptor =
    Box<dyn Fn(&ToolCall, &mut dyn FnMut(&ToolCall) -> ToolResult) -> ToolResult + Send + Sync>;

/// Thread-safe: the worker applies plugin rows at the turn boundary while the UI reads schemas.
/// Rows are Arc-wrapped so no lock is held while a tool runs.
pub struct Registry {
    tools: Mutex<Vec<(Arc<dyn Tool>, PluginRow)>>,
    interceptors: Mutex<Vec<(Arc<Interceptor>, PluginRow)>>,
}

impl Registry {
    pub fn new() -> Self {
        Registry {
            tools: Mutex::new(Vec::new()),
            interceptors: Mutex::new(Vec::new()),
        }
    }

    pub fn add(&self, tool: Box<dyn Tool>, row: PluginRow) {
        let tool: Arc<dyn Tool> = tool.into();
        self.tools
            .lock()
            .expect("registry tools lock")
            .push((tool, row));
    }

    pub fn add_interceptor(&self, i: Interceptor, row: PluginRow) {
        let i: Arc<Interceptor> = Arc::new(i);
        self.interceptors
            .lock()
            .expect("registry interceptor lock")
            .push((i, row));
    }

    /// Drop every row a plugin contributed. Used by unload and by re-registration.
    pub fn remove_all_plugin_rows(&self) {
        self.tools
            .lock()
            .expect("registry tools lock")
            .retain(|(_, r)| !matches!(r, PluginRow::Plugin(_)));
        self.interceptors
            .lock()
            .expect("registry interceptor lock")
            .retain(|(_, r)| !matches!(r, PluginRow::Plugin(_)));
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .lock()
            .expect("registry tools lock")
            .iter()
            .map(|(t, r)| ToolSchema {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
                plugin_row: match r {
                    PluginRow::Plugin(id) => id.clone(),
                    PluginRow::Core => String::new(),
                },
                index: 0,
            })
            .collect()
    }

    /// Ids of the plugin rows that currently contribute tools or interceptors.
    pub fn plugin_rows(&self) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for (_, row) in self.tools.lock().expect("registry tools lock").iter() {
            if let PluginRow::Plugin(id) = row {
                ids.push(id.clone());
            }
        }
        for (_, row) in self
            .interceptors
            .lock()
            .expect("registry interceptor lock")
            .iter()
        {
            if let PluginRow::Plugin(id) = row {
                ids.push(id.clone());
            }
        }
        ids.sort();
        ids.dedup();
        ids
    }

    pub fn execute(&self, call: &ToolCall, ctx: &ToolCtx) -> ToolResult {
        // Copy the Arc-wrapped interceptors, then run without holding any lock.
        let interceptors: Vec<Arc<Interceptor>> = self
            .interceptors
            .lock()
            .expect("registry interceptor lock")
            .iter()
            .map(|(i, _)| i.clone())
            .collect();
        self.run_at(&interceptors, 0, call, ctx)
    }

    fn run_at(
        &self,
        interceptors: &[Arc<Interceptor>],
        index: usize,
        call: &ToolCall,
        ctx: &ToolCtx,
    ) -> ToolResult {
        if index == interceptors.len() {
            return self.run_tool(call, ctx);
        }
        let mut next = |c: &ToolCall| self.run_at(interceptors, index + 1, c, ctx);
        interceptors[index](call, &mut next)
    }

    fn run_tool(&self, call: &ToolCall, ctx: &ToolCtx) -> ToolResult {
        let tool = self
            .tools
            .lock()
            .expect("registry tools lock")
            .iter()
            .find(|(t, _)| t.name() == call.name)
            .map(|(t, _)| t.clone());
        match tool {
            Some(tool) => tool.run(&call.args, ctx, &call.id),
            None => ToolResult {
                content: format!("unknown tool: {}", call.name),
                is_error: true,
            },
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command with /bin/bash -lc in the session folder. Returns exit code, stdout, and stderr."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command to run" },
                "cwd": { "type": "string", "description": "Folder to run in, defaults to the session folder" },
                "timeout_secs": { "type": "integer", "description": "Kill the command after this many seconds" }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    fn run(&self, args: &Value, ctx: &ToolCtx, call_id: &str) -> ToolResult {
        let command = match args.get("command").and_then(|c| c.as_str()) {
            Some(c) => c,
            None => {
                return ToolResult {
                    content: "bash needs a command string".to_string(),
                    is_error: true,
                };
            }
        };
        let cwd = match args.get("cwd").and_then(|c| c.as_str()) {
            Some(c) => PathBuf::from(c),
            None => ctx.cwd.clone(),
        };
        let timeout = args
            .get("timeout_secs")
            .and_then(|t| t.as_u64())
            .unwrap_or(ctx.timeout_secs)
            .max(1);

        std::fs::create_dir_all(&ctx.spill_dir).ok();
        let out_path = ctx.spill_dir.join(format!("{call_id}.out"));
        let err_path = ctx.spill_dir.join(format!("{call_id}.err"));
        let out_file = match std::fs::File::create(&out_path) {
            Ok(f) => f,
            Err(e) => {
                return ToolResult {
                    content: format!("cannot write spill file: {e}"),
                    is_error: true,
                };
            }
        };
        let err_file = match std::fs::File::create(&err_path) {
            Ok(f) => f,
            Err(e) => {
                return ToolResult {
                    content: format!("cannot write spill file: {e}"),
                    is_error: true,
                };
            }
        };

        let spawned = Command::new("/bin/bash")
            .arg("-lc")
            .arg(command)
            .current_dir(&cwd)
            .stdout(Stdio::from(out_file))
            .stderr(Stdio::from(err_file))
            .spawn();

        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                return ToolResult {
                    content: format!("cannot start /bin/bash: {e}"),
                    is_error: true,
                };
            }
        };

        let deadline = Instant::now() + Duration::from_secs(timeout);
        let mut timed_out = false;
        let mut killed_by_user = false;
        let code = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.code().unwrap_or(-1),
                Ok(None) => {
                    if ctx.abort.load(Ordering::Relaxed) {
                        let _ = child.kill();
                        let _ = child.wait();
                        killed_by_user = true;
                        break -1;
                    }
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        timed_out = true;
                        break 124;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    return ToolResult {
                        content: format!("waiting on the command failed: {e}"),
                        is_error: true,
                    };
                }
            }
        };

        let stdout = read_file(&out_path);
        let stderr = read_file(&err_path);

        let mut content = String::new();
        content.push_str(&format!("exit: {code}\n"));
        if killed_by_user {
            content.push_str("killed by user (Ctrl+C)\n");
        }
        if timed_out {
            content.push_str(&format!("timed out after {timeout}s\n"));
        }
        content.push_str("stdout:\n");
        content.push_str(&excerpt(&stdout, ctx.excerpt_bytes));
        content.push_str("\nstderr:\n");
        content.push_str(&excerpt(&stderr, ctx.excerpt_bytes));
        content.push_str(&format!(
            "\nraw output: {} and {}",
            out_path.display(),
            err_path.display()
        ));

        ToolResult {
            content,
            is_error: killed_by_user || timed_out || code != 0,
        }
    }
}

fn read_file(path: &std::path::Path) -> Vec<u8> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).ok();
    bytes
}

/// Head plus tail with a marker naming the dropped bytes. The raw file keeps everything.
pub fn excerpt(bytes: &[u8], cap: usize) -> String {
    if bytes.len() <= cap {
        return String::from_utf8_lossy(bytes).to_string();
    }
    let head_len = cap / 2;
    let tail_len = cap - head_len;
    let dropped = bytes.len() - cap;
    let head = String::from_utf8_lossy(&bytes[..head_len]);
    let tail = String::from_utf8_lossy(&bytes[bytes.len() - tail_len..]);
    format!("{head}\n...[{dropped} bytes dropped; the raw file has the full output]...\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx(dir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            timeout_secs: 10,
            excerpt_bytes: 512,
            spill_dir: dir.join("spill"),
            abort: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn abort_kills_a_running_command() {
        let dir = std::path::PathBuf::from("/tmp/neon-test-abort");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Registry::default();
        registry.add(Box::new(BashTool), PluginRow::Core);
        let call = ToolCall {
            id: "ab".to_string(),
            name: "bash".to_string(),
            args: json!({ "command": "sleep 5 && echo late" }),
            thought_sig: None,
        };
        let mut ctx = ctx(&dir);
        ctx.abort = Arc::new(AtomicBool::new(false));
        let flag = ctx.abort.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            flag.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let result = registry.execute(&call, &ctx);
        let elapsed = started.elapsed();
        assert!(result.is_error);
        assert!(result.content.contains("killed by user"));
        assert!(!result.content.contains("late"));
        assert!(
            elapsed < Duration::from_secs(3),
            "child was not killed: {elapsed:?}"
        );
    }

    #[test]
    fn runs_and_spills() {
        let dir = std::path::PathBuf::from("/tmp/neon-test-runs");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Registry::default();
        registry.add(Box::new(BashTool), PluginRow::Core);
        let call = ToolCall {
            id: "call_0".to_string(),
            name: "bash".to_string(),
            args: json!({ "command": "echo hello && echo bad >&2" }),
            thought_sig: None,
        };
        let r = registry.execute(&call, &ctx(&dir));
        assert!(!r.is_error);
        assert!(r.content.contains("exit: 0"));
        assert!(r.content.contains("hello"));
        assert!(r.content.contains("bad"));
        let raw = std::fs::read(dir.join("spill/call_0.out")).unwrap();
        assert_eq!(String::from_utf8_lossy(&raw), "hello\n");
    }

    #[test]
    fn timeout_kills_and_marks() {
        let dir = std::path::PathBuf::from("/tmp/neon-test-timeout");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Registry::default();
        registry.add(Box::new(BashTool), PluginRow::Core);
        let call = ToolCall {
            id: "c".to_string(),
            name: "bash".to_string(),
            args: json!({ "command": "sleep 5", "timeout_secs": 1 }),
            thought_sig: None,
        };
        let r = registry.execute(&call, &ctx(&dir));
        assert!(r.is_error);
        assert!(r.content.contains("timed out after 1s"));
    }

    #[test]
    fn interceptor_can_block() {
        let dir = std::path::PathBuf::from("/tmp/neon-test-block");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Registry::default();
        registry.add(Box::new(BashTool), PluginRow::Core);
        registry.add_interceptor(
            Box::new(|call, next| {
                if call
                    .args
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .contains("rm -rf")
                {
                    ToolResult {
                        content: "blocked by policy".to_string(),
                        is_error: true,
                    }
                } else {
                    next(call)
                }
            }),
            PluginRow::Core,
        );
        let blocked = registry.execute(
            &ToolCall {
                id: "a".to_string(),
                name: "bash".to_string(),
                args: json!({ "command": "rm -rf /" }),
                thought_sig: None,
            },
            &ctx(&dir),
        );
        assert!(blocked.is_error);
        assert_eq!(blocked.content, "blocked by policy");
        let ok = registry.execute(
            &ToolCall {
                id: "b".to_string(),
                name: "bash".to_string(),
                args: json!({ "command": "echo fine" }),
                thought_sig: None,
            },
            &ctx(&dir),
        );
        assert!(!ok.is_error);
    }

    #[test]
    fn excerpt_head_and_tail() {
        let bytes = vec![b'a'; 1000];
        let text = excerpt(&bytes, 200);
        assert!(text.contains("800 bytes dropped"));
        assert!(text.starts_with('a'));
        assert!(text.ends_with('a'));
        assert!(text.len() < 300);
    }
}
