//! The interpreter half of the host: loading, sandboxing and calling one
//! WebAssembly guest.
//!
//! Nothing in this module ever runs in the CriKey UI process. It runs in the
//! `crikey-wasm-host` executable, which the ordinary native supervisor spawns
//! and supervises exactly like any other native worker (README invariant 1).
//! That is what makes running third-party WebAssembly safe at all: the
//! interpreter's own soundness is a second line of defence, not the first.
//!
//! # Sandbox posture
//!
//! * The guest gets no WASI. There is no filesystem, network, process, random
//!   or environment interface unless the manifest granted it, and a granted
//!   interface is one narrow host function, not a POSIX surface.
//! * An ungranted capability is *absent from the linker*. A module importing
//!   it fails to load with a refusal naming the import and the permission it
//!   needed. There is no denied-return-value path to probe.
//! * Linear memory is bounded by a configured ceiling through wasmi's
//!   [`StoreLimits`], and growth past it traps.
//! * Every call is metered with fuel derived from the manifest's hard
//!   suggestion deadline, so a spinning module is interrupted rather than
//!   waited on. Fuel counts instructions, so the wall clock is additionally
//!   guaranteed by [`crate::watchdog`].
//!
//! # Containment
//!
//! A trap — guest `unreachable`, an out-of-bounds access, memory growth past
//! the ceiling, or fuel exhaustion — fails that one request and poisons the
//! instance. The next call re-instantiates from the already-validated module,
//! because guest heap invariants after a trap are the guest's business and it
//! did not get to finish. The process survives, so the supervisor sees a
//! plugin error rather than a crashed worker, and other plugins — which are
//! other processes entirely — are untouched.

use std::path::{Component, Path, PathBuf};

use crikey_core::{Item, PluginId};
use crikey_plugin_sdk::LogLevel;
use wasmi::errors::LinkerError;
use wasmi::{
    Caller, Config, Engine, Error as WasmiError, Extern, Linker, Memory, Module, Store, StoreLimits,
    StoreLimitsBuilder, TrapCode, TypedFunc,
};

use crate::abi::{self, AbiError, ExecuteRequest, Limits, SuggestRequest};
use crate::config::{Grants, HostConfig};

/// Import module name every host function lives under.
pub const IMPORT_MODULE: &str = "crikey";
/// Always-available bounded diagnostic sink.
pub const IMPORT_LOG: &str = "log";
/// Confined package-directory read; requires a filesystem permission.
pub const IMPORT_READ_FILE: &str = "read_file";
/// Environment variable read; requires `permissions.environment`.
pub const IMPORT_ENV_GET: &str = "env_get";
/// Name the guest must export its linear memory under.
pub const EXPORT_MEMORY: &str = "memory";

/// Required export reporting the ABI revision the module was built against.
pub const EXPORT_ABI_VERSION: &str = "crikey_abi_version";
/// Required export allocating guest memory for a host-supplied blob.
pub const EXPORT_ALLOC: &str = "crikey_alloc";
/// Optional export answering a suggestion request.
pub const EXPORT_SUGGEST: &str = "crikey_suggest";
/// Optional export producing the plugin's catalog.
pub const EXPORT_CATALOG: &str = "crikey_catalog";
/// Optional export running one action.
pub const EXPORT_EXECUTE: &str = "crikey_execute";

/// Maximum size of a `.wasm` file this host will read.
pub const MAX_MODULE_BYTES: usize = 64 * 1024 * 1024;
/// Maximum log records retained from one guest call.
const MAX_LOG_RECORDS: usize = 64;
/// Maximum length of one guest log message, and of any other short string a
/// host function accepts from the guest.
const MAX_GUEST_STRING_BYTES: usize = 4 * 1024;
/// Maximum bytes a granted `crikey::read_file` will serve for one call.
const MAX_READ_FILE_BYTES: usize = 1024 * 1024;

/// The four bytes every WebAssembly binary starts with.
const WASM_MAGIC: [u8; 4] = [0x00, 0x61, 0x73, 0x6D];

/// Host function result: malformed pointer or length.
pub const HOST_ERR_INVALID: i32 = -1;
/// Host function result: the named thing does not exist.
pub const HOST_ERR_NOT_FOUND: i32 = -2;
/// Host function result: the guest output buffer is too small.
pub const HOST_ERR_TOO_LARGE: i32 = -3;

