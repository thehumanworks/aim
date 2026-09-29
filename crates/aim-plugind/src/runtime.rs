//! WebAssembly component runtime isolated in the plugin worker.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

pub use aim_plugin::{BoxFuture, PluginDelegate};
use aim_plugin::{PluginError, PluginManifest, PluginSource, SessionMetadata};
use aim_proto::error::{ErrorCode, ProtoError};
use aim_proto::ids::IdempotencyKey;
use aim_proto::tool::{ToolAnnotations, ToolInput, ToolResult, ToolSpec};
use serde_json::Value;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

/// Exact Wasmtime release used for compiled component cache identity.
pub const WASMTIME_VERSION: &str = "48.0.3";
const MAX_COMPONENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_PROJECT_COMPONENT_BYTES: usize = 2 * 1024 * 1024;
const MAX_ARGUMENT_BYTES: usize = 1024 * 1024;
const MAX_KV_VALUE_BYTES: usize = 1024 * 1024;
const CALL_FUEL: u64 = 20_000_000;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../aim-plugin/wit",
        world: "extension",
        imports: { default: async },
        exports: { default: async },
    });
}

use bindings::aim::plugin::{bus, host, kv, session, tools, types, ui};

#[derive(Clone)]
struct RuntimePlugin {
    manifest: PluginManifest,
    hash: String,
    component_hash: String,
    bytes: Arc<Vec<u8>>,
    grants: BTreeSet<String>,
    specs: Vec<ToolSpec>,
    kv_path: PathBuf,
}

/// Plugin tools composed into one agent session.
#[derive(Clone)]
pub struct PluginToolHost {
    plugins: Arc<Vec<RuntimePlugin>>,
    delegate: Arc<dyn PluginDelegate>,
    allowed_tools: Arc<HashSet<String>>,
    metadata: Arc<SessionMetadata>,
}

static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
static COMPONENTS: OnceLock<Mutex<HashMap<String, Component>>> = OnceLock::new();
static CALL_LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();

impl PluginToolHost {
    /// Parses manifests without initializing Wasmtime. Project plugins require exact-hash trust.
    /// The caller supplies a trust snapshot, so the worker never reads the trust registry.
    ///
    /// # Errors
    /// Returns an error if any admitted source has an invalid manifest or size.
    pub fn load(
        home: &Path,
        grants_by_hash: &BTreeMap<String, BTreeSet<String>>,
        sources: Vec<PluginSource>,
        delegate: Arc<dyn PluginDelegate>,
        allowed_tools: HashSet<String>,
    ) -> Result<Self, PluginError> {
        let mut plugins = Vec::new();
        for source in sources {
            let limit = if source.project { MAX_PROJECT_COMPONENT_BYTES } else { MAX_COMPONENT_BYTES };
            if source.component.is_empty() || source.component.len() > limit {
                return Err(PluginError::Invalid("component exceeds source size limit".into()));
            }
            let manifest = PluginManifest::parse(&source.manifest_text)?;
            let hash = source.hash();
            let component_hash = source.component_hash();
            let grant = grants_by_hash.get(&hash);
            if source.project && grant.is_none() {
                continue;
            }
            let grants: BTreeSet<String> = grant.map_or_else(BTreeSet::new, |g| g.intersection(&manifest.capabilities).cloned().collect());
            let specs = if grants.contains("tools.provide") {
                manifest
                    .tools
                    .iter()
                    .map(|tool| {
                        Ok(ToolSpec {
                            name: format!("plugin__{}__{}", manifest.name, tool.name),
                            description: tool.description.clone(),
                            input_schema: serde_json::from_str(&tool.input_schema).map_err(|e| PluginError::Invalid(e.to_string()))?,
                            input: ToolInput::Json,
                            annotations: ToolAnnotations::default(),
                        })
                    })
                    .collect::<Result<Vec<_>, PluginError>>()?
            } else {
                Vec::new()
            };
            let kv_path = home.join("plugin-kv").join(format!("{hash}.json"));
            plugins.push(RuntimePlugin { manifest, hash, component_hash, bytes: Arc::new(source.component), grants, specs, kv_path });
        }
        Ok(Self {
            plugins: Arc::new(plugins),
            delegate,
            allowed_tools: Arc::new(allowed_tools),
            metadata: Arc::new(SessionMetadata::default()),
        })
    }

