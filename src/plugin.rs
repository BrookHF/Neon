//! WASM plugins: loader, capability grants, hot swap at the turn boundary.
//!
//! The interpreter is `wasmi` (pure Rust, no JIT), chosen over the `wasm3` crate,
//! which needs clang+bindgen at build and emits native code that PRoot on Android blocks.
//!
//! ABI v1. Strings cross as host-owned memory: the plugin exports its linear `memory`,
//! a `neon_scratch_base()` address, and `alloc`/`free` wrappers over the host imports.
//! Host imports `neon.alloc(len) -> ptr` and `neon.free(ptr)` bump inside that scratch
//! array; the host's `Store<u32>` data is the bump pointer. A returned pointer points at
//! the payload bytes; the payload length sits in the 4 little-endian bytes just before it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use yaml_rust2::YamlLoader;

use wasmi::{
    AsContext, AsContextMut, Caller, Engine, Instance, Linker, Memory, Module, Store, StoreContext,
    Val,
};

use crate::error::{AgentError, Result};
use crate::http;
use crate::messages::ToolSchema;
use crate::tools::{PluginRow, Registry, Tool, ToolCtx, ToolResult};

pub const NEON_ABI_VERSION: i64 = 1;
/// The scratch bump starts at the plugin's `neon_scratch_base()` plus this headroom,
/// so the 4-byte length prefix lands inside the plugin's own scratch array.
pub const SCRATCH_HEAD: u32 = 4;
/// The import namespace every host service lives in.
pub const HOST_MODULE: &str = "neon";

/// Host services a manifest can name. An ungranted one is never wired as an import.
pub const CAPABILITIES: [&str; 6] = [
    "fs.read",
    "fs.write",
    "bash.run",
    "net.fetch",
    "clock.now",
    "log",
];

/// One plugin row: from a profile layer or a `/plugin load` action. Paths may start with `~/`.
#[derive(Clone, Debug)]
pub struct Row {
    pub id: String,
    pub path: PathBuf,
    /// True when the layer set the plugin path; patches that omit it keep the old path.
    pub path_set: bool,
    pub disabled: bool,
    pub grants: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Manifest {
    pub id: String,
    pub capabilities: Vec<String>,
    /// Tool names the manifest declares; checked against `neon_tools()` at load.
    pub tools: Vec<String>,
}

fn as_string_list(v: &yaml_rust2::Yaml) -> Vec<String> {
    match v.as_vec() {
        Some(items) => items
            .iter()
            .filter_map(|i| i.as_str().map(|s| s.to_string()))
            .collect(),
        None => Vec::new(),
    }
}

impl Manifest {
    pub fn load(dir: &Path) -> Result<Manifest> {
        let file = dir.join("plugin.yaml");
        let text = std::fs::read_to_string(&file)
            .map_err(|e| AgentError::Config(format!("{}: {e}", file.display())))?;
        let docs = YamlLoader::load_from_str(&text)
            .map_err(|e| AgentError::Config(format!("{}: {e}", file.display())))?;
        let doc = docs
            .first()
            .ok_or_else(|| AgentError::Config("empty plugin.yaml".to_string()))?;
        let id = doc["id"]
            .as_str()
            .ok_or_else(|| AgentError::Config(format!("{}: id is required", file.display())))?
            .to_string();
        Ok(Manifest {
            id,
            capabilities: as_string_list(&doc["capabilities"]),
            tools: as_string_list(&doc["tools"]),
        })
    }
}

fn adapter(msg: impl Into<String>) -> AgentError {
    AgentError::Adapter(msg.into())
}

/// Read `(ptr, len)` from the plugin's memory; len is the 4 little-endian bytes before ptr.
fn read_string(ctx: StoreContext<'_, u32>, memory: &Memory, ptr: u32) -> Result<String> {
    let mut header = [0u8; 4];
    memory
        .read(ctx, (ptr - 4) as usize, &mut header)
        .map_err(|e| adapter(format!("string header unreadable: {e}")))?;
    let len = u32::from_le_bytes(header);
    if len == 0 {
        return Err(adapter("plugin returned an empty string".to_string()));
    }
    let mut payload = vec![0u8; len as usize];
    memory
        .read(ctx, ptr as usize, &mut payload)
        .map_err(|e| adapter(format!("plugin string unreadable: {e}")))?;
    Ok(String::from_utf8_lossy(&payload).to_string())
}

/// Write a service result into the scratch area. Returns the payload pointer.
fn write_string(caller: &mut Caller<'_, u32>, text: &str) -> Result<u32> {
    let payload = text.as_bytes();
    let ptr = *caller.data() + SCRATCH_HEAD;
    let len_bytes = payload.len().to_le_bytes();
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| adapter("plugin exports no memory"))?;
    memory
        .write(
            caller.as_context_mut(),
            (ptr - SCRATCH_HEAD) as usize,
            &len_bytes,
        )
        .map_err(|e| adapter(format!("cannot write service length: {e}")))?;
    memory
        .write(caller.as_context_mut(), ptr as usize, payload)
        .map_err(|e| adapter(format!("cannot write service string: {e}")))?;
    *caller.data_mut() = ptr + payload.len() as u32;
    Ok(ptr)
}

