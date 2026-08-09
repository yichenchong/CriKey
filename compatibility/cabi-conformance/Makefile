# Builds the restricted C-ABI conformance fixtures the way a third party builds
# a plugin: outside the Cargo workspace, with a plain compiler invocation,
# against the published header and nothing else.
#
#   make                 # everything, into build/
#   make package         # only the well-behaved example package
#   make CC=clang        # any C11 compiler
#
# `make package` produces build/package/, which is exactly the shape of an
# installed CriKey plugin directory: a crikey.toml and a bin/ payload. The
# manifest is generated rather than committed because a package names the
# entrypoint for the platform it was built on, and the library's file name
# differs per platform.

CC ?= cc
CRIKEY_INCLUDE ?= ../../crates/crikey-cabi-host/include
OUT ?= build

CFLAGS ?= -std=c11 -O2 -Wall -Wextra -Werror -fPIC -fvisibility=hidden
CPPFLAGS += -I$(CRIKEY_INCLUDE)

UNAME_S := $(shell uname -s)
ifeq ($(UNAME_S),Darwin)
  SOEXT := dylib
  SHARED := -dynamiclib
  PLUGIN_OS := macos
else
  SOEXT := so
  SHARED := -shared
  PLUGIN_OS := linux
endif

UNAME_M := $(shell uname -m)
ifeq ($(UNAME_M),arm64)
  PLUGIN_ARCH := aarch64
else
  PLUGIN_ARCH := $(UNAME_M)
endif

PLUGIN_LIB := libcrikey_cabi_example.$(SOEXT)
PACKAGE := $(OUT)/package

.PHONY: all package fixtures clean
all: package fixtures

package: $(PACKAGE)/bin/$(PLUGIN_LIB) $(PACKAGE)/crikey.toml

$(PACKAGE)/bin/$(PLUGIN_LIB): plugin.c $(CRIKEY_INCLUDE)/crikey_plugin.h
	mkdir -p $(PACKAGE)/bin
	$(CC) $(CPPFLAGS) $(CFLAGS) $(SHARED) -o $@ plugin.c

$(PACKAGE)/crikey.toml: Makefile
	mkdir -p $(PACKAGE)
	printf '%s\n' \
	  'manifest-version = 1' \
	  '' \
	  '[plugin]' \
	  'id = "dev.example.cabi"' \
	  'name = "C ABI Conformance"' \
	  'version = "1.0.0"' \
	  'runtime = "c-abi"' \
	  'entrypoint.$(PLUGIN_OS)-$(PLUGIN_ARCH) = "bin/$(PLUGIN_LIB)"' \
	  '' \
	  '[permissions]' \
	  'native-library-loading = true' \
	  '' \
	  '[platform]' \
	  'os = ["$(PLUGIN_OS)"]' \
	  'arch = ["$(PLUGIN_ARCH)"]' \
	  '' \
	  '[performance]' \
	  'startup = "lazy"' \
	  'suggest-soft-timeout-ms = 200' \
	  'suggest-hard-timeout-ms = 1000' \
	  > $@

# The two libraries the host must refuse. They are not packages: nothing should
# ever get far enough with them for a manifest to matter.
fixtures: $(OUT)/libcrikey_cabi_bad_version.$(SOEXT) $(OUT)/libcrikey_cabi_missing_symbol.$(SOEXT)

$(OUT)/libcrikey_cabi_bad_version.$(SOEXT): bad_version.c $(CRIKEY_INCLUDE)/crikey_plugin.h
	mkdir -p $(OUT)
	$(CC) $(CPPFLAGS) $(CFLAGS) $(SHARED) -o $@ bad_version.c

$(OUT)/libcrikey_cabi_missing_symbol.$(SOEXT): missing_symbol.c $(CRIKEY_INCLUDE)/crikey_plugin.h
	mkdir -p $(OUT)
	$(CC) $(CPPFLAGS) $(CFLAGS) $(SHARED) -o $@ missing_symbol.c

clean:
	rm -rf $(OUT)
