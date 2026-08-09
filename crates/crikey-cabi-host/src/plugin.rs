//! The adapter: a loaded C library presented to CriKey as an ordinary native
//! plugin.
//!
//! Everything hostile about the boundary is handled here and nowhere else.
//! A batch a plugin returns is treated as untrusted input in exactly the same
//! way a wire frame is (README invariant 8): counts and lengths are bounded
//! before a byte is read, every string is validated as UTF-8, and a batch that
//! breaks the contract is refused whole rather than partially accepted.

use std::ffi::c_void;
use std::path::PathBuf;
use std::time::Duration;

use crikey_core::{CoreError, Item, Result};
use crikey_plugin_sdk::{
    CatalogSink, ExecuteRequest, ItemBuilder, LogLevel, Plugin, PluginContext, Query, SuggestionSink,
};

use crate::abi::{self, AbiAction, AbiHost, AbiItems, AbiQuery, AbiStr};
use crate::library::{LoadError, PluginAbi, SymbolSource};
use crate::watchdog::Watchdog;

/// Extra time a plugin gets past its declared hard timeout before the host
/// gives up and aborts. The manifest deadline is the contract; this is the
/// margin for a plugin that is genuinely on its way out.
pub const ABORT_GRACE: Duration = Duration::from_millis(2_000);

/// Ceiling on rows in one batch. A plugin is told this value in
/// [`AbiHost::max_items`] and a batch above it is refused rather than clipped:
/// silently dropping rows would make a plugin bug look like a ranking bug.
pub const MAX_ITEMS: u32 = 4_096;

/// Ceiling on any single string a plugin returns.
pub const MAX_STRING_BYTES: u32 = 64 * 1024;

/// Everything the host decided before the library was loaded.
#[derive(Debug, Clone)]
pub struct HostOptions {
    pub plugin_id: String,
    pub package_dir: PathBuf,
    /// When the cancellation flag is raised.
    pub suggest_soft: Duration,
    /// The manifest's hard suggestion timeout.
    pub suggest_hard: Duration,
    /// The manifest's hard timeout applied to actions.
    pub action_hard: Duration,
}

/// A loaded restricted C-ABI plugin.
///
/// Field order is load-bearing: `source` is declared last so the library is
/// unmapped *after* the handle has been shut down and the resolved pointers
/// have gone out of scope.
#[derive(Debug)]
pub struct CabiPlugin {
    options: HostOptions,
    entry: PluginAbi,
    handle: *mut c_void,
    origin: String,
    watchdog: Watchdog,
    /// Set once a plugin breaks the ABI contract. Every later call is refused
    /// without entering plugin code: a library that produced one impossible
    /// batch has no credibility left, and calling it again is how a latent
    /// pointer bug turns into a crash somewhere unrelated.
    poisoned: Option<String>,
    shutdown_called: bool,
    _source: Box<dyn SymbolSource>,
}

impl CabiPlugin {
    /// Resolves the ABI, applies the version gate, and initialises the plugin.
    ///
    /// # Safety
    ///
    /// `source` must expose the symbols of a library built against
    /// `include/crikey_plugin.h`. This is the one thing the host cannot check
    /// and does not claim to (ADR-0015).
    #[allow(unsafe_code)]
    pub unsafe fn load(
        source: Box<dyn SymbolSource>,
        options: HostOptions,
    ) -> std::result::Result<Self, LoadError> {
        let origin = source.origin().to_owned();
        // SAFETY: delegated to this function's contract; `source` is owned by
        // the value being constructed, so it outlives every resolved pointer.
        let entry = unsafe { PluginAbi::resolve(source.as_ref()) }?;

        let package_dir = options.package_dir.display().to_string();
        let host = AbiHost {
            abi_version: abi::ABI_VERSION,
            max_items: MAX_ITEMS,
            max_string_bytes: MAX_STRING_BYTES,
            reserved0: 0,
            plugin_id: AbiStr::borrowed(&options.plugin_id),
            package_dir: AbiStr::borrowed(&package_dir),
            reserved: [0; 4],
        };
        let mut handle: *mut c_void = std::ptr::null_mut();
        // SAFETY: `host` outlives the call and the header says it is borrowed
        // for the call only; `handle` is a valid out-parameter.
        let status = unsafe { (entry.init)(&host, &mut handle) };
        if status != abi::STATUS_OK {
            // SAFETY: the header allows `last_error` on a handle whose `init`
            // failed; the plugin owns the bytes and we only copy them.
            let detail = unsafe { read_last_error(&entry, handle) };
            return Err(LoadError::Init {
                library: origin,
                status,
                detail: if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                },
            });
        }

