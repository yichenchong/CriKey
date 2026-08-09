/*
 * A library with the right ABI version that never exports
 * `crikey_plugin_suggest`.
 *
 * The host must refuse it by symbol name before `crikey_plugin_init` runs, so
 * a half-exported plugin never gets to create state that would then need
 * tearing down.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

#include "crikey_plugin.h"

#include <stdlib.h>

CRIKEY_PLUGIN_EXPORT const uint32_t crikey_plugin_abi_version = CRIKEY_PLUGIN_ABI_VERSION;

CRIKEY_PLUGIN_EXPORT int32_t crikey_plugin_init(const CrikeyPluginHost *host, void **plugin_out)
{
    (void)host;
    (void)plugin_out;
    /* Reached only if the missing-symbol gate failed to gate. */
    abort();
}

/* `crikey_plugin_suggest` is deliberately absent. */

CRIKEY_PLUGIN_EXPORT void crikey_plugin_free_items(void *plugin, CrikeyPluginItems *items)
{
    (void)plugin;
    (void)items;
    abort();
}

CRIKEY_PLUGIN_EXPORT int32_t crikey_plugin_execute(void *plugin, const CrikeyPluginAction *action)
{
    (void)plugin;
    (void)action;
    abort();
}

CRIKEY_PLUGIN_EXPORT void crikey_plugin_shutdown(void *plugin)
{
    (void)plugin;
    abort();
}
