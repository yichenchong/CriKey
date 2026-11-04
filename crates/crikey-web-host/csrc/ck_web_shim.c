/*
 * Implementation of the C ABI declared in ck_web_shim.h.
 *
 * Transcribed from harnesses that were run against WPE WebKit 2.52.6 +
 * libwpe 1.16.3 + WPEBackend-fdo 1.16.1 and observed working: the headless SHM
 * export path, the `WebKitInputMethodContext` subclass, and raw input
 * dispatch. Lifecycle handling and navigation policy are the host's own.
 */
#define _GNU_SOURCE

#include "ck_web_shim.h"

#include <wpe/wpe.h>
#include <wpe/fdo.h>
#include <wpe/unstable/fdo-shm.h>
#include <wpe/webkit.h>
#include <wayland-server-core.h>

#include <glib.h>
#include <glib-unix.h>
#include <signal.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>

/* ------------------------------------------------------------------ *
 * The input method context.
 *
 * A concrete subclass is mandatory: WebKitInputMethodContext is abstract and
 * `webkit_web_view_set_input_method_context` is the only door the engine opens
 * for composition. The overrides are exactly the four the engine was measured
 * to use -- `get_preedit` (consulted on every `preedit-changed`),
 * `notify_cursor_area` (the caret rectangle the host needs to place the
 * candidate window), `filter_key_event` (consulted for injected events) and
 * `notify_surrounding` (accepted and discarded, because IME correction of
 * committed text is out of scope for v1).
 * ------------------------------------------------------------------ */

#define CK_TYPE_IM (ck_im_get_type())
G_DECLARE_FINAL_TYPE(CkIm, ck_im, CK, IM, WebKitInputMethodContext)

struct _CkIm {
    WebKitInputMethodContext parent;
    char *preedit;         /* owned, never NULL after construction */
    guint cursor_utf16;    /* WebKit EditingRange uses UTF-16 code units */
    guint length_utf16;
    ck_web_callbacks callbacks;
};
G_DEFINE_TYPE(CkIm, ck_im, WEBKIT_TYPE_INPUT_METHOD_CONTEXT)

static void ck_im_get_preedit(WebKitInputMethodContext *context, gchar **text,
                              GList **underlines, guint *cursor_offset)
{
    CkIm *self = CK_IM(context);
    const char *preedit = self->preedit ? self->preedit : "";
    if (text)
        *text = g_strdup(preedit);
    if (underlines) {
        *underlines = NULL;
        if (*preedit)
            *underlines = g_list_prepend(NULL,
                webkit_input_method_underline_new(0, self->length_utf16));
    }
    if (cursor_offset)
        *cursor_offset = self->cursor_utf16;
}

static void ck_im_notify_cursor_area(WebKitInputMethodContext *context, int x, int y,
                                     int width, int height)
{
    CkIm *self = CK_IM(context);
    if (self->callbacks.caret)
        self->callbacks.caret(self->callbacks.user, x, y, width, height);
}

/*
 * FALSE always: CriKey's host has already decided what to do with the key
 * before it reaches the engine. Composition keys never arrive here at all --
 * the launcher gives them to the system input method and sends the result as
 * an ImeEvent -- so a key that does arrive is one the page should see.
 */
static gboolean ck_im_filter_key_event(WebKitInputMethodContext *context, gpointer event)
{
    (void)context;
    (void)event;
    return FALSE;
}

/*
 * Accepted and dropped. The engine volunteers the text around the caret so an
 * input method can correct what it already committed; that correction is
 * waived for v1 (the mechanism is broken at both ends), and holding a copy of
 * the user's field contents for a feature that does not exist would be a
 * gratuitous secret to keep.
 */
static void ck_im_notify_surrounding(WebKitInputMethodContext *context, const gchar *text,
                                     guint length, guint cursor_index, guint selection_index)
{
    (void)context; (void)text; (void)length; (void)cursor_index; (void)selection_index;
}