        Ok(Self {
            options,
            entry,
            handle,
            origin,
            watchdog: Watchdog::spawn(),
            poisoned: None,
            shutdown_called: false,
            _source: source,
        })
    }

    /// The library this plugin came from, for diagnostics.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    fn poison(&mut self, context: &dyn PluginContext, detail: String) -> CoreError {
        context.log(
            LogLevel::Error,
            &format!("{}: {detail}; refusing every further call", self.origin),
        );
        self.poisoned = Some(detail.clone());
        CoreError::Invalid(detail)
    }

    fn check_poisoned(&self) -> Result<()> {
        match &self.poisoned {
            Some(detail) => Err(CoreError::Invalid(format!(
                "{}: refused because the plugin previously broke the ABI contract: {detail}",
                self.origin
            ))),
            None => Ok(()),
        }
    }

    /// Turns a non-OK status into the error that describes it, reading the
    /// plugin's optional detail. Never called with `CRIKEY_PLUGIN_OK`.
    #[allow(unsafe_code)]
    fn status_error(&self, what: &str, status: i32) -> CoreError {
        if status == abi::STATUS_CANCELLED {
            return CoreError::Cancelled;
        }
        // SAFETY: called immediately after a returning entry point on a handle
        // this host still owns; the plugin owns the bytes and we only copy.
        let detail = unsafe { read_last_error(&self.entry, self.handle) };
        let name = match status {
            abi::STATUS_UNSUPPORTED => "is not implemented by this plugin".to_owned(),
            _ => format!("failed with status {status}"),
        };
        CoreError::Invalid(if detail.is_empty() {
            format!("{}: {what} {name}", self.origin)
        } else {
            format!("{}: {what} {name}: {detail}", self.origin)
        })
    }
}

// SAFETY-adjacent note: `CabiPlugin` holds raw pointers and is therefore
// neither `Send` nor `Sync`, which is exactly right. The SDK serving loop owns
// it on one thread and never moves it; the watchdog thread touches only the
// atomic flag, never the handle.

impl Drop for CabiPlugin {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        if self.shutdown_called {
            return;
        }
        self.shutdown_called = true;
        // A poisoned plugin is not asked to tear itself down: its state is not
        // trustworthy, and the process is about to end anyway.
        if self.poisoned.is_none() {
            self.watchdog.arm(
                "crikey_plugin_shutdown",
                self.options.action_hard,
                self.options.action_hard + ABORT_GRACE,
            );
            // SAFETY: the handle came from a successful `init`, no other call
            // is in flight, and this runs at most once.
            unsafe { (self.entry.shutdown)(self.handle) };
            self.watchdog.disarm();
        }
    }
}

impl Plugin for CabiPlugin {
    fn start(&mut self, _context: &dyn PluginContext) -> Result<()> {
        // Initialisation already happened in `load`, before this host accepted
        // the library at all. There is nothing left to do that could fail.
        Ok(())
    }

    fn build_catalog(&mut self, _context: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        // The restricted ABI has no catalog entry point. This is a real gap and
        // is reported as one: an empty, terminated catalog rather than a
        // pretend one (README invariant 7).
        sink.finish()
    }

    #[allow(unsafe_code)]
    fn suggest(
        &mut self,
        query: Query,
        context: &dyn PluginContext,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        self.check_poisoned()?;
        if sink.is_cancelled() {
            return Ok(());
        }

        let requested = query.deadline_ms.map(Duration::from_millis);
        let soft = requested.map_or(self.options.suggest_soft, |value| {
            value.min(self.options.suggest_soft)
        });
        let hard = requested.map_or(self.options.suggest_hard, |value| {
            value.min(self.options.suggest_hard)
        });

        let raw_query = AbiQuery {
            text: AbiStr::borrowed(&query.text),
            normalized: AbiStr::borrowed(&query.normalized),
            generation: query.generation,
            deadline_ms: u64::try_from(hard.as_millis()).unwrap_or(u64::MAX),
            cancelled: self.watchdog.cancel_flag(),
            reserved: [0; 2],
        };
        let mut out = AbiItems::default();

        self.watchdog
            .arm("crikey_plugin_suggest", soft, hard + ABORT_GRACE);
        // SAFETY: both pointers reference locals that outlive the call, the
        // handle came from a successful `init`, and no other call is in flight
        // because the SDK serving loop is single-threaded.
        let status = unsafe { (self.entry.suggest)(self.handle, &raw_query, &mut out) };
        self.watchdog.disarm();

        if status != abi::STATUS_OK {
            // The header is explicit that a failing `suggest` must not have
            // handed the host anything to free.
            // A plugin that observed cancellation is not a failure: the host
            // asked it to stop and it stopped.
            let error = self.status_error("crikey_plugin_suggest", status);
            if matches!(error, CoreError::Cancelled) {
                return sink.finish();
            }
            return Err(error);
        }

        // SAFETY: `status` was OK, so the header guarantees `out` describes a
        // plugin-owned batch that stays valid until `free_items`.
        let decoded = unsafe { decode_items(&out) };
        // Returned before the decode result is inspected: the host owes this
        // call for every successful `suggest`, including one whose contents it
        // is about to refuse.
        // SAFETY: `out` is byte-for-byte what the plugin produced, and this is
        // the single matching free for this batch.
        unsafe { (self.entry.free_items)(self.handle, &mut out) };

        let items = match decoded {
            Ok(items) => items,
            Err(detail) => return Err(self.poison(context, detail)),
        };
        if !items.is_empty() {
            sink.emit_batch(items)?;
        }
        sink.finish()
    }

