/*
 * CriKey restricted C plugin ABI, version 1.
 *
 * A plugin built against this header is an ordinary shared library
 * (`.so`/`.dylib`/`.dll`).  It is NEVER loaded into the CriKey launcher
 * process.  `crikey-cabi-host` is a separate executable, started and
 * supervised exactly like any other native plugin, that loads the library and
 * speaks the CriKey native protocol on its behalf.  The library is therefore
 * in-process for that host and out-of-process for CriKey (ADR-0015; spec 2.2,
 * 2.3; acceptance criterion 30).
 *
 * A plugin has the FULL authority of the `crikey-cabi-host` process.  Nothing
 * in this ABI is a sandbox.  The isolation this design buys is that a fault,
 * a hang or a leak destroys only that host process, and the supervisor
 * restarts it; it does not buy protection from a plugin that deliberately
 * misuses the authority it was given.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

#ifndef CRIKEY_PLUGIN_H
#define CRIKEY_PLUGIN_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32)
#define CRIKEY_PLUGIN_EXPORT __declspec(dllexport)
#else
#define CRIKEY_PLUGIN_EXPORT __attribute__((visibility("default")))
#endif

/*
 * ---------------------------------------------------------------------------
 * Versioning
 * ---------------------------------------------------------------------------
 *
 * `crikey_plugin_abi_version` is an exported DATA symbol, not a function.  The
 * host reads it with a plain load before it resolves any other symbol and
 * before it calls anything, so a version mismatch is refused without executing
 * one instruction of plugin code.  A mismatch is fatal and is reported by
 * library path, expected version and found version.
 *
 * The ABI grows only by adding trailing fields inside the `reserved` arrays
 * below or by bumping this constant.  Fields are never renumbered, resized or
 * repurposed.  Every `reserved` element MUST be zero when a plugin fills a
 * struct and MUST be ignored when a plugin reads one.
 */
#define CRIKEY_PLUGIN_ABI_VERSION 1u

/* Status codes returned by every fallible entry point. */
#define CRIKEY_PLUGIN_OK 0          /* Success. */
#define CRIKEY_PLUGIN_ERROR 1       /* Plugin-reported failure; request fails. */
#define CRIKEY_PLUGIN_CANCELLED 2   /* Abandoned because `cancelled` was set. */
#define CRIKEY_PLUGIN_UNSUPPORTED 3 /* Plugin does not implement this request. */

/*
 * ---------------------------------------------------------------------------
 * Shared types
 * ---------------------------------------------------------------------------
 */

/*
 * A borrowed UTF-8 string slice.  `ptr` is never dereferenced when `len` is
 * zero, so an empty string may use a NULL `ptr`.  The bytes are NOT required
 * to be NUL-terminated and MUST NOT be assumed to be.
 *
 * The host validates UTF-8 on every string a plugin returns and refuses the
 * whole batch if any is invalid; it never repairs or truncates one.
 */
typedef struct CrikeyPluginStr {
    const char *ptr;
    size_t len;
} CrikeyPluginStr;

/* One suggestion row.  `id` must be non-empty and stable across queries. */
typedef struct CrikeyPluginItem {
    CrikeyPluginStr id;
    CrikeyPluginStr label;
    CrikeyPluginStr description;
    CrikeyPluginStr target;
    int32_t score_hint;
    uint32_t reserved0;
    uint64_t reserved[2];
} CrikeyPluginItem;

/*
 * A plugin-owned batch of rows.
 *
 * OWNERSHIP: the plugin allocates `items` and every byte every `CrikeyPluginStr`
 * inside it points at.  The host copies what it needs during the call and then
 * hands the SAME struct back to `crikey_plugin_free_items`.  The plugin must
 * keep the memory valid until that call and must free it there.  `cookie` is
 * opaque to the host and is returned untouched; use it to carry an allocator
 * handle or an arena pointer.
 *
 * A plugin that returns zero rows must still set `items` and `count`
 * consistently (`count == 0`, `items` NULL or valid) and will still receive the
 * matching `crikey_plugin_free_items` call.
 */
