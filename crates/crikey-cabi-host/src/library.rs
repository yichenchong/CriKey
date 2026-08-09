//! Symbol resolution and the version gate.
//!
//! Loading is split in two so the dangerous half is small and the contract is
//! testable without a C compiler in the loop. [`SymbolSource`] is "somewhere
//! symbols come from"; [`DynamicLibrary`] is the only implementation that
//! actually maps third-party code, and the only place in this crate that calls
//! `dlopen`. [`PluginAbi::resolve`] holds the entire refusal policy and never
//! touches the filesystem.
//!
//! The order is load-bearing (ADR-0015):
//!
//! 1. read the `crikey_plugin_abi_version` **data** symbol — a plain load, so a
//!    mismatch is refused without executing any plugin code at all;
//! 2. resolve every remaining required symbol, refusing by name;
//! 3. only then call `crikey_plugin_init`.

use std::ffi::c_void;
use std::path::Path;
use std::ptr::NonNull;

use crate::abi;

/// Why a library was refused. Every variant names the library and the specific
/// thing that was wrong with it; none of them is a generic "load failed".
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum LoadError {
    #[error("{library}: refused before loading: {reason}")]
    Policy { library: String, reason: String },
    #[error("{library}: the operating system could not load the library: {detail}")]
    Open { library: String, detail: String },
    #[error("{library}: exports ABI version {found}, but crikey-cabi-host implements version {expected}")]
    AbiVersionMismatch {
        library: String,
        found: u32,
        expected: u32,
    },
    #[error("{library}: required symbol `{symbol}` is not exported")]
    MissingSymbol { library: String, symbol: &'static str },
    #[error("{library}: crikey_plugin_init failed with status {status}{detail}")]
    Init {
        library: String,
        status: i32,
        detail: String,
    },
}

impl LoadError {
    /// The library this refusal is about, for a diagnostic that only has the
    /// error in hand.
    pub fn library(&self) -> &str {
        match self {
            Self::Policy { library, .. }
            | Self::Open { library, .. }
            | Self::AbiVersionMismatch { library, .. }
            | Self::MissingSymbol { library, .. }
            | Self::Init { library, .. } => library,
        }
    }
}

/// Somewhere exported symbols come from.
///
/// [`DynamicLibrary`] implements it in production. Tests implement it over a
/// table of `extern "C"` functions defined in the test binary, which exercises
/// the real ABI marshalling and the real refusal policy without `dlopen`.
pub trait SymbolSource: std::fmt::Debug {
    /// Identifies the library in refusal messages.
    fn origin(&self) -> &str;

    /// Resolves one exported symbol.
    ///
    /// # Safety
    ///
    /// The returned address is valid only while `self` lives, and only
    /// meaningful when read as the type this ABI declares for `name`.
    #[allow(unsafe_code)]
    unsafe fn symbol(&self, name: &str) -> Option<NonNull<c_void>>;
}

/// A third-party shared library mapped into this host process.
///
/// Dropping this unmaps the library, invalidating every pointer resolved from
/// it. [`crate::plugin::CabiPlugin`] therefore owns the source and the resolved
/// pointers together, and drops the pointers first.
#[derive(Debug)]
pub struct DynamicLibrary {
    library: libloading::Library,
    origin: String,
}

impl DynamicLibrary {
    /// Maps `path`, which the caller has already put through
    /// [`crate::policy::resolve_library`].
    ///
    /// This runs the platform loader over third-party bytes, and on ELF and
    /// Mach-O that can execute library constructors before this function
    /// returns. There is no portable way to map a library without giving it
    /// that opportunity, which is precisely why the whole thing lives in a
    /// separate process (ADR-0015).
    pub fn open(path: &Path) -> Result<Self, LoadError> {
        let origin = path.display().to_string();
        // SAFETY: `Library::new` is unsafe because the library's initialisers
        // run arbitrary code, and because unloading must outlive every
        // resolved symbol. The first is inherent to this feature and is
        // contained by the process boundary; the second is upheld by
        // `CabiPlugin`, which never lets a resolved pointer outlive `self`.
        #[allow(unsafe_code)]
        let opened = unsafe { libloading::Library::new(path) };
        let library = opened.map_err(|error| LoadError::Open {
            library: origin.clone(),
            detail: error.to_string(),
        })?;
        Ok(Self { library, origin })
    }
}

impl SymbolSource for DynamicLibrary {
    fn origin(&self) -> &str {
        &self.origin
    }

    #[allow(unsafe_code)]
    unsafe fn symbol(&self, name: &str) -> Option<NonNull<c_void>> {
        // `libloading` wants a NUL-terminated name; build one here rather than
        // relying on every caller to have written the trailing byte.
        let mut owned = Vec::with_capacity(name.len() + 1);
        owned.extend_from_slice(name.as_bytes());
        owned.push(0);
        // SAFETY: `owned` is NUL-terminated and every name this crate passes
        // is a fixed ASCII literal with no interior NUL. The address is handed
        // out under this method's documented contract, which ties it to
        // `self`'s lifetime.
        let symbol: libloading::Symbol<'_, *mut c_void> = unsafe { self.library.get(&owned) }.ok()?;
        #[allow(unsafe_code)]
        let address = unsafe { *symbol.into_raw() };
        NonNull::new(address)
    }
}

/// The resolved entry points of one library, after the version gate passed and
/// before any of them has been called.
#[derive(Debug, Clone, Copy)]
pub struct PluginAbi {
    pub init: abi::InitFn,
    pub suggest: abi::SuggestFn,
    pub free_items: abi::FreeItemsFn,
    pub execute: abi::ExecuteFn,
    pub shutdown: abi::ShutdownFn,
    pub last_error: Option<abi::LastErrorFn>,
}

impl PluginAbi {
    /// Applies the version gate, then resolves every required entry point.
    ///
    /// # Safety
    ///
    /// `source` must outlive every use of the returned [`PluginAbi`], and the
    /// library behind it must actually implement the ABI its version symbol
    /// claims. That second condition is the one this host cannot verify and
    /// does not pretend to (ADR-0015).
    #[allow(unsafe_code)]
    pub unsafe fn resolve(source: &dyn SymbolSource) -> Result<Self, LoadError> {
        let library = source.origin().to_owned();

        // Step 1: the version gate, before anything else. The header declares
        // this as a data symbol precisely so reading it is a load, not a call.
        // SAFETY: delegated to this function's contract.
        let version_symbol =
            unsafe { source.symbol(abi::SYMBOL_ABI_VERSION) }.ok_or_else(|| LoadError::MissingSymbol {
                library: library.clone(),
                symbol: abi::SYMBOL_ABI_VERSION,
            })?;
        // SAFETY: the header declares `const uint32_t`. A library exporting
        // the name as something narrower is malformed in a way no loader can
        // detect, and the process boundary is the containment for that.
        let found = unsafe { version_symbol.cast::<u32>().as_ptr().read() };
        if found != abi::ABI_VERSION {
            return Err(LoadError::AbiVersionMismatch {
                library,
                found,
                expected: abi::ABI_VERSION,
            });
        }

        // Step 2: every required function, refused by name. All of them are
        // resolved before `init` runs, so a half-exported library never gets
        // to create state that would then need tearing down.
        let required = |symbol: &'static str| -> Result<NonNull<c_void>, LoadError> {
            // SAFETY: delegated to this function's contract.
            unsafe { source.symbol(symbol) }.ok_or_else(|| LoadError::MissingSymbol {
                library: library.clone(),
                symbol,
            })
        };
        let init = required(abi::SYMBOL_INIT)?;
        let suggest = required(abi::SYMBOL_SUGGEST)?;
        let free_items = required(abi::SYMBOL_FREE_ITEMS)?;
        let execute = required(abi::SYMBOL_EXECUTE)?;
        let shutdown = required(abi::SYMBOL_SHUTDOWN)?;
        // SAFETY: delegated to this function's contract.
        let last_error = unsafe { source.symbol(abi::SYMBOL_LAST_ERROR) };

        // SAFETY: each address was exported under the name the header binds to
        // exactly this signature. A library exporting the name with a
        // different signature is the class of bug the version symbol exists to
        // prevent and the process boundary exists to contain.
        unsafe {
            Ok(Self {
                init: std::mem::transmute::<*mut c_void, abi::InitFn>(init.as_ptr()),
                suggest: std::mem::transmute::<*mut c_void, abi::SuggestFn>(suggest.as_ptr()),
                free_items: std::mem::transmute::<*mut c_void, abi::FreeItemsFn>(free_items.as_ptr()),
                execute: std::mem::transmute::<*mut c_void, abi::ExecuteFn>(execute.as_ptr()),
                shutdown: std::mem::transmute::<*mut c_void, abi::ShutdownFn>(shutdown.as_ptr()),
                last_error: last_error
                    .map(|symbol| std::mem::transmute::<*mut c_void, abi::LastErrorFn>(symbol.as_ptr())),
            })
        }
    }
}