    /// Adds a bounded read-only view of the owning session.
    #[must_use]
    pub fn with_session_metadata(mut self, metadata: SessionMetadata) -> Self {
        self.metadata = Arc::new(metadata);
        self
    }

    /// Returns all manifest-advertised namespaced tool specs.
    #[must_use]
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.plugins.iter().flat_map(|plugin| plugin.specs.iter().cloned()).collect()
    }

    /// Starts compilation on a background blocking worker after session creation. The first
    /// model request does not await compilation; a first call still compiles if warming lags.
    pub fn prewarm(&self) {
        let plugins = Arc::clone(&self.plugins);
        if plugins.iter().all(|plugin| plugin.specs.is_empty()) {
            return;
        }
        tokio::spawn(async move {
            for plugin in plugins.iter().filter(|plugin| !plugin.specs.is_empty()) {
                if compile_component(plugin).await.is_err() {
                    tracing::warn!(plugin = %plugin.manifest.name, "plugin background compilation failed");
                }
            }
        });
    }

    /// Calls a plugin tool, compiling and validating its component on first use.
    pub fn call(&self, name: String, arguments: Value, key: IdempotencyKey) -> BoxFuture<Result<ToolResult, ProtoError>> {
        let plugin = self.plugins.iter().find(|plugin| plugin.specs.iter().any(|spec| spec.name == name)).cloned();
        let Some(plugin) = plugin else {
            return Box::pin(async move { Err(ProtoError::new(ErrorCode::NotFound, format!("no plugin tool named `{name}`"))) });
        };
        let delegate = Arc::clone(&self.delegate);
        let allowed = Arc::clone(&self.allowed_tools);
        let metadata = Arc::clone(&self.metadata);
        Box::pin(async move {
            tokio::time::timeout(std::time::Duration::from_secs(30), async move {
                // Guest read/modify/write sequences in KV are serial for this component within the
                // daemon. Individual file updates remain locked for concurrent processes.
                let locks = CALL_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
                let call_lock = {
                    let mut registry = locks.lock().map_err(|_| runtime_error("plugin call locks poisoned"))?;
                    Arc::clone(registry.entry(plugin.hash.clone()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))))
                };
                let _guard = call_lock.lock().await;
                let (engine, component) = compile_component(&plugin).await.map_err(runtime_error)?;
                let prefix = format!("plugin__{}__", plugin.manifest.name);
                let raw_name =
                    name.strip_prefix(&prefix).ok_or_else(|| ProtoError::new(ErrorCode::InvalidParams, "plugin namespace mismatch"))?;
                let json = serde_json::to_string(&arguments).map_err(|e| ProtoError::new(ErrorCode::InvalidParams, e.to_string()))?;
                if json.len() > MAX_ARGUMENT_BYTES {
                    return Err(ProtoError::new(ErrorCode::InvalidParams, "plugin arguments exceed 1 MiB"));
                }
                let (mut store, guest) =
                    instantiate(&engine, &component, &plugin, delegate, allowed, metadata, key).await.map_err(runtime_error)?;
                let registration = guest
                    .aim_plugin_plugin()
                    .call_init(&mut store, &"{}".to_owned())
                    .await
                    .map_err(runtime_error)?
                    .map_err(|error| ProtoError::new(ErrorCode::Unavailable, format!("plugin init: {error:?}")))?;
                validate_registration(&plugin.manifest, &registration).map_err(runtime_error)?;
                let result = guest
                    .aim_plugin_plugin()
                    .call_call_tool(&mut store, raw_name, &name, &json)
                    .await
                    .map_err(runtime_error)?
                    .map_err(|error| ProtoError::new(ErrorCode::Unavailable, format!("plugin call: {error:?}")))?;
                serde_json::from_str::<ToolResult>(&result.content)
                    .map_err(|e| ProtoError::new(ErrorCode::Unavailable, format!("invalid plugin result: {e}")))
            })
            .await
            .map_err(|_| ProtoError::new(ErrorCode::Unavailable, "plugin call deadline exceeded"))?
        })
    }

    /// Delivers an event to admitted components under fresh fuel and a bounded deadline.
    /// Returned guest actions are discarded until the event/action protocol is composed by aim.
    pub fn on_event(
        &self,
        name: String,
        schema: u16,
        seq: u64,
        session: Option<String>,
        payload: Value,
    ) -> BoxFuture<Result<(), ProtoError>> {
        let plugins = Arc::clone(&self.plugins);
        let delegate = Arc::clone(&self.delegate);
        let allowed = Arc::clone(&self.allowed_tools);
        let metadata = Arc::clone(&self.metadata);
        Box::pin(async move {
            let json = serde_json::to_string(&payload).map_err(|error| ProtoError::new(ErrorCode::InvalidParams, error.to_string()))?;
            if name.len() > 256 || json.len() > MAX_ARGUMENT_BYTES || session.as_ref().is_some_and(|value| value.len() > 256) {
                return Err(ProtoError::new(ErrorCode::InvalidParams, "plugin event exceeds size limit"));
            }
            for plugin in plugins.iter() {
                let locks = CALL_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
                let call_lock = {
                    let mut registry = locks.lock().map_err(|_| runtime_error("plugin call locks poisoned"))?;
                    Arc::clone(registry.entry(plugin.hash.clone()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))))
                };
                let delegate = Arc::clone(&delegate);
                let allowed = Arc::clone(&allowed);
                let metadata = Arc::clone(&metadata);
                let name = name.clone();
                let session = session.clone();
                let json = json.clone();
                tokio::time::timeout(std::time::Duration::from_secs(30), async move {
                    let _guard = call_lock.lock().await;
                    let (engine, component) = compile_component(plugin).await.map_err(runtime_error)?;
                    let key = IdempotencyKey::new(format!("plugin-event:{seq}"));
                    let (mut store, guest) =
                        instantiate(&engine, &component, plugin, delegate, allowed, metadata, key).await.map_err(runtime_error)?;
                    let registration = guest
                        .aim_plugin_plugin()
                        .call_init(&mut store, &"{}".to_owned())
                        .await
                        .map_err(runtime_error)?
                        .map_err(|error| ProtoError::new(ErrorCode::Unavailable, format!("plugin init: {error:?}")))?;
                    validate_registration(&plugin.manifest, &registration).map_err(runtime_error)?;
                    let event = types::Event { name, schema, seq, session, payload: json };
                    let _actions = guest.aim_plugin_plugin().call_on_event(&mut store, &event).await.map_err(runtime_error)?;
                    Ok::<(), ProtoError>(())
                })
                .await
                .map_err(|_| ProtoError::new(ErrorCode::Unavailable, "plugin event deadline exceeded"))??;
            }
            Ok(())
        })
    }

    /// Calls the guest shutdown export for admitted components under a bounded deadline.
    pub fn shutdown(&self, reason: String) -> BoxFuture<Result<(), ProtoError>> {
        let plugins = Arc::clone(&self.plugins);
        let delegate = Arc::clone(&self.delegate);
        let allowed = Arc::clone(&self.allowed_tools);
        let metadata = Arc::clone(&self.metadata);
        Box::pin(async move {
            if reason.len() > 256 {
                return Err(ProtoError::new(ErrorCode::InvalidParams, "plugin shutdown reason exceeds size limit"));
            }
            for plugin in plugins.iter() {
                let locks = CALL_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
                let call_lock = {
                    let mut registry = locks.lock().map_err(|_| runtime_error("plugin call locks poisoned"))?;
                    Arc::clone(registry.entry(plugin.hash.clone()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))))
                };
                let delegate = Arc::clone(&delegate);
                let allowed = Arc::clone(&allowed);
                let metadata = Arc::clone(&metadata);
                let reason = reason.clone();
                tokio::time::timeout(std::time::Duration::from_secs(30), async move {
                    let _guard = call_lock.lock().await;
                    let (engine, component) = compile_component(plugin).await.map_err(runtime_error)?;
                    let key = IdempotencyKey::new("plugin-shutdown");
                    let (mut store, guest) =
                        instantiate(&engine, &component, plugin, delegate, allowed, metadata, key).await.map_err(runtime_error)?;
                    guest.aim_plugin_plugin().call_shutdown(&mut store, &reason).await.map_err(runtime_error)?;
                    Ok::<(), ProtoError>(())
                })
                .await
                .map_err(|_| ProtoError::new(ErrorCode::Unavailable, "plugin shutdown deadline exceeded"))??;
            }
            Ok(())
        })
    }
}