typedef struct CrikeyPluginItems {
    CrikeyPluginItem *items;
    size_t count;
    void *cookie;
    uint64_t reserved[2];
} CrikeyPluginItems;

/*
 * Host identity handed to `crikey_plugin_init`.
 *
 * OWNERSHIP: host-owned and valid ONLY for the duration of the `init` call.  A
 * plugin that needs any of it later must copy it.
 */
typedef struct CrikeyPluginHost {
    /* Always equal to CRIKEY_PLUGIN_ABI_VERSION for the loaded plugin. */
    uint32_t abi_version;
    /* Upper bound on `CrikeyPluginItems::count`; a larger batch is refused. */
    uint32_t max_items;
    /* Upper bound in bytes on any single string a plugin returns. */
    uint32_t max_string_bytes;
    uint32_t reserved0;
    /* Plugin id the host is serving under. */
    CrikeyPluginStr plugin_id;
    /* Absolute path of the installed package directory that owns the library. */
    CrikeyPluginStr package_dir;
    uint64_t reserved[4];
} CrikeyPluginHost;

/*
 * One suggestion request.
 *
 * OWNERSHIP: host-owned and valid ONLY for the duration of the `suggest` call.
 *
 * `cancelled` is never NULL.  It points at a host-owned flag that becomes
 * non-zero when the request is superseded or its soft deadline passes.  Read it
 * with a relaxed atomic or volatile load; poll it before expensive work and
 * inside every loop.  A plugin that observes a set flag SHOULD return
 * CRIKEY_PLUGIN_CANCELLED promptly.
 *
 * A plugin that ignores `cancelled` past `deadline_ms` cannot be interrupted:
 * the restricted ABI has no safe way to unwind a foreign call.  The host
 * aborts its own process instead, and the supervisor treats that as a worker
 * crash.  Ignoring the deadline therefore costs the plugin its process.
 */
typedef struct CrikeyPluginQuery {
    CrikeyPluginStr text;
    CrikeyPluginStr normalized;
    /* Monotonic query generation; echo nothing, it is informational. */
    uint64_t generation;
    /* Milliseconds from entry after which the host stops waiting. */
    uint64_t deadline_ms;
    const volatile int32_t *cancelled;
    uint64_t reserved[2];
} CrikeyPluginQuery;

/*
 * One action execution request.
 *
 * OWNERSHIP: host-owned and valid ONLY for the duration of the `execute` call.
 * `action_id` and `argument` may be empty, meaning "not supplied".
 */
typedef struct CrikeyPluginAction {
    CrikeyPluginStr item_id;
    CrikeyPluginStr action_id;
    CrikeyPluginStr argument;
    uint64_t deadline_ms;
    const volatile int32_t *cancelled;
    uint64_t reserved[2];
} CrikeyPluginAction;

/*
 * ---------------------------------------------------------------------------
 * Required exports
 * ---------------------------------------------------------------------------
 *
 * All six symbols below are REQUIRED.  The host resolves every one of them
 * before it calls `crikey_plugin_init`, and refuses the library by symbol name
 * and library path if any is missing, so a partially exported plugin never
 * gets to create state.
 *
 * THREAD SAFETY: the host serialises every call on one library.  No two
 * entry points are ever active at the same time, and every call arrives on the
 * same thread.  A plugin does not need internal locking for host calls; a
 * plugin that starts its own threads is responsible for their synchronisation
 * and for joining them in `crikey_plugin_shutdown`.
 *
 * REENTRANCY: this ABI passes no function pointers in either direction.  A
 * plugin cannot call back into the host, so it cannot re-enter it.  Diagnostics
 * go to `stderr`, which the supervisor captures and bounds.
 *
 * A PLUGIN MUST NEVER:
 *   - let a C++ or `setjmp`/`longjmp` exception escape any entry point; an
 *     unwind across this boundary is undefined behaviour and will abort;
 *   - retain a pointer the host passed in beyond the call that passed it;
 *   - free memory the host owns, or expect the host to free memory it owns;
 *   - block indefinitely, or ignore `cancelled` past `deadline_ms`;
 *   - call `exit`, `abort`, `_exit` or install a handler for the host's
 *     signals;
 *   - close, dup or write to file descriptors 0, 1 or 2 (the host's protocol
 *     transport and captured diagnostics live there);
 *   - assume it will be unloaded; `crikey_plugin_shutdown` may be the last
 *     thing that runs before the process exits.
 */

