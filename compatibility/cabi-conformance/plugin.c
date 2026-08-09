/*
 * Example restricted C-ABI plugin, built the way a third party builds one:
 * out of the Cargo workspace, with a plain Makefile, against the published
 * header and nothing else.
 *
 * It doubles as the conformance fixture for `crikey-cabi-host`. Mode selection
 * mirrors `compatibility/native-conformance`: `CRIKEY_CABI_MODE`, then the
 * trimmed contents of a `cabi-mode` file in the working directory, then
 * `echo`.
 *
 * Two of the modes deliberately misbehave. That is the point of the fixture:
 * the host's containment claim is only worth anything if something actually
 * exercises it.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

#if !defined(_WIN32)
// Expose POSIX declarations under the fixture's strict C11 build profile.
#define _POSIX_C_SOURCE 200809L
#endif
#include "crikey_plugin.h"

#include <stdio.h>
#include <stdlib.h>
#include <signal.h>
#include <string.h>

#if defined(_WIN32)
#include <windows.h>
#define crikey_getpid() ((unsigned long)GetCurrentProcessId())
#define crikey_sleep_ms(ms) Sleep((DWORD)(ms))
#else
// `nanosleep` is used instead of obsolete `usleep` under strict C11.
#include <time.h>
#include <unistd.h>
#define crikey_getpid() ((unsigned long)getpid())
#define crikey_sleep_ms(ms) \
    do { \
        struct timespec request = { (time_t)((ms) / 1000u), (long)(((ms) % 1000u) * 1000000u) }; \
        (void)nanosleep(&request, NULL); \
    } while (0)
#endif

CRIKEY_PLUGIN_EXPORT const uint32_t crikey_plugin_abi_version = CRIKEY_PLUGIN_ABI_VERSION;

#define MODE_MAX 64
#define STRINGS_MAX 16

typedef struct Plugin {
    char mode[MODE_MAX];
    const char *last_error;
} Plugin;

/*
 * One suggestion batch and everything it points at, in a single owned block.
 * `cookie` carries it back to `crikey_plugin_free_items`, so the free path
 * never has to reconstruct what the suggest path allocated.
 */
typedef struct Batch {
    CrikeyPluginItem items[2];
    char *strings[STRINGS_MAX];
    size_t string_count;
} Batch;

static void read_mode(char *out, size_t capacity)
{
    const char *from_env = getenv("CRIKEY_CABI_MODE");
    if (from_env != NULL && from_env[0] != '\0') {
        snprintf(out, capacity, "%s", from_env);
        return;
    }

    FILE *file = fopen("cabi-mode", "r");
    if (file != NULL) {
        char line[MODE_MAX] = {0};
        if (fgets(line, (int)sizeof(line), file) != NULL) {
            size_t length = strlen(line);
            while (length > 0 && (line[length - 1] == '\n' || line[length - 1] == '\r' ||
                                  line[length - 1] == ' ' || line[length - 1] == '\t')) {
                line[--length] = '\0';
            }
            if (length > 0) {
                snprintf(out, capacity, "%s", line);
                fclose(file);
                return;
            }
        }
        fclose(file);
    }

    snprintf(out, capacity, "%s", "echo");
}

/* Records an owned copy in the batch, or NULL if the batch has no room left. */
static const char *retain(Batch *batch, const char *value)
{
    if (batch->string_count >= STRINGS_MAX) {
        return NULL;
    }
    size_t length = strlen(value);
    char *copy = (char *)malloc(length + 1);
    if (copy == NULL) {
        return NULL;
    }
    memcpy(copy, value, length + 1);
    batch->strings[batch->string_count++] = copy;
    return copy;
}

static CrikeyPluginStr slice(const char *value)
{
    CrikeyPluginStr result;
    result.ptr = value;
    result.len = value == NULL ? 0u : strlen(value);
    return result;
}

CRIKEY_PLUGIN_EXPORT int32_t crikey_plugin_init(const CrikeyPluginHost *host, void **plugin_out)
{
    if (host == NULL || plugin_out == NULL) {
        return CRIKEY_PLUGIN_ERROR;
    }
    if (host->abi_version != CRIKEY_PLUGIN_ABI_VERSION) {
        return CRIKEY_PLUGIN_ERROR;
    }

    Plugin *plugin = (Plugin *)calloc(1, sizeof(Plugin));
    if (plugin == NULL) {
        return CRIKEY_PLUGIN_ERROR;
    }
    read_mode(plugin->mode, sizeof(plugin->mode));

    if (strcmp(plugin->mode, "fail-init") == 0) {
        /* Nothing is handed back, so the host will not call shutdown; the
         * plugin releases its own state before reporting the failure. */
        free(plugin);
        return CRIKEY_PLUGIN_ERROR;
    }

    *plugin_out = plugin;
    return CRIKEY_PLUGIN_OK;
}

