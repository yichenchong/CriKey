//! The restricted C ABI, mirrored from `include/crikey_plugin.h`.
//!
//! This module is declarations only: types, symbol names and status codes. It
//! calls nothing and dereferences nothing, so the header and this file can be
//! diffed field for field. Every struct is `#[repr(C)]` and every field is in
//! header order; the `reserved` arrays exist so a later ABI revision can add
//! fields without moving one that already exists (ADR-0015).

use std::ffi::{c_char, c_void};
use std::ptr;

/// ABI revision this host implements. Must match `CRIKEY_PLUGIN_ABI_VERSION`.
pub const ABI_VERSION: u32 = 1;

/// Success.
pub const STATUS_OK: i32 = 0;
/// Plugin-reported failure; the request fails and the plugin keeps running.
pub const STATUS_ERROR: i32 = 1;
/// The plugin observed the cancellation flag and abandoned the request.
pub const STATUS_CANCELLED: i32 = 2;
/// The plugin does not implement this request.
pub const STATUS_UNSUPPORTED: i32 = 3;

/// Exported data symbol read before any other symbol is resolved.
pub const SYMBOL_ABI_VERSION: &str = "crikey_plugin_abi_version";
pub const SYMBOL_INIT: &str = "crikey_plugin_init";
pub const SYMBOL_SUGGEST: &str = "crikey_plugin_suggest";
pub const SYMBOL_FREE_ITEMS: &str = "crikey_plugin_free_items";
pub const SYMBOL_EXECUTE: &str = "crikey_plugin_execute";
pub const SYMBOL_SHUTDOWN: &str = "crikey_plugin_shutdown";
/// The one optional symbol; its absence is not a refusal.
pub const SYMBOL_LAST_ERROR: &str = "crikey_plugin_last_error";

/// Every symbol a library must export, in resolution order. A library missing
/// any of these is refused by name before `crikey_plugin_init` is called.
pub const REQUIRED_SYMBOLS: [&str; 6] = [
    SYMBOL_ABI_VERSION,
    SYMBOL_INIT,
    SYMBOL_SUGGEST,
    SYMBOL_FREE_ITEMS,
    SYMBOL_EXECUTE,
    SYMBOL_SHUTDOWN,
];

/// A borrowed UTF-8 slice with an explicit length. Nothing in this ABI scans
/// for a NUL terminator it cannot prove is present.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AbiStr {
    pub ptr: *const c_char,
    pub len: usize,
}

impl AbiStr {
    pub const EMPTY: Self = Self {
        ptr: ptr::null(),
        len: 0,
    };

    /// Borrows `value` for as long as the caller keeps it alive. Used only for
    /// host-owned strings passed *into* a plugin call.
    pub fn borrowed(value: &str) -> Self {
        Self {
            ptr: value.as_ptr().cast::<c_char>(),
            len: value.len(),
        }
    }
}

impl Default for AbiStr {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// One suggestion row produced by a plugin.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AbiItem {
    pub id: AbiStr,
    pub label: AbiStr,
    pub description: AbiStr,
    pub target: AbiStr,
    pub score_hint: i32,
    pub reserved0: u32,
    pub reserved: [u64; 2],
}

/// A plugin-owned batch. The host returns this struct verbatim to
/// `crikey_plugin_free_items`; `cookie` is never interpreted.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AbiItems {
    pub items: *mut AbiItem,
    pub count: usize,
    pub cookie: *mut c_void,
    pub reserved: [u64; 2],
}

impl Default for AbiItems {
    fn default() -> Self {
        Self {
            items: ptr::null_mut(),
            count: 0,
            cookie: ptr::null_mut(),
            reserved: [0; 2],
        }
    }
}

/// Host identity handed to `crikey_plugin_init`, valid for that call only.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AbiHost {
    pub abi_version: u32,
    pub max_items: u32,
    pub max_string_bytes: u32,
    pub reserved0: u32,
    pub plugin_id: AbiStr,
    pub package_dir: AbiStr,
    pub reserved: [u64; 4],
}

/// One suggestion request, valid for that call only.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AbiQuery {
    pub text: AbiStr,
    pub normalized: AbiStr,
    pub generation: u64,
    pub deadline_ms: u64,
    pub cancelled: *const i32,
    pub reserved: [u64; 2],
}

/// One action request, valid for that call only.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AbiAction {
    pub item_id: AbiStr,
    pub action_id: AbiStr,
    pub argument: AbiStr,
    pub deadline_ms: u64,
    pub cancelled: *const i32,
    pub reserved: [u64; 2],
}

/// `int32_t crikey_plugin_init(const CrikeyPluginHost*, void**)`.
pub type InitFn = unsafe extern "C" fn(*const AbiHost, *mut *mut c_void) -> i32;
/// `int32_t crikey_plugin_suggest(void*, const CrikeyPluginQuery*, CrikeyPluginItems*)`.
pub type SuggestFn = unsafe extern "C" fn(*mut c_void, *const AbiQuery, *mut AbiItems) -> i32;
/// `void crikey_plugin_free_items(void*, CrikeyPluginItems*)`.
pub type FreeItemsFn = unsafe extern "C" fn(*mut c_void, *mut AbiItems);
/// `int32_t crikey_plugin_execute(void*, const CrikeyPluginAction*)`.
pub type ExecuteFn = unsafe extern "C" fn(*mut c_void, *const AbiAction) -> i32;
/// `void crikey_plugin_shutdown(void*)`.
pub type ShutdownFn = unsafe extern "C" fn(*mut c_void);
/// `CrikeyPluginStr crikey_plugin_last_error(void*)`.
pub type LastErrorFn = unsafe extern "C" fn(*mut c_void) -> AbiStr;