/// Why a guest could not be loaded or could not answer.
#[derive(Debug)]
pub enum GuestError {
    /// The module file could not be read, or was above [`MAX_MODULE_BYTES`].
    Unreadable { path: PathBuf, reason: String },
    /// The file did not begin with the WebAssembly magic. Text modules are
    /// deliberately not accepted; a runtime takes one input encoding.
    NotWebAssembly { path: PathBuf },
    /// wasmi refused to validate or instantiate the module.
    Invalid(WasmiError),
    /// A host import could not be defined.
    LinkerRefused(LinkerError),
    /// The module imports something outside the `crikey` namespace, or a name
    /// no CriKey host provides.
    ForeignImport { module: String, name: String },
    /// The module imports a capability the manifest did not grant.
    DeniedCapability { name: String, permission: &'static str },
    /// A required export is missing or has the wrong signature.
    MissingExport { name: &'static str, reason: String },
    /// The module reports an ABI revision this host does not implement.
    AbiMismatch { found: i32, expected: i32 },
    /// The module exports none of the three entry points, so it can never
    /// answer anything.
    NoEntryPoints,
    /// The guest does not implement the entry point this call needs.
    Unsupported(&'static str),
    /// The guest exhausted its fuel budget: a spinning or runaway call.
    DeadlineExceeded { fuel: u64 },
    /// The guest trapped.
    Trap(WasmiError),
    /// The guest returned a pointer or length outside its own memory.
    BadResponseRange {
        pointer: u32,
        length: u32,
        memory: usize,
    },
    /// The guest's allocator refused or returned an unusable pointer.
    AllocationFailed { requested: usize, returned: i32 },
    /// The guest's response blob was malformed.
    Malformed(AbiError),
    /// The guest's `crikey_execute` reported a plugin-side failure.
    ExecuteFailed { code: i32 },
}

impl std::fmt::Display for GuestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable { path, reason } => {
                write!(formatter, "cannot read wasm module {}: {reason}", path.display())
            }
            Self::NotWebAssembly { path } => write!(
                formatter,
                "{} is not a WebAssembly binary: it does not begin with the wasm magic",
                path.display()
            ),
            Self::Invalid(error) => write!(formatter, "wasm module rejected: {error}"),
            Self::LinkerRefused(error) => {
                write!(formatter, "wasm host imports could not be defined: {error}")
            }
            Self::ForeignImport { module, name } => write!(
                formatter,
                "wasm module imports {module}::{name}, which no CriKey host provides"
            ),
            Self::DeniedCapability { name, permission } => write!(
                formatter,
                "wasm module imports {IMPORT_MODULE}::{name}, which needs the {permission} \
                 permission the manifest does not grant"
            ),
            Self::MissingExport { name, reason } => {
                write!(formatter, "wasm module export {name} is unusable: {reason}")
            }
            Self::AbiMismatch { found, expected } => write!(
                formatter,
                "wasm module targets guest ABI {found}, this host implements {expected}"
            ),
            Self::NoEntryPoints => write!(
                formatter,
                "wasm module exports none of {EXPORT_SUGGEST}, {EXPORT_CATALOG} or \
                 {EXPORT_EXECUTE}, so it can never answer a request"
            ),
            Self::Unsupported(name) => write!(formatter, "wasm module does not export {name}"),
            Self::DeadlineExceeded { fuel } => write!(
                formatter,
                "wasm guest exhausted its {fuel}-unit fuel budget and was interrupted"
            ),
            Self::Trap(error) => write!(formatter, "wasm guest trapped: {error}"),
            Self::BadResponseRange {
                pointer,
                length,
                memory,
            } => write!(
                formatter,
                "wasm guest returned range {pointer}..+{length} outside its {memory}-byte memory"
            ),
            Self::AllocationFailed { requested, returned } => write!(
                formatter,
                "wasm guest allocator returned {returned} for a {requested}-byte request"
            ),
            Self::Malformed(error) => write!(formatter, "wasm guest response rejected: {error}"),
            Self::ExecuteFailed { code } => {
                write!(formatter, "wasm guest action failed with code {code}")
            }
        }
    }
}

impl std::error::Error for GuestError {}

impl GuestError {
    /// Whether this failure is the deadline being enforced rather than the
    /// plugin misbehaving. The two are reported differently upstream: one is a
    /// slow plugin, the other is a broken one.
    pub fn is_deadline(&self) -> bool {
        matches!(self, Self::DeadlineExceeded { .. })
    }

