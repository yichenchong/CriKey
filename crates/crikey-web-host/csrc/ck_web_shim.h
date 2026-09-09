/*
 * C ABI over the parts of libwpe / WPEBackend-fdo / WPE WebKit that cannot be
 * reached from Rust without reimplementing the GObject type system.
 *
 * Why this file exists at all is argued in `src/engine.rs`. In short: the
 * `WebKitInputMethodContext` subclass CriKey must install is built out of
 * `G_DECLARE_FINAL_TYPE` / `G_DEFINE_TYPE`, which are C preprocessor
 * machinery, and this shim is a transcription of harnesses that were measured
 * working against this exact engine rather than a fresh design.
 *
 * Every entry point here is called from one thread: the thread that runs
 * `ck_web_run`. `ck_web_watch_fd` is the only way another thread gets the
 * loop's attention, and it does so by waking it, never by touching a surface.
 *
 * The numeric codes below deliberately mirror the protocol enums in
 * `crikey_native_protocol::message` so no translation table is needed on the
 * Rust side; `src/engine.rs` asserts the correspondence.
 */
#ifndef CK_WEB_SHIM_H
#define CK_WEB_SHIM_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct ck_web_surface ck_web_surface;

/* WebStorageMode */
#define CK_WEB_STORAGE_EPHEMERAL  1
#define CK_WEB_STORAGE_PERSISTENT 2

/* WebLoadCode */
#define CK_WEB_LOAD_STARTED    1
#define CK_WEB_LOAD_REDIRECTED 2
#define CK_WEB_LOAD_COMMITTED  3
#define CK_WEB_LOAD_FINISHED   4
#define CK_WEB_LOAD_FAILED     5

/* WebGoneCode */
#define CK_WEB_GONE_CLOSED             1
#define CK_WEB_GONE_MEMORY_LIMIT       2
#define CK_WEB_GONE_CRASHED            3
#define CK_WEB_GONE_STARTUP_FAILED     4
#define CK_WEB_GONE_PROTOCOL_VIOLATION 5

/* ck_web_init failures */
#define CK_WEB_INIT_OK           0
#define CK_WEB_INIT_NO_LOADER  (-1)
#define CK_WEB_INIT_NO_SHM     (-2)

/*
 * Callbacks are invoked on the loop thread, never re-entered, and every
 * pointer they receive is borrowed for the duration of the call only.
 *
 * `frame` in particular is called between `wl_shm_buffer_begin_access` and
 * `wl_shm_buffer_end_access`: the pixels are valid until it returns and
 * invalid immediately afterwards. Copy or convert; never retain.
 */
typedef struct {
    void *user;
    void (*frame)(void *user, const uint8_t *bgra, uint32_t width, uint32_t height,
                  uint32_t stride);
    void (*caret)(void *user, int32_t x, int32_t y, int32_t width, int32_t height);
    void (*load_state)(void *user, int32_t state, const char *url, const char *failure,
                       int32_t can_go_back, int32_t can_go_forward, const char *title);
    void (*gone)(void *user, int32_t reason, const char *detail);
} ck_web_callbacks;

/*
 * Loads the FDO backend and selects its headless SHM path. Must be called
 * exactly once, before any surface is created. Returns CK_WEB_INIT_OK or one
 * of the CK_WEB_INIT_* failures.
 */
int32_t ck_web_init(void);

/*
 * Creates one web surface of the requested pixel size.
 *
 * `storage_dir` is used only when `storage_mode` is CK_WEB_STORAGE_PERSISTENT,
 * where it names a directory this surface may keep website data in; ephemeral
 * surfaces get a session that writes nothing to disk. Returns NULL if the
 * engine refused to produce a backend.
 */
ck_web_surface *ck_web_surface_new(uint32_t width, uint32_t height, int32_t storage_mode,
                                   const char *storage_dir, const ck_web_callbacks *callbacks);

void ck_web_surface_load_uri(ck_web_surface *surface, const char *uri);
/* Test and diagnostic entry point: loads literal markup with no origin. */
void ck_web_surface_load_html(ck_web_surface *surface, const char *html);
void ck_web_surface_resize(ck_web_surface *surface, uint32_t width, uint32_t height);

/* `modifiers`, `button` and `state` are already in libwpe's own encoding;
 * `src/input.rs` performs the translation from the protocol's encoding. */
void ck_web_surface_key(ck_web_surface *surface, uint32_t time_ms, uint32_t keysym,
                        uint32_t hardware_keycode, int32_t pressed, uint32_t modifiers);
void ck_web_surface_pointer_motion(ck_web_surface *surface, uint32_t time_ms, int32_t x,
                                   int32_t y, uint32_t modifiers);
void ck_web_surface_pointer_button(ck_web_surface *surface, uint32_t time_ms, int32_t x,
                                   int32_t y, uint32_t button, uint32_t state,
                                   uint32_t modifiers);
void ck_web_surface_axis(ck_web_surface *surface, uint32_t time_ms, int32_t x, int32_t y,
                         double delta_x, double delta_y, uint32_t modifiers);
void ck_web_surface_focus(ck_web_surface *surface, int32_t focused);

/*
 * IME primitives. The *order* in which these are called is the whole
 * correctness argument and it lives in `src/ime.rs`, which is testable without
 * an engine; this layer only does what it is told.
 */
void ck_web_surface_set_preedit(ck_web_surface *surface, const char *text,
                                uint32_t cursor_chars);
void ck_web_surface_preedit_started(ck_web_surface *surface);
void ck_web_surface_preedit_changed(ck_web_surface *surface);
void ck_web_surface_preedit_finished(ck_web_surface *surface);
void ck_web_surface_commit(ck_web_surface *surface, const char *text);

void ck_web_surface_destroy(ck_web_surface *surface);

/*
 * Adds `fd` to the loop's poll set and calls `ready` whenever it is readable.
 * This is how the protocol reader thread hands work to the loop thread.
 */
void ck_web_watch_fd(int32_t fd, void (*ready)(void *user), void *user);

/* Runs the loop until `ck_web_quit`; both are called on the loop thread only,
 * except `ck_web_quit`, which is safe from any thread. */
void ck_web_run(void);
void ck_web_quit(void);

#ifdef __cplusplus
}
#endif

#endif /* CK_WEB_SHIM_H */