/// Host import name for a capability: `clock.now` is imported as `neon_clock_now`.
fn import_name(cap: &str) -> String {
    format!("neon_{}", cap.replace('.', "_"))
}

/// The capability an import name refers back to.
fn cap_of(import: &str) -> String {
    import
        .strip_prefix("neon_")
        .unwrap_or(import)
        .replace('_', ".")
}

struct PluginState {
    store: Store<u32>,
    instance: Instance,
}

/// A loaded plugin: manifest, grants, and the wasm instance behind a lock.
pub struct Plugin {
    pub id: String,
    pub grants: Vec<String>,
    state: Arc<Mutex<PluginState>>,
}

impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("id", &self.id)
            .field("grants", &self.grants)
            .finish()
    }
}

impl Plugin {
    pub fn plugin_id(&self) -> &str {
        &self.id
    }

    fn memory(ctx: StoreContext<'_, u32>, instance: &Instance) -> Result<Memory> {
        instance
            .get_memory(ctx, "memory")
            .ok_or_else(|| adapter("plugin exports no memory"))
    }

    fn call(instance: &Instance, store: &mut Store<u32>, name: &str, args: &[Val]) -> Result<i32> {
        let func = instance
            .get_func(&*store, name)
            .ok_or_else(|| adapter(format!("export {name} is missing")))?;
        let mut results = [Val::I32(0)];
        func.call(store, args, &mut results)
            .map_err(|e| adapter(format!("{name} failed: {e}")))?;
        results[0]
            .i32()
            .ok_or_else(|| adapter(format!("{name} did not return an integer")))
    }

    fn call_void(
        instance: &Instance,
        store: &mut Store<u32>,
        name: &str,
        args: &[Val],
    ) -> Result<()> {
        let func = instance
            .get_func(&*store, name)
            .ok_or_else(|| adapter(format!("export {name} is missing")))?;
        func.call(store, args, &mut [])
            .map_err(|e| adapter(format!("{name} failed: {e}")))
    }

    /// Call an export that takes no arguments and returns a length-prefixed string pointer.
    fn call_export_string(&self, name: &str) -> Result<String> {
        let mut guard = self.state.lock().expect("plugin lock");
        let instance = guard.instance; // Instance is Copy
        let mut store = std::mem::take(&mut guard.store);
        let out = Self::export_string(&instance, &mut store, name);
        guard.store = store;
        out
    }

    fn export_string(instance: &Instance, store: &mut Store<u32>, name: &str) -> Result<String> {
        let ptr = Self::call(instance, store, name, &[])?;
        let memory = Self::memory(store.as_context(), instance)?;
        let text = read_string(store.as_context(), &memory, ptr as u32)?;
        Self::call_void(instance, store, "free", &[Val::I32(ptr)])?;
        Ok(text)
    }