/* Exported DATA symbol.  Must be initialised to CRIKEY_PLUGIN_ABI_VERSION. */
CRIKEY_PLUGIN_EXPORT extern const uint32_t crikey_plugin_abi_version;

/*
 * Creates plugin state.
 *
 * PRECONDITIONS: `host` and `plugin_out` are non-NULL; called exactly once,
 * before any other entry point.
 * POSTCONDITIONS: on CRIKEY_PLUGIN_OK, `*plugin_out` holds the plugin's own
 * handle, which the host passes back unchanged to every later call and finally
 * to `crikey_plugin_shutdown`.  A NULL handle is legal for a stateless plugin.
 * On any other status the host does not call `crikey_plugin_shutdown`, so the
 * plugin must release whatever it allocated before returning the failure.
 */
CRIKEY_PLUGIN_EXPORT int32_t crikey_plugin_init(const CrikeyPluginHost *host, void **plugin_out);

/*
 * Answers one suggestion request.
 *
 * PRECONDITIONS: `query` and `out_items` are non-NULL; `*out_items` is zeroed
 * by the host before the call.
 * POSTCONDITIONS: on CRIKEY_PLUGIN_OK, `*out_items` describes a plugin-owned
 * batch of at most `max_items` rows, and the host will call
 * `crikey_plugin_free_items` with it exactly once.  On any other status the
 * host ignores `*out_items` and does NOT call `crikey_plugin_free_items`, so
 * the plugin must not have allocated anything it expects the host to return.
 */
CRIKEY_PLUGIN_EXPORT int32_t crikey_plugin_suggest(void *plugin, const CrikeyPluginQuery *query,
                                                   CrikeyPluginItems *out_items);

/*
 * Releases a batch previously returned by `crikey_plugin_suggest`.
 *
 * PRECONDITIONS: `items` is exactly the struct the plugin produced, unmodified;
 * called exactly once per successful `crikey_plugin_suggest`.
 * POSTCONDITIONS: every allocation described by `items` is released.  The host
 * has already copied everything it needed, so the plugin may free eagerly.
 */
CRIKEY_PLUGIN_EXPORT void crikey_plugin_free_items(void *plugin, CrikeyPluginItems *items);

/*
 * Executes one selected item or action.
 *
 * PRECONDITIONS: `action` is non-NULL.
 * POSTCONDITIONS: CRIKEY_PLUGIN_OK means the action was performed.  Nothing
 * crosses back: an action reports outcome, not data.
 */
CRIKEY_PLUGIN_EXPORT int32_t crikey_plugin_execute(void *plugin, const CrikeyPluginAction *action);

/*
 * Destroys plugin state.
 *
 * PRECONDITIONS: called at most once, after every other call has returned, and
 * only if `crikey_plugin_init` returned CRIKEY_PLUGIN_OK.
 * POSTCONDITIONS: the handle is invalid.  The host makes no further call.
 */
CRIKEY_PLUGIN_EXPORT void crikey_plugin_shutdown(void *plugin);

/*
 * ---------------------------------------------------------------------------
 * Optional export
 * ---------------------------------------------------------------------------
 */

/*
 * Returns a detail for the most recent non-OK status on this handle.
 *
 * A plugin with nothing to say returns a zero-length slice.  The slice carries
 * its own length for the same reason every other string here does: the host
 * never scans for a terminator it cannot prove is present.
 *
 * OWNERSHIP: plugin-owned.  The host copies it immediately, bounded by
 * `max_string_bytes`, and never frees it.  It must stay valid until the next
 * call on the same handle.
 */
CRIKEY_PLUGIN_EXPORT CrikeyPluginStr crikey_plugin_last_error(void *plugin);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* CRIKEY_PLUGIN_H */