fn runtime_error(error: impl fmt::Display) -> ProtoError {
    ProtoError::new(ErrorCode::Unavailable, format!("plugin runtime: {error:#}"))
}

fn validate_registration(manifest: &PluginManifest, registration: &types::Registration) -> Result<(), PluginError> {
    if manifest.tools.len() != registration.tools.len() {
        return Err(PluginError::Invalid("guest registration differs from manifest".into()));
    }
    for (declared, actual) in manifest.tools.iter().zip(&registration.tools) {
        let declared_schema: Value = serde_json::from_str(&declared.input_schema).map_err(|e| PluginError::Invalid(e.to_string()))?;
        let actual_schema: Value = serde_json::from_str(&actual.input_schema).map_err(|e| PluginError::Invalid(e.to_string()))?;
        if declared.name != actual.name
            || declared.description != actual.description
            || declared_schema != actual_schema
            || declared.sensitive != actual.sensitive
        {
            return Err(PluginError::Invalid("guest tool registration differs from manifest".into()));
        }
    }
    Ok(())
}

fn create_engine() -> Result<Engine, String> {
    let mut config = Config::new();
    config.wasm_component_model(true).consume_fuel(true);
    config.allocation_strategy(InstanceAllocationStrategy::Pooling(PoolingAllocationConfig::default()));
    Engine::new(&config).map_err(|error| error.to_string())
}