static void ck_im_reset(WebKitInputMethodContext *context)
{
    CkIm *self = CK_IM(context);
    g_free(self->preedit);
    self->preedit = g_strdup("");
    self->cursor_utf16 = 0;
    self->length_utf16 = 0;
}

static void ck_im_finalize(GObject *object)
{
    g_clear_pointer(&CK_IM(object)->preedit, g_free);
    G_OBJECT_CLASS(ck_im_parent_class)->finalize(object);
}

static void ck_im_class_init(CkImClass *klass)
{
    WebKitInputMethodContextClass *context = WEBKIT_INPUT_METHOD_CONTEXT_CLASS(klass);
    G_OBJECT_CLASS(klass)->finalize = ck_im_finalize;
    context->get_preedit = ck_im_get_preedit;
    context->notify_cursor_area = ck_im_notify_cursor_area;
    context->filter_key_event = ck_im_filter_key_event;
    context->notify_surrounding = ck_im_notify_surrounding;
    context->reset = ck_im_reset;
}

static void ck_im_init(CkIm *self)
{
    self->preedit = g_strdup("");
    self->cursor_utf16 = 0;
    self->length_utf16 = 0;
}

/* ------------------------------------------------------------------ *
 * The surface.
 * ------------------------------------------------------------------ */

struct ck_web_surface {
    struct wpe_view_backend_exportable_fdo *exportable;
    struct wpe_view_backend *backend;
    WebKitWebView *view;
    WebKitNetworkSession *session;
    CkIm *im;
    ck_web_callbacks callbacks;
    bool gone_reported;
    bool load_failed;
};

static void report_gone(ck_web_surface *surface, int32_t reason, const char *detail)
{
    /* One Gone per surface: a crash that also closes the view must not be
     * announced twice, and the first cause is the true one. */
    if (surface->gone_reported)
        return;
    surface->gone_reported = true;
    if (surface->callbacks.gone)
        surface->callbacks.gone(surface->callbacks.user, reason, detail ? detail : "");
}

static void on_export_shm(void *data, struct wpe_fdo_shm_exported_buffer *buffer)
{
    ck_web_surface *surface = data;
    struct wl_shm_buffer *shm = wpe_fdo_shm_exported_buffer_get_shm_buffer(buffer);
    if (shm && surface->callbacks.frame) {
        int width = wl_shm_buffer_get_width(shm);
        int height = wl_shm_buffer_get_height(shm);
        int stride = wl_shm_buffer_get_stride(shm);
        if (width > 0 && height > 0 && stride > 0 &&
            (uint64_t)stride >= (uint64_t)width * 4) {
            wl_shm_buffer_begin_access(shm);
            const uint8_t *pixels = wl_shm_buffer_get_data(shm);
            if (pixels)
                surface->callbacks.frame(surface->callbacks.user, pixels, (uint32_t)width,
                                         (uint32_t)height, (uint32_t)stride);
            wl_shm_buffer_end_access(shm);
        }
    }
    /* The borrowed mapping is released only after Rust has copied the pixels.
     * Frame completion paces rendering; Rust's bounded output queue coalesces
     * frames independently when the launcher cannot keep up. */
    wpe_view_backend_exportable_fdo_dispatch_release_shm_exported_buffer(surface->exportable,
                                                                        buffer);
    wpe_view_backend_exportable_fdo_dispatch_frame_complete(surface->exportable);
}

static const struct wpe_view_backend_exportable_fdo_client s_exportable_client = {
    NULL, NULL, on_export_shm, NULL, NULL
};

