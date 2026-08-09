/*
 * A library that exports every required symbol but declares an ABI version
 * this host does not implement.
 *
 * The host must refuse it by name, by expected version and by found version,
 * and must do so without calling one of the functions below.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

#include "crikey_plugin.h"

#include <stdlib.h>

CRIKEY_PLUGIN_EXPORT const uint32_t crikey_plugin_abi_version = CRIKEY_PLUGIN_ABI_VERSION + 1000u;

CRIKEY_PLUGIN_EXPORT int32_t crikey_plugin_init(const CrikeyPluginHost *host, void **plugin_out)
{
    (void)host;
    (void)plugin_out;
    /* Reached only if the version gate failed to gate. */
    abort();
}

CRIKEY_PLUGIN_EXPORT int32_t crikey_plugin_suggest(void *plugin, const CrikeyPluginQuery *query,
                                                   CrikeyPluginItems *out_items)
{
    (void)plugin;
    (void)query;
    (void)out_items;
    abort();
}

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