fn compiled(engine: &Engine, hash: &str, source: &[u8]) -> Result<Component, PluginError> {
    let key = cache_key(hash, CALL_FUEL, 64 * 1024 * 1024);
    let cache = COMPONENTS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(component) = cache.lock().map_err(|_| PluginError::Runtime("component cache poisoned".into()))?.get(&key) {
        return Ok(component.clone());
    }
    let component = Component::new(engine, source).map_err(|e| PluginError::Runtime(e.to_string()))?;
    cache.lock().map_err(|_| PluginError::Runtime("component cache poisoned".into()))?.insert(key, component.clone());
    Ok(component)
}

async fn compile_component(plugin: &RuntimePlugin) -> Result<(Engine, Component), PluginError> {
    let hash = plugin.component_hash.clone();
    let bytes = Arc::clone(&plugin.bytes);
    tokio::task::spawn_blocking(move || {
        let engine = ENGINE.get_or_init(create_engine).as_ref().map_err(|message| PluginError::Runtime(message.clone()))?.clone();
        let component = compiled(&engine, &hash, &bytes)?;
        Ok((engine, component))
    })
    .await
    .map_err(|error| PluginError::Runtime(error.to_string()))?
}

fn cache_key(hash: &str, fuel: u64, memory_limit: usize) -> String {
    format!("{hash}:{WASMTIME_VERSION}:pooling:async:fuel{fuel}:memory{memory_limit}")
}