    /// Call `neon_handle(tool_index, args_ptr, args_len)` and read the JSON reply.
    fn call_handle(&self, index: u32, args: &[u8]) -> Result<String> {
        let mut guard = self.state.lock().expect("plugin lock");
        let instance = guard.instance;
        let mut store = std::mem::take(&mut guard.store);
        let out = self.handle_in(&instance, &mut store, index, args);
        guard.store = store;
        out
    }

    fn handle_in(
        &self,
        instance: &Instance,
        store: &mut Store<u32>,
        index: u32,
        args: &[u8],
    ) -> Result<String> {
        let ptr = Self::call(instance, store, "alloc", &[Val::I32(args.len() as i32)])?;
        if ptr < 0 {
            return Err(adapter("plugin scratch is full"));
        }
        let memory = Self::memory(store.as_context(), instance)?;
        memory
            .write(store.as_context_mut(), ptr as usize, args)
            .map_err(|e| adapter(format!("cannot write args: {e}")))?;
        let reply_ptr = Self::call(
            instance,
            store,
            "neon_handle",
            &[
                Val::I32(index as i32),
                Val::I32(ptr),
                Val::I32(args.len() as i32),
            ],
        )
        .map_err(|e| adapter(format!("{}: {e}", self.id)))?;
        if reply_ptr < 0 {
            return Err(adapter(format!("{}: neon_handle failed", self.id)));
        }
        let reply = read_string(store.as_context(), &memory, reply_ptr as u32)?;
        Self::call_void(instance, store, "free", &[Val::I32(ptr)])?;
        Ok(reply)
    }