CRIKEY_PLUGIN_EXPORT int32_t crikey_plugin_suggest(void *handle, const CrikeyPluginQuery *query,
                                                   CrikeyPluginItems *out_items)
{
    Plugin *plugin = (Plugin *)handle;
    if (plugin == NULL || query == NULL || out_items == NULL) {
        return CRIKEY_PLUGIN_ERROR;
    }
    plugin->last_error = NULL;

    if (strcmp(plugin->mode, "fail-suggest") == 0) {
        plugin->last_error = "this fixture always refuses to suggest";
        return CRIKEY_PLUGIN_ERROR;
    }

    if (strcmp(plugin->mode, "crash-on-suggest") == 0) {
        /* Abort is a portable abnormal process exit and cannot be optimised
         * away like a null-pointer store. The host must observe this as a
         * worker crash while the sibling worker remains alive. */
        abort();
    }

    if (strcmp(plugin->mode, "hang") == 0) {
        /* Never reads `cancelled`. The host's hard deadline is the only thing
         * that ends this, by aborting the host process. */
        for (;;) {
            crikey_sleep_ms(50);
        }
    }

    if (strcmp(plugin->mode, "slow") == 0) {
        /* The well-behaved counterpart: polls, and stops when asked. */
        while (query->cancelled == NULL || *query->cancelled == 0) {
            crikey_sleep_ms(5);
        }
        return CRIKEY_PLUGIN_CANCELLED;
    }

    Batch *batch = (Batch *)calloc(1, sizeof(Batch));
    if (batch == NULL) {
        plugin->last_error = "out of memory";
        return CRIKEY_PLUGIN_ERROR;
    }

    char pid_text[32];
    snprintf(pid_text, sizeof(pid_text), "%lu", crikey_getpid());

    char echo_id[256];
    size_t text_len = query->text.len;
    if (text_len > sizeof(echo_id) - 32) {
        text_len = sizeof(echo_id) - 32;
    }
    snprintf(echo_id, sizeof(echo_id), "cabi.echo:%.*s", (int)text_len,
             query->text.ptr == NULL ? "" : query->text.ptr);

    const char *pid_id = retain(batch, "cabi.pid");
    const char *pid_label = retain(batch, "host process id");
    const char *pid_target = retain(batch, pid_text);
    const char *echo_key = retain(batch, echo_id);
    const char *echo_label = retain(batch, "echo");

    if (pid_id == NULL || pid_label == NULL || pid_target == NULL || echo_key == NULL ||
        echo_label == NULL) {
        for (size_t index = 0; index < batch->string_count; ++index) {
            free(batch->strings[index]);
        }
        free(batch);
        plugin->last_error = "out of memory";
        return CRIKEY_PLUGIN_ERROR;
    }

    /* Item 0 reports the process id so a test can prove the library is NOT in
     * the launcher process. */
    batch->items[0].id = slice(pid_id);
    batch->items[0].label = slice(pid_label);
    batch->items[0].target = slice(pid_target);
    batch->items[0].score_hint = 100;

    batch->items[1].id = slice(echo_key);
    batch->items[1].label = slice(echo_label);
    batch->items[1].target = slice(echo_key);
    batch->items[1].score_hint = 50;

    out_items->items = batch->items;
    out_items->count = 2;
    out_items->cookie = batch;
    return CRIKEY_PLUGIN_OK;
}

CRIKEY_PLUGIN_EXPORT void crikey_plugin_free_items(void *handle, CrikeyPluginItems *items)
{
    (void)handle;
    if (items == NULL || items->cookie == NULL) {
        return;
    }
    Batch *batch = (Batch *)items->cookie;
    for (size_t index = 0; index < batch->string_count; ++index) {
        free(batch->strings[index]);
    }
    free(batch);
    items->items = NULL;
    items->count = 0;
    items->cookie = NULL;
}

CRIKEY_PLUGIN_EXPORT int32_t crikey_plugin_execute(void *handle, const CrikeyPluginAction *action)
{
    Plugin *plugin = (Plugin *)handle;
    if (plugin == NULL || action == NULL) {
        return CRIKEY_PLUGIN_ERROR;
    }
    plugin->last_error = NULL;

    if (strcmp(plugin->mode, "fail-execute") == 0) {
        plugin->last_error = "this fixture always refuses to execute";
        return CRIKEY_PLUGIN_ERROR;
    }
    /* Standard error is the supervised diagnostic channel; standard output
     * belongs to the host's protocol transport and is never touched. */
    fprintf(stderr, "cabi-conformance: executed %.*s\n", (int)action->item_id.len,
            action->item_id.ptr == NULL ? "" : action->item_id.ptr);
    return CRIKEY_PLUGIN_OK;
}

CRIKEY_PLUGIN_EXPORT void crikey_plugin_shutdown(void *handle)
{
    free(handle);
}

CRIKEY_PLUGIN_EXPORT CrikeyPluginStr crikey_plugin_last_error(void *handle)
{
    Plugin *plugin = (Plugin *)handle;
    if (plugin == NULL || plugin->last_error == NULL) {
        CrikeyPluginStr empty;
        empty.ptr = NULL;
        empty.len = 0;
        return empty;
    }
    return slice(plugin->last_error);
}