struct HostState {
    grants: BTreeSet<String>,
    delegate: Arc<dyn PluginDelegate>,
    allowed_tools: Arc<HashSet<String>>,
    metadata: Arc<SessionMetadata>,
    kv_path: PathBuf,
    limits: StoreLimits,
    wasi: WasiCtx,
    table: ResourceTable,
    outer_key: IdempotencyKey,
    nested_ordinal: u64,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

async fn instantiate(
    engine: &Engine,
    component: &Component,
    plugin: &RuntimePlugin,
    delegate: Arc<dyn PluginDelegate>,
    allowed_tools: Arc<HashSet<String>>,
    metadata: Arc<SessionMetadata>,
    outer_key: IdempotencyKey,
) -> Result<(Store<HostState>, bindings::Extension), wasmtime::Error> {
    let state = HostState {
        grants: plugin.grants.clone(),
        delegate,
        allowed_tools,
        metadata,
        kv_path: plugin.kv_path.clone(),
        limits: StoreLimitsBuilder::new().memory_size(64 * 1024 * 1024).build(),
        wasi: WasiCtxBuilder::new().build(),
        table: ResourceTable::new(),
        outer_key,
        nested_ordinal: 0,
    };
    let mut store = Store::new(engine, state);
    store.limiter(|state| &mut state.limits);
    store.set_fuel(CALL_FUEL)?;
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    bindings::Extension::add_to_linker::<_, HasSelf<_>>(&mut linker, |state: &mut HostState| state)?;
    let guest = bindings::Extension::instantiate_async(&mut store, component, &linker).await?;
    Ok((store, guest))
}

fn denied() -> types::Error {
    types::Error::Denied("capability not granted".into())
}

fn with_kv<T>(path: PathBuf, write: bool, update: impl FnOnce(&mut BTreeMap<String, Vec<u8>>) -> T) -> Result<T, types::Error> {
    let parent = path.parent().ok_or_else(|| types::Error::Failed("kv path has no parent".into()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(|e| types::Error::Failed(e.to_string()))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(parent).map_err(|e| types::Error::Failed(e.to_string()))?;
    let lock_path = path.with_extension("lock");
    #[cfg(unix)]
    let lock = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(&lock_path)
    }
    .map_err(|e| types::Error::Failed(e.to_string()))?;
    #[cfg(not(unix))]
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| types::Error::Failed(e.to_string()))?;
    lock.lock().map_err(|e| types::Error::Failed(e.to_string()))?;
    let mut values: BTreeMap<String, Vec<u8>> = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| types::Error::Failed(e.to_string()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => return Err(types::Error::Failed(e.to_string())),
    };
    let result = update(&mut values);
    if write {
        let bytes = serde_json::to_vec(&values).map_err(|e| types::Error::Failed(e.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| types::Error::Failed(e.to_string()))?;
            std::io::Write::write_all(&mut file, &bytes).map_err(|e| types::Error::Failed(e.to_string()))?;
            file.sync_all().map_err(|e| types::Error::Failed(e.to_string()))?;
        }
        #[cfg(not(unix))]
        std::fs::write(&tmp, &bytes).map_err(|e| types::Error::Failed(e.to_string()))?;
        std::fs::rename(tmp, path).map_err(|e| types::Error::Failed(e.to_string()))?;
    }
    Ok(result)
}

fn scope_matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        Some((before, after)) => name.starts_with(before) && name.ends_with(after),
        None => pattern == name,
    }
}

impl types::Host for HostState {}

#[expect(clippy::unused_async_trait_impl, reason = "WIT-generated imports require asynchronous signatures")]
impl host::Host for HostState {
    async fn log(&mut self, _level: types::Level, _msg: String) {}
    async fn capability(&mut self, key: String) -> String {
        if self.grants.contains(&key) { "granted".into() } else { "denied".into() }
    }
    async fn now_ms(&mut self) -> u64 {
        if !self.grants.contains("clock.wall") {
            return 0;
        }
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
    }
}

impl kv::Host for HostState {
    async fn get(&mut self, key: String) -> Result<Option<Vec<u8>>, types::Error> {
        if !self.grants.contains("kv") {
            return Err(denied());
        }
        let path = self.kv_path.clone();
        tokio::task::spawn_blocking(move || with_kv(path, false, |kv| kv.get(&key).cloned()))
            .await
            .map_err(|e| types::Error::Failed(e.to_string()))?
    }
    async fn set(&mut self, key: String, value: Vec<u8>) -> Result<(), types::Error> {
        if !self.grants.contains("kv") {
            return Err(denied());
        }
        if key.len() > 256 || value.len() > MAX_KV_VALUE_BYTES {
            return Err(types::Error::Invalid("kv size limit".into()));
        }
        let path = self.kv_path.clone();
        tokio::task::spawn_blocking(move || {
            with_kv(path, true, |kv| {
                kv.insert(key, value);
            })
        })
        .await
        .map_err(|e| types::Error::Failed(e.to_string()))?
    }
    async fn delete(&mut self, key: String) -> Result<(), types::Error> {
        if !self.grants.contains("kv") {
            return Err(denied());
        }
        let path = self.kv_path.clone();
        tokio::task::spawn_blocking(move || {
            with_kv(path, true, |kv| {
                kv.remove(&key);
            })
        })
        .await
        .map_err(|e| types::Error::Failed(e.to_string()))?
    }
    async fn list(&mut self, prefix: String) -> Result<Vec<String>, types::Error> {
        if !self.grants.contains("kv") {
            return Err(denied());
        }
        let path = self.kv_path.clone();
        tokio::task::spawn_blocking(move || {
            with_kv(path, false, |kv| kv.keys().filter(|key| key.starts_with(&prefix)).take(1024).cloned().collect())
        })
        .await
        .map_err(|e| types::Error::Failed(e.to_string()))?
    }
}