    /// Whether the guest stopped mid-execution, leaving its heap in a state
    /// only the guest could have reasoned about. Such an instance is discarded
    /// rather than reused.
    fn poisons_instance(&self) -> bool {
        matches!(
            self,
            Self::Invalid(_)
                | Self::DeadlineExceeded { .. }
                | Self::Trap(_)
                | Self::BadResponseRange { .. }
                | Self::AllocationFailed { .. }
        )
    }
}

/// One diagnostic record a guest emitted during a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestLog {
    /// Severity the guest asked for; an unrecognised level becomes `Info`.
    pub level: LogLevel,
    /// Message text, bounded by [`MAX_GUEST_STRING_BYTES`].
    pub message: String,
}

/// Store state shared with the host functions.
#[derive(Debug)]
struct GuestState {
    limits: StoreLimits,
    logs: Vec<GuestLog>,
    dropped_logs: u32,
    package_root: PathBuf,
}

fn level_from_tag(tag: i32) -> LogLevel {
    match tag {
        0 => LogLevel::Error,
        1 => LogLevel::Warn,
        3 => LogLevel::Debug,
        4 => LogLevel::Trace,
        _ => LogLevel::Info,
    }
}

/// Resolves a guest-supplied relative path inside `root`, or refuses.
///
/// The same discipline `plugin_icons` applies to package-relative icon paths:
/// only normal components, no absolute paths, no parent traversal, and a
/// canonicalisation check so a symlink inside the package cannot point out of
/// it. A guest cannot name a file outside the package directory even when the
/// manifest granted filesystem access.
fn confined_path(root: &Path, requested: &str) -> Option<PathBuf> {
    let relative = Path::new(requested);
    if relative.as_os_str().is_empty() || relative.is_absolute() {
        return None;
    }
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    let canonical_root = root.canonicalize().ok()?;
    let canonical = root.join(relative).canonicalize().ok()?;
    canonical.starts_with(&canonical_root).then_some(canonical)
}