static void on_load_changed(WebKitWebView *view, WebKitLoadEvent event, gpointer data)
{
    ck_web_surface *surface = data;
    if (!surface->callbacks.load_state)
        return;
    if (event == WEBKIT_LOAD_STARTED)
        surface->load_failed = false;
    else if (event == WEBKIT_LOAD_FINISHED && surface->load_failed)
        return; /* WebKit finishes failed loads too; preserve the failure. */
    int32_t state;
    switch (event) {
    case WEBKIT_LOAD_STARTED:    state = CK_WEB_LOAD_STARTED;    break;
    case WEBKIT_LOAD_REDIRECTED: state = CK_WEB_LOAD_REDIRECTED; break;
    case WEBKIT_LOAD_COMMITTED:  state = CK_WEB_LOAD_COMMITTED;  break;
    case WEBKIT_LOAD_FINISHED:   state = CK_WEB_LOAD_FINISHED;   break;
    default: return;
    }
    const char *uri = webkit_web_view_get_uri(view);
    const char *title = webkit_web_view_get_title(view);
    surface->callbacks.load_state(surface->callbacks.user, state, uri ? uri : "", "",
                                  webkit_web_view_can_go_back(view) ? 1 : 0,
                                  webkit_web_view_can_go_forward(view) ? 1 : 0,
                                  title ? title : "");
}

/* Link-visible for native-boundary tests of the actual signal callback. This
 * is not an additional entry point in the public embedding header. */
gboolean ck_web_on_load_failed(WebKitWebView *view, WebKitLoadEvent event, gchar *uri,
                               GError *error, gpointer data)
{
    ck_web_surface *surface = data;
    (void)event;
    (void)view;
    /* A replacement navigation cancels the previous one. Reporting that as a
     * page failure makes an ordinary Navigate kill the surface in the launcher. */
    if (error &&
        (g_error_matches(error, WEBKIT_NETWORK_ERROR, WEBKIT_NETWORK_ERROR_CANCELLED) ||
         /* Media documents intentionally replace their preliminary load. */
         g_error_matches(error, WEBKIT_MEDIA_ERROR, WEBKIT_MEDIA_ERROR_WILL_HANDLE_LOAD) ||
         g_error_matches(error, WEBKIT_POLICY_ERROR,
                         WEBKIT_POLICY_ERROR_FRAME_LOAD_INTERRUPTED_BY_POLICY_CHANGE)))
        return TRUE;
    surface->load_failed = true;
    if (surface->callbacks.load_state)
        surface->callbacks.load_state(surface->callbacks.user, CK_WEB_LOAD_FAILED,
                                      uri ? uri : "",
                                      (error && error->message) ? error->message : "load failed",
                                      webkit_web_view_can_go_back(surface->view) ? 1 : 0,
                                      webkit_web_view_can_go_forward(surface->view) ? 1 : 0, "");
    /* TRUE: the failure has been reported to the launcher, which owns the
     * in-place error panel. Letting the engine load its own error page would
     * paint a second, unstyled one underneath. */
    return TRUE;
}

static void on_web_process_terminated(WebKitWebView *view,
                                      WebKitWebProcessTerminationReason reason, gpointer data)
{
    ck_web_surface *surface = data;
    (void)view;
    switch (reason) {
    case WEBKIT_WEB_PROCESS_EXCEEDED_MEMORY_LIMIT:
        report_gone(surface, CK_WEB_GONE_MEMORY_LIMIT, "web process exceeded its memory limit");
        break;
    case WEBKIT_WEB_PROCESS_TERMINATED_BY_API:
        report_gone(surface, CK_WEB_GONE_CLOSED, "web process terminated by request");
        break;
    default:
        report_gone(surface, CK_WEB_GONE_CRASHED, "web process crashed");
        break;
    }
}

static void on_close(WebKitWebView *view, gpointer data)
{
    (void)view;
    report_gone((ck_web_surface *)data, CK_WEB_GONE_CLOSED, "page closed itself");
}

