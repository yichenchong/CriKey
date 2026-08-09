//! Minimal third-party-shaped CriKey WebAssembly plugin.
//!
//! This fixture intentionally depends on the published host ABI with its
//! interpreter feature disabled. It returns one deterministic suggestion and
//! demonstrates the complete export contract without importing any host
//! capability. Build it for `wasm32-unknown-unknown` and hand the resulting
//! `.wasm` file to a `crikey-wasm-host` package.

#![allow(unsafe_code)]

use crikey_core::{Category, Item, ItemId, PluginId};
use crikey_wasm_host::abi::{self, Limits, SuggestRequest};

#[no_mangle]
pub extern "C" fn crikey_abi_version() -> i32 {
    abi::ABI_VERSION
}

/// A deliberately tiny bump allocator. The host writes request blobs into
/// this memory and reads response blobs back out. A real plugin would use its
/// normal allocator; this fixture keeps the example's only unsafe operation in
/// one obvious place.
static mut NEXT: usize = 65_536;

#[no_mangle]
pub extern "C" fn crikey_alloc(length: i32) -> i32 {
    let Ok(length) = usize::try_from(length) else { return 0 };
    let start = unsafe { NEXT };
    let Some(end) = start.checked_add(length) else { return 0 };
    // The host caps this fixture's memory and the wasm module declares four
    // pages. Refuse rather than wrapping if a hostile request asks for more.
    if end > 4 * 65_536 {
        return 0;
    }
    unsafe { NEXT = end };
    i32::try_from(start).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn crikey_suggest(pointer: i32, length: i32) -> i64 {
    let Ok(pointer) = usize::try_from(pointer) else { return 0 };
    let Ok(length) = usize::try_from(length) else { return 0 };
    let request = unsafe { std::slice::from_raw_parts(pointer as *const u8, length) };
    let Ok(request) = SuggestRequest::decode(request, Limits::default()) else { return 0 };
    let plugin = PluginId("wasm.example.conformance".into());
    let item = Item {
        stable_id: ItemId("wasm-example".into()),
        plugin_id: plugin,
        category: Category::Keyword,
        label: format!("WASM: {}", request.text),
        description: "A suggestion from the out-of-process WASM conformance guest".into(),
        target: "wasm-example://suggestion".into(),
        search_terms: vec!["wasm".into()],
        icon_reference: None,
        argument_policy: Default::default(),
        hit_policy: Default::default(),
        score_hint: 10,
        metadata: Default::default(),
        actions: Vec::new(),
    };
    let bytes = abi::encode_item_batch(&[item]);
    let Ok(length) = u32::try_from(bytes.len()) else { return 0 };
    let output = crikey_alloc(i32::try_from(bytes.len()).unwrap_or(0));
    if output <= 0 {
        return 0;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), output as *mut u8, bytes.len());
    }
    // The export packs `(pointer << 32) | length`; the host reads the pair back
    // out of the same 64 bits, so the wrapping cast to the declared `i64`
    // return type preserves the bit pattern rather than the numeric value.
    ((u64::from(output as u32) << 32) | u64::from(length)) as i64
}

#[no_mangle]
pub extern "C" fn crikey_catalog() -> i64 {
    0
}

#[no_mangle]
pub extern "C" fn crikey_execute(_pointer: i32, _length: i32) -> i32 {
    0
}