    pub fn tool_schemas(&self) -> Result<Vec<ToolSchema>> {
        let text = self.call_export_string("neon_tools")?;
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|e| adapter(format!("{}: neon_tools is not JSON: {e}", self.id)))?;
        let items = parsed
            .as_array()
            .ok_or_else(|| adapter(format!("{}: neon_tools is not an array", self.id)))?;
        Ok(items
            .iter()
            .enumerate()
            .map(|(i, t)| ToolSchema {
                name: t["name"].as_str().unwrap_or("").to_string(),
                description: t["description"].as_str().unwrap_or("").to_string(),
                parameters: t["parameters"].clone(),
                plugin_row: self.id.clone(),
                index: i as u32,
            })
            .collect())
    }

    /// Run the plugin's `neon_dispose`. The host drops the instance afterwards.
    pub fn dispose(&self) {
        let mut guard = self.state.lock().expect("plugin lock");
        let instance = guard.instance;
        let mut store = std::mem::take(&mut guard.store);
        let _ = Self::call_void(&instance, &mut store, "neon_dispose", &[]);
        guard.store = store;
    }

    /// Read a manifest dir, check the ABI, wire only the granted imports, instantiate.
    pub fn load(
        engine: &Engine,
        dir: &Path,
        row: &Row,
        ctx: Arc<ToolCtx>,
        log: Arc<dyn Fn(&str) + Send + Sync>,
    ) -> Result<Plugin> {
        let manifest = Manifest::load(dir)?;
        if manifest.id != row.id {
            return Err(AgentError::Config(format!(
                "row id {} does not match manifest id {}",
                row.id, manifest.id
            )));
        }
        for cap in &manifest.capabilities {
            if !CAPABILITIES.contains(&cap.as_str()) {
                return Err(AgentError::Config(format!(
                    "{}: unknown capability {cap}",
                    dir.join("plugin.yaml").display()
                )));
            }
        }

        let wasm_path = dir.join("plugin.wasm");
        let bytes = std::fs::read(&wasm_path)
            .map_err(|e| AgentError::Config(format!("{}: {e}", wasm_path.display())))?;
        let module = Module::new(engine, &bytes)
            .map_err(|e| adapter(format!("{}: {e}", wasm_path.display())))?;

        for name in [
            "neon_abi_version",
            "neon_tools",
            "neon_handle",
            "neon_dispose",
            "neon_scratch_base",
            "memory",
            "alloc",
            "free",
        ] {
            if module.get_export(name).is_none() {
                return Err(adapter(format!(
                    "{}: plugin is missing export {name}",
                    wasm_path.display()
                )));
            }
        }

        let mut store = Store::new(engine, 0);
        let mut linker = Linker::<u32>::new(engine);

        linker
            .func_wrap(
                HOST_MODULE,
                "neon_alloc",
                move |mut caller: Caller<'_, u32>, len: i32| -> i32 {
                    let len = len.max(0) as u32;
                    let next = *caller.data();
                    let ptr = next + SCRATCH_HEAD;
                    let end = ptr + len;
                    let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                        Some(m) => m,
                        None => return -1,
                    };
                    if end as usize > memory.data_size(caller.as_context()) {
                        return -1;
                    }
                    let len_bytes = len.to_le_bytes();
                    if memory
                        .write(
                            caller.as_context_mut(),
                            (ptr - SCRATCH_HEAD) as usize,
                            &len_bytes,
                        )
                        .is_err()
                    {
                        return -1;
                    }
                    *caller.data_mut() = end;
                    ptr as i32
                },
            )
            .map_err(|e| adapter(format!("alloc import: {e}")))?;
        linker
            .func_wrap(
                HOST_MODULE,
                "neon_free",
                |_caller: Caller<'_, u32>, _ptr: i32| {},
            )
            .map_err(|e| adapter(format!("free import: {e}")))?;

        // Wire a service only when the row grants it: an ungranted name is never wired.
        for cap in &manifest.capabilities {
            if !row.grants.iter().any(|g| g == cap) {
                continue;
            }
            wire_service(&mut linker, cap, ctx.clone(), log.clone())?;
        }

        // A plugin that declares an import its row did not grant is refused here, so the
        // error names the capability instead of a bare linker error.
        let ungranted: Vec<String> = module
            .imports()
            .filter(|i| i.module() == HOST_MODULE)
            .filter(|i| i.name() != "neon_alloc" && i.name() != "neon_free")
            .filter(|i| !row.grants.iter().any(|g| g == &cap_of(i.name())))
            .map(|i| cap_of(i.name()))
            .collect();
        if !ungranted.is_empty() {
            return Err(adapter(format!(
                "{}: plugin declares {}, but the row grants {:?}. Ungranted imports are not wired.",
                wasm_path.display(),
                ungranted.join(", "),
                row.grants
            )));
        }

        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(|e| adapter(format!("{}: {e}", wasm_path.display())))?;

        let abi = Self::call(&instance, &mut store, "neon_abi_version", &[])?;
        if abi != NEON_ABI_VERSION as i32 {
            return Err(adapter(format!(
                "plugin ABI {abi} does not match this host ABI {NEON_ABI_VERSION}"
            )));
        }

        // The scratch bump starts inside the plugin's own scratch array.
        let scratch_base = Self::call(&instance, &mut store, "neon_scratch_base", &[])?;
        *store.data_mut() = scratch_base as u32 + SCRATCH_HEAD;

        let plugin = Plugin {
            id: manifest.id.clone(),
            grants: row.grants.clone(),
            state: Arc::new(Mutex::new(PluginState { store, instance })),
        };

        // The manifest's declared tool names must match what the plugin exports.
        let schemas = plugin.tool_schemas()?;
        for t in &manifest.tools {
            if !schemas.iter().any(|s| &s.name == t) {
                return Err(AgentError::Config(format!(
                    "{}: manifest declares tool {t}, the plugin does not export it",
                    wasm_path.display()
                )));
            }
        }

        Ok(plugin)
    }
}