static gboolean on_decide_policy(WebKitWebView *view, WebKitPolicyDecision *decision,
                                  WebKitPolicyDecisionType type, gpointer data)
{
    (void)view;
    (void)data;
    if (type == WEBKIT_POLICY_DECISION_TYPE_NEW_WINDOW_ACTION) {
        /* One protocol surface owns one view, never an untracked popup. */
        webkit_policy_decision_ignore(decision);
        return TRUE;
    }
    if (type != WEBKIT_POLICY_DECISION_TYPE_NAVIGATION_ACTION)
        return FALSE;

    WebKitNavigationAction *action = webkit_navigation_policy_decision_get_navigation_action(
        WEBKIT_NAVIGATION_POLICY_DECISION(decision));
    WebKitURIRequest *request = webkit_navigation_action_get_request(action);
    const char *uri = webkit_uri_request_get_uri(request);
    GUri *parsed = uri ? g_uri_parse(uri, G_URI_FLAGS_NONE, NULL) : NULL;
    const char *scheme = parsed ? g_uri_get_scheme(parsed) : NULL;
    const char *host = parsed ? g_uri_get_host(parsed) : NULL;
    bool allowed = scheme && host && *host &&
        (!g_ascii_strcasecmp(scheme, "http") || !g_ascii_strcasecmp(scheme, "https"));
    if (parsed)
        g_uri_unref(parsed);
    /* Check engine-initiated navigations and redirects as well as protocol
     * commands. Initial URL validation alone does not protect the user's disk. */
    if (allowed)
        webkit_policy_decision_use(decision);
    else
        webkit_policy_decision_ignore(decision);
    return TRUE;
}

static void on_download_started(WebKitNetworkSession *session, WebKitDownload *download,
                                 gpointer data)
{
    (void)session;
    (void)data;
    /* No protocol operation can authorize a file destination. Without this
     * handler WebKit silently writes attachments to the user's Downloads. */
    webkit_download_cancel(download);
}

ck_web_surface *ck_web_surface_new(uint32_t width, uint32_t height, int32_t storage_mode,
                                  const char *storage_dir, const ck_web_callbacks *callbacks)
{
    if (width == 0 || height == 0 || !callbacks)
        return NULL;

    ck_web_surface *surface = g_new0(ck_web_surface, 1);
    surface->callbacks = *callbacks;

    surface->exportable = wpe_view_backend_exportable_fdo_create(&s_exportable_client, surface,
                                                                width, height);
    if (!surface->exportable) {
        g_free(surface);
        return NULL;
    }
    surface->backend = wpe_view_backend_exportable_fdo_get_view_backend(surface->exportable);
    if (!surface->backend) {
        wpe_view_backend_exportable_fdo_destroy(surface->exportable);
        g_free(surface);
        return NULL;
    }

    if (storage_mode == CK_WEB_STORAGE_PERSISTENT && storage_dir && *storage_dir) {
        char *cache = g_build_filename(storage_dir, "cache", NULL);
        char *data = g_build_filename(storage_dir, "data", NULL);
        surface->session = webkit_network_session_new(data, cache);
        /* A persistent website-data manager does not persist cookies by
         * default. Keep only non-session cookies, inside this plugin's
         * explicitly approved profile, never WebKit's shared default path. */
        char *cookies = g_build_filename(storage_dir, "cookies.sqlite", NULL);
        webkit_cookie_manager_set_persistent_storage(
            webkit_network_session_get_cookie_manager(surface->session), cookies,
            WEBKIT_COOKIE_PERSISTENT_STORAGE_SQLITE);
        g_free(cookies);
        g_free(cache);
        g_free(data);
    } else {
        surface->session = webkit_network_session_new_ephemeral();
    }
    g_signal_connect(surface->session, "download-started", G_CALLBACK(on_download_started), NULL);

    WebKitWebViewBackend *view_backend =
        webkit_web_view_backend_new(surface->backend,
                                    (GDestroyNotify)wpe_view_backend_exportable_fdo_destroy,
                                    surface->exportable);
    surface->view = WEBKIT_WEB_VIEW(g_object_new(WEBKIT_TYPE_WEB_VIEW,
                                                 "backend", view_backend,
                                                 "network-session", surface->session,
                                                 NULL));

    surface->im = g_object_new(CK_TYPE_IM, NULL);
    surface->im->callbacks = *callbacks;
    webkit_web_view_set_input_method_context(surface->view,
                                             WEBKIT_INPUT_METHOD_CONTEXT(surface->im));

    g_signal_connect(surface->view, "load-changed", G_CALLBACK(on_load_changed), surface);
    g_signal_connect(surface->view, "load-failed", G_CALLBACK(ck_web_on_load_failed), surface);
    g_signal_connect(surface->view, "web-process-terminated",
                     G_CALLBACK(on_web_process_terminated), surface);
    g_signal_connect(surface->view, "close", G_CALLBACK(on_close), surface);
    g_signal_connect(surface->view, "decide-policy", G_CALLBACK(on_decide_policy), surface);

    /* Without all three the engine treats the view as offscreen and stops
     * producing frames, and an unfocused view routes no key event. */
    wpe_view_backend_add_activity_state(surface->backend,
                                        wpe_view_activity_state_visible |
                                        wpe_view_activity_state_focused |
                                        wpe_view_activity_state_in_window);
    return surface;
}