/// Reads at most `cap` bytes, refusing rather than truncating.
fn read_capped(path: &Path, cap: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > cap as u64 {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    // One byte past the cap: the metadata length is a hint taken before the
    // read, and a file that grew in between must still be refused.
    file.by_ref().take(cap as u64 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= cap).then_some(bytes)
}

/// Copies `length` bytes out of guest memory, refusing an out-of-range window.
fn slice_from_memory(
    memory: &Memory,
    caller: &Caller<'_, GuestState>,
    pointer: i32,
    length: i32,
    cap: usize,
) -> Option<Vec<u8>> {
    let pointer = usize::try_from(pointer).ok()?;
    let length = usize::try_from(length).ok()?;
    if length > cap {
        return None;
    }
    let mut buffer = vec![0u8; length];
    memory.read(caller, pointer, &mut buffer).ok()?;
    Some(buffer)
}

/// Per-call values a [`Live`] operation needs without borrowing [`Guest`].
#[derive(Debug, Clone)]
struct CallContext {
    fuel: u64,
    limits: Limits,
    plugin: PluginId,
}

/// A live instance. Recreated after any trap.
#[derive(Debug)]
struct Live {
    store: Store<GuestState>,
    memory: Memory,
    alloc: TypedFunc<i32, i32>,
    suggest: Option<TypedFunc<(i32, i32), i64>>,
    catalog: Option<TypedFunc<(), i64>>,
    execute: Option<TypedFunc<(i32, i32), i32>>,
}

impl Live {
    /// Asks the guest allocator for space and copies a request in.
    fn write_blob(&mut self, blob: &[u8], fuel: u64) -> Result<i32, GuestError> {
        let length = i32::try_from(blob.len()).map_err(|_| GuestError::AllocationFailed {
            requested: blob.len(),
            returned: 0,
        })?;
        self.store.set_fuel(fuel).map_err(GuestError::Invalid)?;
        let pointer = self
            .alloc
            .call(&mut self.store, length)
            .map_err(|error| classify(error, fuel))?;
        if pointer <= 0 {
            return Err(GuestError::AllocationFailed {
                requested: blob.len(),
                returned: pointer,
            });
        }
        let available = self.memory.data_size(&self.store);
        self.memory
            .write(&mut self.store, pointer as usize, blob)
            .map_err(|_| GuestError::BadResponseRange {
                pointer: pointer as u32,
                length: blob.len() as u32,
                memory: available,
            })?;
        Ok(pointer)
    }

    /// Unpacks a `(pointer << 32 | length)` return value and copies the blob
    /// out of guest memory.
    fn response_bytes(&self, packed: i64, limits: Limits) -> Result<Vec<u8>, GuestError> {
        let packed = packed as u64;
        if packed == 0 {
            // A guest with nothing to say may return zero rather than encode
            // an empty batch. Synthesising the empty batch here keeps the one
            // decode path in charge of producing items.
            return Ok(abi::encode_item_batch(&[]));
        }
        let pointer = (packed >> 32) as u32;
        let length = packed as u32;
        let length_usize = length as usize;
        if length_usize > limits.max_blob_bytes {
            return Err(GuestError::Malformed(AbiError::TooLarge {
                field: "item-batch",
                len: length_usize,
                limit: limits.max_blob_bytes,
            }));
        }
        let available = self.memory.data_size(&self.store);
        let end = (pointer as usize).checked_add(length_usize);
        if end.is_none_or(|end| end > available) {
            return Err(GuestError::BadResponseRange {
                pointer,
                length,
                memory: available,
            });
        }
        let mut bytes = vec![0u8; length_usize];
        self.memory
            .read(&self.store, pointer as usize, &mut bytes)
            .map_err(|_| GuestError::BadResponseRange {
                pointer,
                length,
                memory: available,
            })?;
        Ok(bytes)
    }

    /// Runs one entry point that takes a blob and returns an item batch.
    fn items_from(
        &mut self,
        entry: TypedFunc<(i32, i32), i64>,
        blob: &[u8],
        context: &CallContext,
    ) -> Result<Vec<Item>, GuestError> {
        let pointer = self.write_blob(blob, context.fuel)?;
        self.store.set_fuel(context.fuel).map_err(GuestError::Invalid)?;
        let packed = entry
            .call(&mut self.store, (pointer, blob.len() as i32))
            .map_err(|error| classify(error, context.fuel))?;
        let bytes = self.response_bytes(packed, context.limits)?;
        abi::decode_item_batch(&bytes, &context.plugin, context.limits).map_err(GuestError::Malformed)
    }
}

/// One loaded WebAssembly plugin.
#[derive(Debug)]
pub struct Guest {
    engine: Engine,
    module: Module,
    linker: Linker<GuestState>,
    config: HostConfig,
    plugin: PluginId,
    live: Option<Live>,
    has_suggest: bool,
    has_catalog: bool,
    has_execute: bool,
}

impl Guest {
    /// Validates, links and instantiates the configured module.
    ///
    /// Every refusal here happens before the plugin can answer anything, which
    /// is the point: a module whose imports exceed its grants is unavailable,
    /// not silently degraded.
    pub fn load(config: HostConfig, plugin: PluginId) -> Result<Self, GuestError> {
        let bytes = read_capped(&config.module, MAX_MODULE_BYTES).ok_or_else(|| GuestError::Unreadable {
            path: config.module.clone(),
            reason: format!("absent, not a file, or above the {MAX_MODULE_BYTES} byte ceiling"),
        })?;
        if bytes.len() < WASM_MAGIC.len() || bytes[..WASM_MAGIC.len()] != WASM_MAGIC {
            return Err(GuestError::NotWebAssembly {
                path: config.module.clone(),
            });
        }

        let mut engine_config = Config::default();
        engine_config.consume_fuel(true);
        let engine = Engine::new(&engine_config);
        let module = Module::new(&engine, &bytes[..]).map_err(GuestError::Invalid)?;

        audit_imports(&module, config.grants)?;
        let linker = build_linker(&engine, config.grants)?;

        let mut guest = Self {
            engine,
            module,
            linker,
            config,
            plugin,
            live: None,
            has_suggest: false,
            has_catalog: false,
            has_execute: false,
        };
        // Instantiate eagerly so a module that cannot start is a load failure
        // with a precise reason rather than a mystery on the first keystroke.
        guest.instantiate()?;
        Ok(guest)
    }

    /// The identity every decoded item is attributed to.
    pub fn plugin(&self) -> &PluginId {
        &self.plugin
    }

    /// Configuration this guest was loaded with.
    pub fn config(&self) -> &HostConfig {
        &self.config
    }

    /// Whether the module can answer suggestion requests.
    pub fn answers_suggestions(&self) -> bool {
        self.has_suggest
    }

    /// Whether the module can build a catalog.
    pub fn builds_catalog(&self) -> bool {
        self.has_catalog
    }

    /// Whether the module can execute actions.
    pub fn executes_actions(&self) -> bool {
        self.has_execute
    }

    /// Drains the diagnostics the guest emitted since the last drain.
    pub fn take_logs(&mut self) -> Vec<GuestLog> {
        let Some(live) = self.live.as_mut() else {
            return Vec::new();
        };
        let state = live.store.data_mut();
        let dropped = std::mem::take(&mut state.dropped_logs);
        let mut logs = std::mem::take(&mut state.logs);
        if dropped != 0 {
            logs.push(GuestLog {
                level: LogLevel::Warn,
                message: format!("{dropped} further guest log records were dropped"),
            });
        }
        logs
    }

    fn instantiate(&mut self) -> Result<(), GuestError> {
        let fuel = self.config.fuel_per_call();
        let limits = StoreLimitsBuilder::new()
            .memory_size(self.config.memory_bytes)
            .memories(1)
            .instances(1)
            .trap_on_grow_failure(true)
            .build();
        let state = GuestState {
            limits,
            logs: Vec::new(),
            dropped_logs: 0,
            package_root: self.config.package_root.clone(),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        // Instantiation runs the module's `start` function and its data
        // segment initialisation, both of which are guest code, so they are
        // metered like any other call.
        store.set_fuel(fuel).map_err(GuestError::Invalid)?;

        let instance = self
            .linker
            .instantiate_and_start(&mut store, &self.module)
            .map_err(GuestError::Invalid)?;

        let memory = instance
            .get_memory(&store, EXPORT_MEMORY)
            .ok_or_else(|| GuestError::MissingExport {
                name: EXPORT_MEMORY,
                reason: "the module must export its linear memory as `memory`".to_owned(),
            })?;

        let version: TypedFunc<(), i32> =
            instance
                .get_typed_func(&store, EXPORT_ABI_VERSION)
                .map_err(|error| GuestError::MissingExport {
                    name: EXPORT_ABI_VERSION,
                    reason: error.to_string(),
                })?;
        store.set_fuel(fuel).map_err(GuestError::Invalid)?;
        let found = version
            .call(&mut store, ())
            .map_err(|error| classify(error, fuel))?;
        if found != abi::ABI_VERSION {
            return Err(GuestError::AbiMismatch {
                found,
                expected: abi::ABI_VERSION,
            });
        }

        let alloc: TypedFunc<i32, i32> =
            instance
                .get_typed_func(&store, EXPORT_ALLOC)
                .map_err(|error| GuestError::MissingExport {
                    name: EXPORT_ALLOC,
                    reason: error.to_string(),
                })?;
        let suggest: Option<TypedFunc<(i32, i32), i64>> =
            instance.get_typed_func(&store, EXPORT_SUGGEST).ok();
        let catalog: Option<TypedFunc<(), i64>> = instance.get_typed_func(&store, EXPORT_CATALOG).ok();
        let execute: Option<TypedFunc<(i32, i32), i32>> =
            instance.get_typed_func(&store, EXPORT_EXECUTE).ok();
        if suggest.is_none() && catalog.is_none() && execute.is_none() {
            return Err(GuestError::NoEntryPoints);
        }

        self.has_suggest = suggest.is_some();
        self.has_catalog = catalog.is_some();
        self.has_execute = execute.is_some();
        self.live = Some(Live {
            store,
            memory,
            alloc,
            suggest,
            catalog,
            execute,
        });
        Ok(())
    }

    /// Runs `operation` against a live instance, rebuilding the instance first
    /// if a previous call poisoned it and discarding it afterwards if this one
    /// did.
    fn with_live<R>(
        &mut self,
        operation: impl FnOnce(&mut Live, &CallContext) -> Result<R, GuestError>,
    ) -> Result<R, GuestError> {
        let context = CallContext {
            fuel: self.config.fuel_per_call(),
            limits: self.config.limits,
            plugin: self.plugin.clone(),
        };
        if self.live.is_none() {
            self.instantiate()?;
        }
        let live = self
            .live
            .as_mut()
            .expect("instantiate either populates the instance or returns an error");
        let result = operation(live, &context);
        if result.as_ref().err().is_some_and(GuestError::poisons_instance) {
            self.live = None;
        }
        result
    }

    /// Answers one suggestion request.
    pub fn suggest(&mut self, request: &SuggestRequest) -> Result<Vec<Item>, GuestError> {
        let blob = request.encode();
        self.with_live(|live, context| {
            let entry = live.suggest.ok_or(GuestError::Unsupported(EXPORT_SUGGEST))?;
            live.items_from(entry, &blob, context)
        })
    }

    /// Builds the plugin's catalog.
    pub fn catalog(&mut self) -> Result<Vec<Item>, GuestError> {
        self.with_live(|live, context| {
            let entry = live.catalog.ok_or(GuestError::Unsupported(EXPORT_CATALOG))?;
            live.store.set_fuel(context.fuel).map_err(GuestError::Invalid)?;
            let packed = entry
                .call(&mut live.store, ())
                .map_err(|error| classify(error, context.fuel))?;
            let bytes = live.response_bytes(packed, context.limits)?;
            abi::decode_item_batch(&bytes, &context.plugin, context.limits).map_err(GuestError::Malformed)
        })
    }

    /// Runs one action inside the guest.
    pub fn execute(&mut self, request: &ExecuteRequest) -> Result<(), GuestError> {
        let blob = request.encode();
        self.with_live(|live, context| {
            let entry = live.execute.ok_or(GuestError::Unsupported(EXPORT_EXECUTE))?;
            let pointer = live.write_blob(&blob, context.fuel)?;
            live.store.set_fuel(context.fuel).map_err(GuestError::Invalid)?;
            match entry.call(&mut live.store, (pointer, blob.len() as i32)) {
                Ok(0) => Ok(()),
                Ok(code) => Err(GuestError::ExecuteFailed { code }),
                Err(error) => Err(classify(error, context.fuel)),
            }
        })
    }
}

/// Maps a wasmi call failure onto the host's two very different stories:
/// "your plugin ran out of time" and "your plugin is broken".
fn classify(error: WasmiError, fuel: u64) -> GuestError {
    if error.as_trap_code() == Some(TrapCode::OutOfFuel) {
        GuestError::DeadlineExceeded { fuel }
    } else {
        GuestError::Trap(error)
    }
}

/// Refuses a module whose imports exceed what the manifest granted.
///
/// The linker alone would already refuse an undefined import, but its message
/// is about linking. A plugin author and an operator both need to be told that
/// the *permission* is the missing thing, and which one.
pub fn audit_imports(module: &Module, grants: Grants) -> Result<(), GuestError> {
    for import in module.imports() {
        let (namespace, name) = (import.module(), import.name());
        if namespace != IMPORT_MODULE {
            return Err(GuestError::ForeignImport {
                module: namespace.to_owned(),
                name: name.to_owned(),
            });
        }
        match name {
            IMPORT_LOG => {}
            IMPORT_READ_FILE if grants.filesystem_read => {}
            IMPORT_READ_FILE => {
                return Err(GuestError::DeniedCapability {
                    name: name.to_owned(),
                    permission: "filesystem",
                })
            }
            IMPORT_ENV_GET if grants.environment => {}
            IMPORT_ENV_GET => {
                return Err(GuestError::DeniedCapability {
                    name: name.to_owned(),
                    permission: "environment",
                })
            }
            other => {
                return Err(GuestError::ForeignImport {
                    module: namespace.to_owned(),
                    name: other.to_owned(),
                })
            }
        }
    }
    Ok(())
}

fn defined(result: Result<&mut Linker<GuestState>, LinkerError>) -> Result<(), GuestError> {
    result.map(|_| ()).map_err(GuestError::LinkerRefused)
}

/// Builds the linker holding exactly the granted capabilities.
fn build_linker(engine: &Engine, grants: Grants) -> Result<Linker<GuestState>, GuestError> {
    let mut linker = Linker::new(engine);

    defined(linker.func_wrap(
        IMPORT_MODULE,
        IMPORT_LOG,
        |mut caller: Caller<'_, GuestState>, level: i32, pointer: i32, length: i32| {
            let Some(memory) = caller.get_export(EXPORT_MEMORY).and_then(Extern::into_memory) else {
                return;
            };
            let Some(bytes) = slice_from_memory(&memory, &caller, pointer, length, MAX_GUEST_STRING_BYTES)
            else {
                return;
            };
            let message = String::from_utf8_lossy(&bytes).into_owned();
            let state = caller.data_mut();
            if state.logs.len() >= MAX_LOG_RECORDS {
                state.dropped_logs = state.dropped_logs.saturating_add(1);
                return;
            }
            state.logs.push(GuestLog {
                level: level_from_tag(level),
                message,
            });
        },
    ))?;

    if grants.filesystem_read {
        defined(linker.func_wrap(
            IMPORT_MODULE,
            IMPORT_READ_FILE,
            |mut caller: Caller<'_, GuestState>,
             path_pointer: i32,
             path_length: i32,
             out_pointer: i32,
             out_capacity: i32|
             -> i32 {
                let Some(memory) = caller.get_export(EXPORT_MEMORY).and_then(Extern::into_memory) else {
                    return HOST_ERR_INVALID;
                };
                let Some(raw) = slice_from_memory(
                    &memory,
                    &caller,
                    path_pointer,
                    path_length,
                    MAX_GUEST_STRING_BYTES,
                ) else {
                    return HOST_ERR_INVALID;
                };
                let Ok(requested) = std::str::from_utf8(&raw) else {
                    return HOST_ERR_INVALID;
                };
                let Ok(capacity) = usize::try_from(out_capacity) else {
                    return HOST_ERR_INVALID;
                };
                let root = caller.data().package_root.clone();
                let Some(path) = confined_path(&root, requested) else {
                    return HOST_ERR_NOT_FOUND;
                };
                let Some(bytes) = read_capped(&path, MAX_READ_FILE_BYTES) else {
                    return HOST_ERR_NOT_FOUND;
                };
                if bytes.len() > capacity {
                    return HOST_ERR_TOO_LARGE;
                }
                let Ok(offset) = usize::try_from(out_pointer) else {
                    return HOST_ERR_INVALID;
                };
                if memory.write(&mut caller, offset, &bytes).is_err() {
                    return HOST_ERR_INVALID;
                }
                i32::try_from(bytes.len()).unwrap_or(HOST_ERR_TOO_LARGE)
            },
        ))?;
    }

    if grants.environment {
        defined(linker.func_wrap(
            IMPORT_MODULE,
            IMPORT_ENV_GET,
            |mut caller: Caller<'_, GuestState>,
             name_pointer: i32,
             name_length: i32,
             out_pointer: i32,
             out_capacity: i32|
             -> i32 {
                let Some(memory) = caller.get_export(EXPORT_MEMORY).and_then(Extern::into_memory) else {
                    return HOST_ERR_INVALID;
                };
                let Some(raw) = slice_from_memory(
                    &memory,
                    &caller,
                    name_pointer,
                    name_length,
                    MAX_GUEST_STRING_BYTES,
                ) else {
                    return HOST_ERR_INVALID;
                };
                let Ok(name) = std::str::from_utf8(&raw) else {
                    return HOST_ERR_INVALID;
                };
                let Ok(capacity) = usize::try_from(out_capacity) else {
                    return HOST_ERR_INVALID;
                };
                let Some(value) = std::env::var_os(name) else {
                    return HOST_ERR_NOT_FOUND;
                };
                let bytes = value.to_string_lossy().into_owned().into_bytes();
                if bytes.len() > capacity {
                    return HOST_ERR_TOO_LARGE;
                }
                let Ok(offset) = usize::try_from(out_pointer) else {
                    return HOST_ERR_INVALID;
                };
                if memory.write(&mut caller, offset, &bytes).is_err() {
                    return HOST_ERR_INVALID;
                }
                i32::try_from(bytes.len()).unwrap_or(HOST_ERR_TOO_LARGE)
            },
        ))?;
    }

    Ok(linker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn a_confined_read_refuses_absolute_and_traversing_paths() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(confined_path(root, "/etc/passwd").is_none());
        assert!(confined_path(root, "../Cargo.toml").is_none());
        assert!(confined_path(root, "").is_none());
        assert!(confined_path(root, "src/../../Cargo.toml").is_none());
        assert!(confined_path(root, "Cargo.toml").is_some());
        assert!(confined_path(root, "./Cargo.toml").is_some());
    }

    #[test]
    fn an_unrecognised_log_level_becomes_info_rather_than_being_dropped() {
        assert_eq!(level_from_tag(0), LogLevel::Error);
        assert_eq!(level_from_tag(1), LogLevel::Warn);
        assert_eq!(level_from_tag(2), LogLevel::Info);
        assert_eq!(level_from_tag(3), LogLevel::Debug);
        assert_eq!(level_from_tag(4), LogLevel::Trace);
        assert_eq!(level_from_tag(-9), LogLevel::Info);
        assert_eq!(level_from_tag(99), LogLevel::Info);
    }
    fn guest_fixture(source: &str) -> (HostConfig, PluginId, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "crikey-wasm-host-test-{}-{}.wasm",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let bytes = wat::parse_str(source).expect("fixture WAT");
        std::fs::write(&path, bytes).expect("fixture module");
        let path_for_lookup = path.to_string_lossy().into_owned();
        let config = crate::config::HostConfig::from_lookup(|name| {
            (name == crate::config::ENV_MODULE).then(|| path_for_lookup.clone())
        })
        .expect("fixture config");
        (config, PluginId("wasm.test.fixture".into()), path)
    }

    const BASE: &str = r#"(module
        (memory (export "memory") 1)
        (func (export "crikey_abi_version") (result i32) i32.const 1)
        (func (export "crikey_alloc") (param i32) (result i32) i32.const 1024)
    "#;

    #[test]
    fn a_valid_module_loads_and_an_empty_suggestion_answers() {
        let (config, plugin, path) = guest_fixture(&format!(
            "{BASE} (func (export \"crikey_suggest\") (param i32 i32) (result i64) i64.const 0))"
        ));
        let mut guest = Guest::load(config, plugin).expect("fixture loads");
        let items = guest
            .suggest(&SuggestRequest {
                text: "demo".into(),
                normalized: "demo".into(),
                generation: 1,
                deadline_ms: Some(50),
                selected_item_id: None,
            })
            .expect("empty answer is still an answer");
        assert!(items.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_guest_trap_poisoned_instance_but_did_not_poison_the_host() {
        let (config, plugin, path) = guest_fixture(&format!(
            "{BASE} (func (export \"crikey_suggest\") (param i32 i32) (result i64) unreachable))"
        ));
        let mut guest = Guest::load(config, plugin).expect("fixture loads");
        let error = guest
            .suggest(&SuggestRequest {
                text: "trap".into(),
                normalized: "trap".into(),
                generation: 1,
                deadline_ms: None,
                selected_item_id: None,
            })
            .expect_err("unreachable traps");
        assert!(matches!(error, GuestError::Trap(_)));
        let second = guest.suggest(&SuggestRequest {
            text: "trap again".into(),
            normalized: "trap again".into(),
            generation: 2,
            deadline_ms: None,
            selected_item_id: None,
        });
        assert!(matches!(second, Err(GuestError::Trap(_))));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_spinning_guest_is_interrupted_by_fuel() {
        let (mut config, plugin, path) = guest_fixture(&format!(
            "{BASE} (func (export \"crikey_suggest\") (param i32 i32) (result i64) (i64.const 0) (loop br 0)))"
        ));
        config.fuel_per_ms = 1;
        config.hard_deadline_ms = 1;
        let mut guest = Guest::load(config, plugin).expect("fixture loads");
        let error = guest
            .suggest(&SuggestRequest {
                text: "spin".into(),
                normalized: "spin".into(),
                generation: 1,
                deadline_ms: Some(1),
                selected_item_id: None,
            })
            .expect_err("fuel must interrupt the loop");
        assert!(
            error.is_deadline(),
            "spinning guest must be named as a deadline: {error}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_import_without_its_manifest_grant_is_refused_before_instantiation() {
        let (config, plugin, path) = guest_fixture(
            r#"(module
            (import "crikey" "read_file" (func (param i32 i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "crikey_abi_version") (result i32) i32.const 1)
            (func (export "crikey_alloc") (param i32) (result i32) i32.const 1024)
            (func (export "crikey_suggest") (param i32 i32) (result i64) i64.const 0))
            "#,
        );
        let error = Guest::load(config, plugin).expect_err("filesystem is not granted");
        assert!(matches!(error, GuestError::DeniedCapability { name, .. } if name == "read_file"));
        let _ = std::fs::remove_file(path);
    }
}