/// Read a host-written string from the caller's memory, via its instance exports.
fn caller_text(caller: &Caller<'_, u32>, ptr: i32) -> Result<String> {
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| adapter("plugin exports no memory"))?;
    read_string(caller.as_context(), &memory, ptr as u32)
}

fn wire_service(
    linker: &mut Linker<u32>,
    cap: &str,
    ctx: Arc<ToolCtx>,
    log: Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<()> {
    let name = import_name(cap);
    match cap {
        "log" => {
            linker
                .func_wrap(
                    HOST_MODULE,
                    &name,
                    move |caller: Caller<'_, u32>, ptr: i32| -> i32 {
                        match caller_text(&caller, ptr) {
                            Ok(text) => {
                                log(&text);
                                1
                            }
                            Err(_) => -1,
                        }
                    },
                )
                .map_err(|e| adapter(format!("log: {e}")))?;
            Ok(())
        }
        "clock.now" => {
            linker
                .func_wrap(
                    HOST_MODULE,
                    &name,
                    move |mut caller: Caller<'_, u32>| -> i32 {
                        let secs = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        match write_string(&mut caller, &secs.to_string()) {
                            Ok(ptr) => ptr as i32,
                            Err(_) => -1,
                        }
                    },
                )
                .map_err(|e| adapter(format!("clock.now: {e}")))?;
            Ok(())
        }
        "fs.read" => {
            linker
                .func_wrap(
                    HOST_MODULE,
                    &name,
                    move |mut caller: Caller<'_, u32>, ptr: i32| -> i32 {
                        let path = match caller_text(&caller, ptr) {
                            Ok(p) => p,
                            Err(_) => return -1,
                        };
                        match std::fs::read(PathBuf::from(path)) {
                            Ok(bytes) => {
                                match write_string(&mut caller, &String::from_utf8_lossy(&bytes)) {
                                    Ok(p) => p as i32,
                                    Err(_) => -1,
                                }
                            }
                            Err(_) => -1,
                        }
                    },
                )
                .map_err(|e| adapter(format!("fs.read: {e}")))?;
            Ok(())
        }
        "fs.write" => {
            linker
                .func_wrap(
                    HOST_MODULE,
                    &name,
                    move |caller: Caller<'_, u32>, path_ptr: i32, data_ptr: i32| -> i32 {
                        let path = match caller_text(&caller, path_ptr) {
                            Ok(p) => p,
                            Err(_) => return -1,
                        };
                        let data = match caller_text(&caller, data_ptr) {
                            Ok(d) => d,
                            Err(_) => return -1,
                        };
                        match std::fs::write(PathBuf::from(path), data) {
                            Ok(()) => 1,
                            Err(_) => -1,
                        }
                    },
                )
                .map_err(|e| adapter(format!("fs.write: {e}")))?;
            Ok(())
        }
        "bash.run" => {
            linker
                .func_wrap(
                    HOST_MODULE,
                    &name,
                    move |mut caller: Caller<'_, u32>, ptr: i32| -> i32 {
                        let command = match caller_text(&caller, ptr) {
                            Ok(c) => c,
                            Err(_) => return -1,
                        };
                        let call_id = format!("plugin-bash-{}", std::process::id());
                        let result = crate::tools::BashTool.run(
                            &json!({ "command": command }),
                            &ctx,
                            &call_id,
                        );
                        match write_string(&mut caller, &result.content) {
                            Ok(p) => p as i32,
                            Err(_) => -1,
                        }
                    },
                )
                .map_err(|e| adapter(format!("bash.run: {e}")))?;
            Ok(())
        }
        "net.fetch" => {
            linker
                .func_wrap(
                    HOST_MODULE,
                    &name,
                    move |mut caller: Caller<'_, u32>, ptr: i32| -> i32 {
                        let url = match caller_text(&caller, ptr) {
                            Ok(u) => u,
                            Err(_) => return -1,
                        };
                        match http::get(&url) {
                            Ok(mut resp) => {
                                let body = http::read_all(&mut resp.body);
                                match write_string(&mut caller, &String::from_utf8_lossy(&body)) {
                                    Ok(p) => p as i32,
                                    Err(_) => -1,
                                }
                            }
                            Err(_) => -1,
                        }
                    },
                )
                .map_err(|e| adapter(format!("net.fetch: {e}")))?;
            Ok(())
        }
        other => Err(AgentError::Config(format!("unknown capability {other}"))),
    }
}