void ck_web_surface_load_uri(ck_web_surface *surface, const char *uri)
{
    if (surface && uri)
        webkit_web_view_load_uri(surface->view, uri);
}


void ck_web_surface_resize(ck_web_surface *surface, uint32_t width, uint32_t height)
{
    if (surface && width > 0 && height > 0)
        wpe_view_backend_dispatch_set_size(surface->backend, width, height);
}

void ck_web_surface_key(ck_web_surface *surface, uint32_t time_ms, uint32_t keysym,
                        uint32_t hardware_keycode, int32_t pressed, int32_t repeat,
                        uint32_t modifiers)
{
    if (!surface)
        return;
    /* libwpe 1.x has no repeat flag. A repeated key-down is represented by
     * dispatching the subsequent pressed event, which is exactly this call. */
    (void)repeat;
    struct wpe_input_keyboard_event event = {
        .time = time_ms,
        .key_code = keysym,
        .hardware_key_code = hardware_keycode,
        .pressed = pressed != 0,
        .modifiers = modifiers,
    };
    wpe_view_backend_dispatch_keyboard_event(surface->backend, &event);
}

void ck_web_surface_pointer_motion(ck_web_surface *surface, uint32_t time_ms, int32_t x,
                                   int32_t y, uint32_t modifiers)
{
    if (!surface)
        return;
    struct wpe_input_pointer_event event = {
        .type = wpe_input_pointer_event_type_motion,
        .time = time_ms,
        .x = x,
        .y = y,
        .button = 0,
        .state = 0,
        .modifiers = modifiers,
    };
    wpe_view_backend_dispatch_pointer_event(surface->backend, &event);
}

void ck_web_surface_pointer_button(ck_web_surface *surface, uint32_t time_ms, int32_t x,
                                   int32_t y, uint32_t button, uint32_t state,
                                   uint32_t modifiers)
{
    if (!surface)
        return;
    struct wpe_input_pointer_event event = {
        .type = wpe_input_pointer_event_type_button,
        .time = time_ms,
        .x = x,
        .y = y,
        .button = button,
        .state = state,
        .modifiers = modifiers,
    };
    wpe_view_backend_dispatch_pointer_event(surface->backend, &event);
}