impl tools::Host for HostState {
    async fn call(&mut self, name: String, args: String) -> Result<types::ToolResult, types::Error> {
        if !self.allowed_tools.contains(&name)
            || !self.grants.iter().filter_map(|grant| grant.strip_prefix("tools.call:")).any(|pattern| scope_matches(pattern, &name))
        {
            return Err(denied());
        }
        if args.len() > MAX_ARGUMENT_BYTES {
            return Err(types::Error::Invalid("tool arguments too large".into()));
        }
        let arguments: Value = serde_json::from_str(&args).map_err(|error| types::Error::Invalid(error.to_string()))?;
        let key = IdempotencyKey::new(format!("{}:plugin:{}", self.outer_key, self.nested_ordinal));
        self.nested_ordinal = self.nested_ordinal.saturating_add(1);
        let result = self.delegate.call(name, arguments, key).await.map_err(|error| types::Error::Failed(error.to_string()))?;
        let content = serde_json::to_string(&result).map_err(|error| types::Error::Failed(error.to_string()))?;
        Ok(types::ToolResult { content, is_error: result.is_error, details: None })
    }
}

#[expect(clippy::unused_async_trait_impl, reason = "WIT-generated imports require asynchronous signatures")]
impl session::Host for HostState {
    async fn query(&mut self, query: String) -> Result<String, types::Error> {
        if !self.grants.contains("session.read") {
            return Err(denied());
        }
        let parsed: Value = serde_json::from_str(&query).map_err(|e| types::Error::Invalid(e.to_string()))?;
        if query.len() > 1024 || parsed != serde_json::json!({}) {
            return Err(types::Error::Invalid("only the full read-only metadata query is supported".into()));
        }
        let result = serde_json::to_string(&*self.metadata).map_err(|e| types::Error::Failed(e.to_string()))?;
        if result.len() > 8192 {
            return Err(types::Error::Invalid("session metadata exceeds 8 KiB".into()));
        }
        Ok(result)
    }
}

#[expect(clippy::unused_async_trait_impl, reason = "WIT-generated deny stubs require asynchronous signatures")]
impl ui::Host for HostState {
    async fn apply(&mut self, _messages: Vec<String>) -> Result<(), types::Error> {
        Err(denied())
    }
    async fn notify(&mut self, _level: String, _text: String) {}
    async fn ask(&mut self, _dialog: String) -> Result<Option<String>, types::Error> {
        Err(denied())
    }
}

#[expect(clippy::unused_async_trait_impl, reason = "WIT-generated deny stub requires an asynchronous signature")]
impl bus::Host for HostState {
    async fn publish(&mut self, _topic: String, _message: String) -> Result<(), types::Error> {
        Err(denied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_matching_never_widens_other_names() {
        assert!(scope_matches("read*", "read"));
        assert!(scope_matches("read*", "read_file"));
        assert!(!scope_matches("read*", "write_file"));
    }

    #[test]
    fn changing_engine_limits_invalidates_compiled_cache_key() {
        let source = "a".repeat(64);
        let current = cache_key(&source, CALL_FUEL, 64 * 1024 * 1024);
        assert_ne!(current, cache_key(&source, CALL_FUEL + 1, 64 * 1024 * 1024));
        assert_ne!(current, cache_key(&source, CALL_FUEL, 32 * 1024 * 1024));
        assert_ne!(current, cache_key(&"b".repeat(64), CALL_FUEL, 64 * 1024 * 1024));
    }
}