    #[allow(unsafe_code)]
    fn execute(&mut self, request: ExecuteRequest, _context: &dyn PluginContext) -> Result<()> {
        self.check_poisoned()?;
        let action = request.action.map(|action| action.0).unwrap_or_default();
        let argument = request.argument.unwrap_or_default();
        let hard = self.options.action_hard;
        let raw_action = AbiAction {
            item_id: AbiStr::borrowed(&request.item.0),
            action_id: AbiStr::borrowed(&action),
            argument: AbiStr::borrowed(&argument),
            deadline_ms: u64::try_from(hard.as_millis()).unwrap_or(u64::MAX),
            cancelled: self.watchdog.cancel_flag(),
            reserved: [0; 2],
        };

        self.watchdog
            .arm("crikey_plugin_execute", hard, hard + ABORT_GRACE);
        // SAFETY: `raw_action` outlives the call, the handle came from a
        // successful `init`, and no other call is in flight.
        let status = unsafe { (self.entry.execute)(self.handle, &raw_action) };
        self.watchdog.disarm();

        if status != abi::STATUS_OK {
            return Err(self.status_error("crikey_plugin_execute", status));
        }
        Ok(())
    }

    fn stop(&mut self, _context: &dyn PluginContext) -> Result<()> {
        // `Drop` performs the single `crikey_plugin_shutdown` call, so an
        // orderly stop and a dropped host converge on the same teardown.
        Ok(())
    }
}

/// Copies one plugin-owned string, refusing rather than repairing.
///
/// # Safety
///
/// `value` must be a slice the plugin returned during a call that has not yet
/// returned ownership, as the header defines it.
#[allow(unsafe_code)]
unsafe fn read_str(value: &AbiStr, field: &str) -> std::result::Result<String, String> {
    if value.len == 0 {
        return Ok(String::new());
    }
    if value.ptr.is_null() {
        return Err(format!("{field} has length {} but a null pointer", value.len));
    }
    if value.len > MAX_STRING_BYTES as usize {
        return Err(format!(
            "{field} is {} bytes, above the {MAX_STRING_BYTES} byte ceiling",
            value.len
        ));
    }
    // SAFETY: length is bounded and non-zero and the pointer is non-null; the
    // header requires the bytes to stay valid for the duration of the call.
    let bytes = unsafe { std::slice::from_raw_parts(value.ptr.cast::<u8>(), value.len) };
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|error| format!("{field} is not valid UTF-8: {error}"))
}

/// Copies the optional `crikey_plugin_last_error` detail, or an empty string.
///
/// # Safety
///
/// Must be called on a handle the host still owns, with no other call in
/// flight.
#[allow(unsafe_code)]
unsafe fn read_last_error(entry: &PluginAbi, handle: *mut c_void) -> String {
    let Some(last_error) = entry.last_error else {
        return String::new();
    };
    // SAFETY: delegated to this function's contract.
    let raw = unsafe { last_error(handle) };
    // SAFETY: the header binds the returned slice to "valid until the next
    // call on this handle", and this is that window.
    unsafe { read_str(&raw, "crikey_plugin_last_error") }.unwrap_or_default()
}

/// Validates and copies a whole batch, or refuses all of it.
///
/// # Safety
///
/// `raw` must be the batch a `crikey_plugin_suggest` call returned with status
/// `CRIKEY_PLUGIN_OK`, before the matching `free_items`.
#[allow(unsafe_code)]
unsafe fn decode_items(raw: &AbiItems) -> std::result::Result<Vec<Item>, String> {
    if raw.count == 0 {
        return Ok(Vec::new());
    }
    if raw.count > MAX_ITEMS as usize {
        return Err(format!(
            "returned {} rows, above the {MAX_ITEMS} row ceiling",
            raw.count
        ));
    }
    if raw.items.is_null() {
        return Err(format!("returned {} rows with a null item pointer", raw.count));
    }
    // SAFETY: count is bounded and the pointer is non-null; the header
    // requires the array to stay valid until `free_items`.
    let rows = unsafe { std::slice::from_raw_parts(raw.items.cast_const(), raw.count) };

    let mut items = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        // SAFETY: each field is a slice inside the batch the header keeps
        // valid for this window.
        let id = unsafe { read_str(&row.id, &format!("item[{index}].id")) }?;
        if id.is_empty() {
            return Err(format!("item[{index}].id is empty"));
        }
        let label = unsafe { read_str(&row.label, &format!("item[{index}].label")) }?;
        let description = unsafe { read_str(&row.description, &format!("item[{index}].description")) }?;
        let target = unsafe { read_str(&row.target, &format!("item[{index}].target")) }?;
        items.push(
            ItemBuilder::new(id, label)
                .description(description)
                .target(target)
                .score_hint(row.score_hint)
                .build(),
        );
    }
    Ok(items)
}