void ck_web_surface_axis(ck_web_surface *surface, uint32_t time_ms, int32_t x, int32_t y,
                         double delta_x, double delta_y, int32_t precise, uint32_t modifiers)
{
    if (!surface)
        return;
    if (!precise) {
        /* The legacy 2D discrete path turns a zero orthogonal delta into a
         * phantom notch. Separate nonzero axes preserve true line-wheel input. */
        struct wpe_input_axis_event event = {
            .type = wpe_input_axis_event_type_motion,
            .time = time_ms,
            .x = x,
            .y = y,
            .modifiers = modifiers,
        };
        if (delta_x != 0) {
            event.axis = 1;
            event.value = delta_x > 0 ? 1 : -1;
            wpe_view_backend_dispatch_axis_event(surface->backend, &event);
        }
        if (delta_y != 0) {
            event.axis = 0;
            event.value = delta_y > 0 ? 1 : -1;
            wpe_view_backend_dispatch_axis_event(surface->backend, &event);
        }
        return;
    }
    /* The 2D smooth variant, not the discrete one: a touchpad or a
     * high-resolution wheel delivers fractional deltas on both axes at once,
     * and quantising them into discrete clicks here would be information
     * thrown away before the page ever sees it. */
    struct wpe_input_axis_2d_event event = {
        .base = {
            .type = (enum wpe_input_axis_event_type)(wpe_input_axis_event_type_motion_smooth |
                                                     wpe_input_axis_event_type_mask_2d),
            .time = time_ms,
            .x = x,
            .y = y,
            .axis = 0,
            .value = 0,
            .modifiers = modifiers,
        },
        .x_axis = delta_x,
        .y_axis = delta_y,
    };
    wpe_view_backend_dispatch_axis_event(surface->backend, &event.base);
}

void ck_web_surface_focus(ck_web_surface *surface, int32_t focused)
{
    if (!surface)
        return;
    if (focused)
        wpe_view_backend_add_activity_state(surface->backend, wpe_view_activity_state_focused);
    else
        wpe_view_backend_remove_activity_state(surface->backend, wpe_view_activity_state_focused);
}

void ck_web_surface_set_preedit(ck_web_surface *surface, const char *text, uint32_t cursor_chars)
{
    if (!surface)
        return;
    const char *value = text ? text : "";
    g_free(surface->im->preedit);
    surface->im->preedit = g_strdup(value);
    /* The wire cursor counts Unicode scalar values. WebKit's GLib filter
     * passes get_preedit offsets straight to UTF-16 EditingRange and
     * CompositionUnderline, so supplementary characters occupy two units.
     * Compute both offsets once without allocating a second text buffer. */
    guint characters = 0;
    surface->im->cursor_utf16 = 0;
    surface->im->length_utf16 = 0;
    for (const char *at = value; *at; at = g_utf8_next_char(at)) {
        guint units = g_utf8_get_char(at) > 0xffff ? 2 : 1;
        surface->im->length_utf16 += units;
        if (characters++ < cursor_chars)
            surface->im->cursor_utf16 += units;
    }
}

void ck_web_surface_preedit_started(ck_web_surface *surface)
{
    if (surface)
        g_signal_emit_by_name(surface->im, "preedit-started");
}

void ck_web_surface_preedit_changed(ck_web_surface *surface)
{
    if (surface)
        g_signal_emit_by_name(surface->im, "preedit-changed");
}

void ck_web_surface_preedit_finished(ck_web_surface *surface)
{
    if (surface)
        g_signal_emit_by_name(surface->im, "preedit-finished");
}

void ck_web_surface_commit(ck_web_surface *surface, const char *text)
{
    if (surface)
        g_signal_emit_by_name(surface->im, "committed", text ? text : "");
}

void ck_web_surface_destroy(ck_web_surface *surface)
{
    if (!surface)
        return;
    /* Signal handlers hold `surface`; disconnect before the struct dies or a
     * teardown-time load event calls into freed memory. */
    g_signal_handlers_disconnect_by_data(surface->view, surface);
    surface->callbacks.frame = NULL;
    surface->callbacks.caret = NULL;
    surface->callbacks.load_state = NULL;
    surface->callbacks.gone = NULL;
    if (surface->im)
        surface->im->callbacks = surface->callbacks;
    g_clear_object(&surface->view);
    /* The WebKitWebViewBackend destroy notifier above owns the exportable and
     * its wpe_view_backend. Clearing the view has released both; destroying the
     * exportable here as well would double-free the backend during shutdown. */
    surface->exportable = NULL;
    surface->backend = NULL;
    g_clear_object(&surface->im);
    g_clear_object(&surface->session);
    g_free(surface);
}