/// One tool a plugin exported. `index` is its position in `neon_tools()`.
pub struct WasmTool {
    index: u32,
    schema: ToolSchema,
    plugin: Arc<Plugin>,
}

impl WasmTool {
    pub fn new(plugin: Arc<Plugin>, schema: ToolSchema) -> WasmTool {
        WasmTool {
            index: schema.index,
            schema,
            plugin,
        }
    }

    pub fn plugin_id(&self) -> &str {
        self.plugin.plugin_id()
    }
}

impl Tool for WasmTool {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn description(&self) -> &str {
        &self.schema.description
    }
    fn parameters(&self) -> Value {
        self.schema.parameters.clone()
    }
    fn run(&self, args: &Value, _ctx: &ToolCtx, _call_id: &str) -> ToolResult {
        match self
            .plugin
            .call_handle(self.index, args.to_string().as_bytes())
        {
            Err(e) => ToolResult {
                content: format!("{}: {e}", self.plugin_id()),
                is_error: true,
            },
            Ok(reply) => match serde_json::from_str::<Value>(&reply) {
                Ok(v) => ToolResult {
                    content: v["content"].as_str().unwrap_or("").to_string(),
                    is_error: v["ok"].as_bool() != Some(true),
                },
                Err(e) => ToolResult {
                    content: format!("{}: neon_handle returned bad JSON: {e}", self.plugin_id()),
                    is_error: true,
                },
            },
        }
    }
}

/// The plugin stack plus the pending hot-swap queue. Changes land at the turn boundary.
pub struct PluginHost {
    engine: Engine,
    ctx: Arc<ToolCtx>,
    log: Arc<dyn Fn(&str) + Send + Sync>,
    rows: Vec<Row>,
    loaded: BTreeMap<String, Arc<Plugin>>,
    pending: Vec<Row>,
    unload: Vec<String>,
}

impl PluginHost {
    pub fn new(ctx: Arc<ToolCtx>, log: Arc<dyn Fn(&str) + Send + Sync>) -> PluginHost {
        PluginHost {
            engine: Engine::default(),
            ctx,
            log,
            rows: Vec::new(),
            loaded: BTreeMap::new(),
            pending: Vec::new(),
            unload: Vec::new(),
        }
    }

    /// Queue a row for the next boundary. A `disabled` row unloads by id.
    pub fn queue(&mut self, row: Row) {
        self.pending.push(row);
    }

    pub fn queue_unload(&mut self, id: &str) {
        self.unload.push(id.to_string());
    }

    pub fn queue_reload(&mut self, id: &str) -> Result<()> {
        let row = self
            .rows
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or_else(|| adapter(format!("plugin {id} is not loaded")))?;
        self.unload.push(id.to_string());
        self.pending.push(row);
        Ok(())
    }