/* ------------------------------------------------------------------ *
 * Process-wide loop.
 * ------------------------------------------------------------------ */

static GMainLoop *s_loop;
static guint s_fd_source;
static guint s_signal_sources[3];
static bool s_quit_requested;

static void clear_source(guint *source)
{
    if (!*source)
        return;
    guint id = *source;
    *source = 0;
    g_source_remove(id);
}

void ck_web_cleanup(void)
{
    clear_source(&s_fd_source);
    for (size_t index = 0; index < G_N_ELEMENTS(s_signal_sources); index++)
        clear_source(&s_signal_sources[index]);
    if (s_loop) {
        g_main_loop_unref(s_loop);
        s_loop = NULL;
    }
    s_quit_requested = false;
}

int32_t ck_web_init(void)
{
    ck_web_cleanup();
    if (!wpe_loader_init("libWPEBackend-fdo-1.0.so.1"))
        return CK_WEB_INIT_NO_LOADER;
    /* SHM, not EGL: this host renders with no GPU and no display server, and
     * the SHM path is the one that was measured working in that configuration. */
    if (!wpe_fdo_initialize_shm())
        return CK_WEB_INIT_NO_SHM;
    s_loop = g_main_loop_new(NULL, FALSE);
    return CK_WEB_INIT_OK;
}

typedef struct {
    void (*ready)(void *user);
    void *user;
    guint *source;
} FdWatch;

static gboolean on_fd_ready(gint fd, GIOCondition condition, gpointer data)
{
    FdWatch *watch = data;
    (void)fd;
    if (condition & (G_IO_HUP | G_IO_ERR)) {
        *watch->source = 0;
        watch->ready(watch->user);
        return G_SOURCE_REMOVE;
    }
    watch->ready(watch->user);
    return G_SOURCE_CONTINUE;
}

void ck_web_watch_fd(int32_t fd, void (*ready)(void *user), void *user)
{
    clear_source(&s_fd_source);
    if (!ready)
        return;
    FdWatch *watch = g_new0(FdWatch, 1);
    watch->ready = ready;
    watch->user = user;
    watch->source = &s_fd_source;
    s_fd_source = g_unix_fd_add_full(G_PRIORITY_DEFAULT, fd,
                                     G_IO_IN | G_IO_HUP | G_IO_ERR, on_fd_ready,
                                     watch, g_free);
}

static gboolean on_termination(gpointer data)
{
    FdWatch *watch = data;
    *watch->source = 0;
    watch->ready(watch->user);
    return G_SOURCE_REMOVE;
}

static void watch_signal(size_t index, int signum, void (*ready)(void *user), void *user)
{
    FdWatch *watch = g_new0(FdWatch, 1);
    watch->ready = ready;
    watch->user = user;
    watch->source = &s_signal_sources[index];
    s_signal_sources[index] =
        g_unix_signal_add_full(G_PRIORITY_HIGH, signum, on_termination, watch, g_free);
}

void ck_web_watch_termination(void (*ready)(void *user), void *user)
{
    for (size_t index = 0; index < G_N_ELEMENTS(s_signal_sources); index++)
        clear_source(&s_signal_sources[index]);
    if (!ready)
        return;
    watch_signal(0, SIGTERM, ready, user);
    watch_signal(1, SIGINT, ready, user);
    watch_signal(2, SIGHUP, ready, user);
}

void ck_web_run(void)
{
    /* g_main_loop_quit before g_main_loop_run is otherwise lost. */
    if (s_quit_requested)
        return;
    if (!s_loop)
        s_loop = g_main_loop_new(NULL, FALSE);
    g_main_loop_run(s_loop);
}

void ck_web_quit(void)
{
    s_quit_requested = true;
    if (s_loop)
        g_main_loop_quit(s_loop);
}