    pub fn loaded_ids(&self) -> Vec<String> {
        self.loaded.keys().cloned().collect()
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Handle `/plugin list|load|unload|reload [id]` (also `:plugin` in plain mode).
    /// load reads `~/.config/neon/plugins/<id>` and grants what the manifest declares.
    /// Changes queue; they land at the next turn boundary.
    pub fn handle_command(&mut self, args: &str) -> Result<String> {
        let mut parts = args.split_whitespace();
        let verb = parts.next().unwrap_or("list");
        match verb {
            "list" => {
                let loaded = self.loaded_ids();
                if loaded.is_empty() {
                    Ok("no plugins loaded".to_string())
                } else {
                    Ok(format!("loaded: {}", loaded.join(", ")))
                }
            }
            "load" => {
                let id = parts
                    .next()
                    .ok_or_else(|| adapter("plugin load needs an id"))?;
                let dir = crate::profile::plugin_dir().join(id);
                let manifest = Manifest::load(&dir)?;
                self.queue(Row {
                    id: id.to_string(),
                    path: dir,
                    path_set: true,
                    disabled: false,
                    grants: manifest.capabilities.clone(),
                });
                Ok(format!(
                    "queued {id}, granted {:?}; applies at the next turn",
                    manifest.capabilities
                ))
            }
            "unload" => {
                let id = parts
                    .next()
                    .ok_or_else(|| adapter("plugin unload needs an id"))?;
                self.queue_unload(id);
                Ok(format!("queued unload of {id}; applies at the next turn"))
            }
            "reload" => {
                let id = parts
                    .next()
                    .ok_or_else(|| adapter("plugin reload needs an id"))?;
                self.queue_reload(id)?;
                Ok(format!("queued reload of {id}; applies at the next turn"))
            }
            other => Err(AgentError::Config(format!(
                "unknown plugin command: {other}"
            ))),
        }
    }

    /// Apply the queue, then re-register every loaded plugin's tools on the registry.
    /// Called at the turn boundary, never mid-request.
    pub fn apply(&mut self, registry: &Registry) -> Result<Vec<String>> {
        let pending = std::mem::take(&mut self.pending);
        let unload = std::mem::take(&mut self.unload);
        // Unload first, so a reload row re-loads the plugin it just dropped.
        for id in unload {
            if let Some(plugin) = self.loaded.remove(&id) {
                plugin.dispose();
            }
            self.rows.retain(|r| r.id != id);
        }
        for row in pending {
            if row.disabled {
                if let Some(plugin) = self.loaded.remove(&row.id) {
                    plugin.dispose();
                }
                self.rows.retain(|r| r.id != row.id);
                continue;
            }
            let plugin = Plugin::load(
                &self.engine,
                &row.path,
                &row,
                self.ctx.clone(),
                self.log.clone(),
            )?;
            if let Some(old) = self.loaded.remove(&row.id) {
                old.dispose();
            }
            self.rows.retain(|r| r.id != row.id);
            self.rows.push(row);
            self.loaded.insert(plugin.id.clone(), Arc::new(plugin));
        }

        registry.remove_all_plugin_rows();
        for (row_id, plugin) in &self.loaded {
            for schema in plugin.tool_schemas()? {
                registry.add(
                    Box::new(WasmTool::new(plugin.clone(), schema)),
                    PluginRow::Plugin(row_id.clone()),
                );
            }
        }
        Ok(self.loaded_ids())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/plugins/echo")
    }

    fn test_ctx() -> Arc<ToolCtx> {
        Arc::new(ToolCtx {
            cwd: PathBuf::from("/tmp"),
            timeout_secs: 10,
            excerpt_bytes: 512,
            spill_dir: PathBuf::from("/tmp/neon-test-plugin-spill"),
            abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// The log sink plus the collected lines, as tests use them.
    type LogSink = (Arc<dyn Fn(&str) + Send + Sync>, Arc<Mutex<Vec<String>>>);

    fn logged() -> LogSink {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = lines.clone();
        (
            Arc::new(move |s: &str| sink.lock().unwrap().push(s.to_string())),
            lines,
        )
    }

    fn full_row(path: PathBuf) -> Row {
        Row {
            id: "echo".to_string(),
            path,
            path_set: true,
            disabled: false,
            grants: vec!["log".to_string(), "clock.now".to_string()],
        }
    }

    #[test]
    fn loads_fixture_and_checks_manifest_tools() {
        let dir = fixture_dir();
        let plugin = Plugin::load(
            &Engine::default(),
            &dir,
            &full_row(dir.clone()),
            test_ctx(),
            logged().0,
        )
        .expect("a granted row loads");
        let schemas = plugin.tool_schemas().expect("neon_tools parses");
        assert_eq!(schemas.len(), 2);
        assert_eq!(schemas[0].name, "echo");
        assert_eq!(schemas[1].name, "when");
    }

    #[test]
    fn ungranted_import_refuses() {
        let dir = fixture_dir();
        let row = Row {
            id: "echo".to_string(),
            path: dir.clone(),
            path_set: true,
            disabled: false,
            grants: vec![],
        };
        let err = Plugin::load(&Engine::default(), &dir, &row, test_ctx(), logged().0)
            .expect_err("a declared, ungranted import must refuse");
        assert!(err.to_string().contains("clock.now"), "{err}");
    }

    #[test]
    fn handle_round_trips_and_log_service_runs() {
        let dir = fixture_dir();
        let (log, lines) = logged();
        let plugin = Plugin::load(
            &Engine::default(),
            &dir,
            &full_row(dir.clone()),
            test_ctx(),
            log,
        )
        .expect("a granted row loads");
        let reply = plugin
            .call_handle(0, json!({ "text": "hello" }).to_string().as_bytes())
            .expect("neon_handle answers");
        let v: Value = serde_json::from_str(&reply).expect("the reply is JSON");
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["content"], json!("echo: hello"));

        // Tool 1 calls the granted `log` service; the host sees the line.
        let reply = plugin
            .call_handle(1, json!({ "line": "hi from wasm" }).to_string().as_bytes())
            .expect("neon_handle answers");
        let v: Value = serde_json::from_str(&reply).expect("the reply is JSON");
        assert_eq!(v["ok"], json!(true));
        assert!(lines.lock().unwrap().iter().any(|l| l == "hi from wasm"));
    }

    #[test]
    fn clock_service_returns_a_number() {
        let dir = fixture_dir();
        let plugin = Plugin::load(
            &Engine::default(),
            &dir,
            &full_row(dir.clone()),
            test_ctx(),
            logged().0,
        )
        .expect("a granted row loads");
        let reply = plugin
            .call_handle(1, json!({ "line": "x" }).to_string().as_bytes())
            .expect("neon_handle answers");
        let v: Value = serde_json::from_str(&reply).expect("the reply is JSON");
        assert_eq!(v["ok"], json!(true));
        assert!(
            v["content"]
                .as_str()
                .unwrap_or("")
                .chars()
                .all(|c| c.is_ascii_digit()),
            "content: {}",
            v["content"]
        );
    }

    #[test]
    fn apply_registers_tools_and_unload_removes_them() {
        let registry = Registry::new();
        registry.add(Box::new(crate::tools::BashTool), PluginRow::Core);
        let mut host = PluginHost::new(test_ctx(), logged().0);
        host.queue(full_row(fixture_dir()));
        host.apply(&registry).expect("apply");
        let names: Vec<String> = registry.schemas().iter().map(|s| s.name.clone()).collect();
        assert!(names.contains(&"echo".to_string()));
        assert!(names.contains(&"bash".to_string()));

        host.queue_unload("echo");
        host.apply(&registry).expect("unload apply");
        let names: Vec<String> = registry.schemas().iter().map(|s| s.name.clone()).collect();
        assert!(!names.contains(&"echo".to_string()));
        assert!(names.contains(&"bash".to_string()));
    }

    #[test]
    fn reload_rereads_the_wasm_file() {
        let registry = Registry::new();
        let mut host = PluginHost::new(test_ctx(), logged().0);
        host.queue(full_row(fixture_dir()));
        host.apply(&registry).expect("first apply");
        host.queue_reload("echo").expect("reload queues");
        host.apply(&registry).expect("second apply");
        assert!(host.loaded_ids().contains(&"echo".to_string()));
        assert!(registry.schemas().iter().any(|s| s.name == "echo"));
    }
}
