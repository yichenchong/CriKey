#!/usr/bin/env bash
#
# Stage a relocatable CPython into the layout CriKey's interpreter discovery
# looks for (spec 14.11).
#
# Why this exists
# ---------------
# A shipped CriKey must not depend on whatever Python the target machine
# happens to have: a modern plugin that declares `requires-python` has to work
# on a machine with no system interpreter at all, and plugin code must never
# import from a system-wide `site-packages`. So an installer lays a runtime
# down beside the binary and `crikey_python_host::bundled_interpreter_beside`
# finds it with no environment variable set.
#
# Why it does not download
# ------------------------
# Fetching a runtime would make every build depend on a network and on
# whatever the remote served that day, and would put an unreviewed binary into
# an artefact users trust. The archive is an input: the caller pins it, the
# caller checksums it, and this script fails loudly when it is missing rather
# than producing an artefact that quietly has no runtime in it.
#
# Expected upstream distribution
# ------------------------------
# python-build-standalone (https://github.com/astral-sh/python-build-standalone),
# the `install_only` archive for the target triple — for example
# `cpython-3.13.1+20241205-x86_64-unknown-linux-gnu-install_only.tar.gz`.
# Those builds are relocatable: the interpreter derives its prefix from its own
# argv[0], which is what lets an installed tree be moved. The full (non
# `install_only`) archives, which nest the same tree under `python/install`,
# are accepted too. Nothing else is: a distro interpreter is not relocatable
# and would resolve back into the system prefix, defeating the point.
#
# Layout produced
# ---------------
#   <dest>/python-runtime/bin/python3          (Unix)
#   <dest>/python-runtime/python.exe           (Windows)
#
# `<dest>` is the directory discovery resolves the runtime against:
#   * a self-contained tree  -> the directory holding the `crikey` binary
#   * a prefix install       -> `<prefix>/lib/crikey`, because discovery also
#                               looks at `../lib/crikey/python-runtime`
#                               relative to the binary and a prefix install
#                               must not litter `bin/`.
#
# Usage
# -----
#   packaging/stage-python-runtime.sh --dest DIR [--archive FILE] [--force]
#
#   --archive FILE  python-build-standalone archive. Defaults to
#                   $CRIKEY_PYTHON_STANDALONE_ARCHIVE.
#   --force         Replace an already staged runtime.

set -euo pipefail

program="${0##*/}"

die() {
    printf '%s: %s\n' "$program" "$*" >&2
    exit 1
}

note() {
    printf '%s: %s\n' "$program" "$*"
}

usage() {
    sed -n '3,52p' "$0" | sed 's|^# \{0,1\}||'
}

dest=""
archive="${CRIKEY_PYTHON_STANDALONE_ARCHIVE:-}"
force=0

while [ $# -gt 0 ]; do
    case "$1" in
        --dest)
            [ $# -ge 2 ] || die "--dest requires a directory"
            dest="$2"
            shift 2
            ;;
        --dest=*)
            dest="${1#--dest=}"
            shift
            ;;
        --archive)
            [ $# -ge 2 ] || die "--archive requires a file"
            archive="$2"
            shift 2
            ;;
        --archive=*)
            archive="${1#--archive=}"
            shift
            ;;
        --force)
            force=1
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument '$1' (try --help)"
            ;;
    esac
done

[ -n "$dest" ] || die "--dest is required: the directory the runtime is staged beside"

if [ -z "$archive" ]; then
    die "no python-build-standalone archive given.
  Pass --archive FILE or set CRIKEY_PYTHON_STANDALONE_ARCHIVE.
  This script never downloads one: the runtime that ends up inside a release
  artefact is a pinned, reviewed input, not whatever a remote served today.
  Get an 'install_only' build for the target triple from
  https://github.com/astral-sh/python-build-standalone/releases
  If you meant to ship without a bundled runtime, do not call this script --
  CriKey then falls back to the system interpreter on PATH, and the installer
  must declare that dependency."
fi

[ -f "$archive" ] || die "archive '$archive' does not exist"
# Validate archive metadata before extraction.  In particular, do not let an
# archive member redirect the staged tree through a symlink or write outside
# the scratch directory via `..`.  `tar`'s default path sanitisation is not a
# sufficient security boundary, and a symlink at the top-level `python` entry
# would otherwise make the final runtime point outside the artefact.
case "$archive" in
    *.tar.gz | *.tgz)
        members="$(tar -tzf "$archive")" ||
            die "cannot read archive '$archive' (not a valid tar archive)"
        details="$(tar -tvzf "$archive")" ||
            die "cannot inspect archive '$archive'"
        ;;
    *.tar.zst)
        command -v zstd >/dev/null 2>&1 ||
            die "'$archive' needs zstd, which is not installed"
        members="$(zstd -dc -- "$archive" | tar -tf -)" ||
            die "cannot read archive '$archive' (not a valid tar.zst archive)"
        details="$(zstd -dc -- "$archive" | tar -tvf -)" ||
            die "cannot inspect archive '$archive'"
        ;;
    *.tar)
        members="$(tar -tf "$archive")" ||
            die "cannot read archive '$archive' (not a valid tar archive)"
        details="$(tar -tvf "$archive")" ||
            die "cannot inspect archive '$archive'"
        ;;
    *)
        die "unrecognised archive '$archive' (expected .tar.gz, .tgz, .tar.zst or .tar)"
        ;;
esac

while IFS= read -r member; do
    [ -n "$member" ] || continue
    case "$member" in
        /* | ../* | */../* | */.. | ..)
            die "archive '$archive' contains an unsafe path '$member'"
            ;;
    esac
done <<< "$members"

while IFS= read -r detail; do
    case "$detail" in
        l* | h*)
            die "archive '$archive' contains a symlink or hardlink; links are not allowed"
            ;;
    esac
done <<< "$details"
runtime_dir="$dest/python-runtime"

if [ -e "$runtime_dir" ]; then
    [ "$force" -eq 1 ] || die "'$runtime_dir' already exists (pass --force to replace it)"
    rm -rf -- "$runtime_dir"
fi

# Extract into a scratch directory first, and remove the staged tree again
# unless every check below passed: a half-populated or rejected runtime left on
# disk is worse than none at all, because discovery would find it and an
# installer would ship it.
scratch="$(mktemp -d "${TMPDIR:-/tmp}/crikey-python-runtime.XXXXXX")"
staged_ok=0
cleanup() {
    if [ -n "${scratch:-}" ]; then
        rm -rf -- "$scratch"
    fi
    if [ "${staged_ok:-0}" -ne 1 ] && [ -n "${runtime_dir:-}" ]; then
        rm -rf -- "$runtime_dir"
    fi
}
trap cleanup EXIT

case "$archive" in
    *.tar.gz | *.tgz)
        tar -xzf "$archive" -C "$scratch"
        ;;
    *.tar.zst)
        command -v zstd >/dev/null 2>&1 || die "'$archive' needs zstd, which is not installed"
        zstd -dc -- "$archive" | tar -xf - -C "$scratch"
        ;;
    *.tar)
        tar -xf "$archive" -C "$scratch"
        ;;
    *)
        die "unrecognised archive '$archive' (expected .tar.gz, .tgz, .tar.zst or .tar)"
        ;;
esac

# `install_only` archives unpack to `python/`; the full builds nest the same
# tree under `python/install`. Both are acceptable; anything else is not the
# distribution this script knows how to lay out.
if [ -d "$scratch/python/install" ]; then
    extracted="$scratch/python/install"
elif [ -d "$scratch/python" ]; then
    extracted="$scratch/python"
else
    die "'$archive' does not look like a python-build-standalone archive: no top-level 'python' directory"
fi

if [ -x "$extracted/bin/python3" ]; then
    interpreter_relative="bin/python3"
elif [ -f "$extracted/python.exe" ]; then
    interpreter_relative="python.exe"
else
    die "'$archive' contains no interpreter at 'bin/python3' or 'python.exe'"
fi

mkdir -p -- "$dest"
mv -- "$extracted" "$runtime_dir"

interpreter="$runtime_dir/$interpreter_relative"
[ -f "$interpreter" ] || die "staging produced no regular interpreter at '$interpreter'"
if [ "$interpreter_relative" = "bin/python3" ] && [ ! -x "$interpreter" ]; then
    die "staged interpreter at '$interpreter' is not executable"
fi

# Prove the staged tree is the runtime CriKey will accept, rather than assuming
# it. Discovery probes the interpreter the same way, so a tree that fails here
# would have failed at launch -- better to fail the build.
#
# `-I` is isolated mode: no user site directory, no `PYTHON*` variables, no
# script directory on `sys.path`. It is deliberately stricter than the `-S` the
# worker uses, because what is being checked is that the runtime stands alone.
if [ "$interpreter_relative" = "bin/python3" ]; then
    report="$(
        env -u PYTHONHOME -u PYTHONPATH -u PYTHONSTARTUP \
            "$interpreter" -I -c \
            'import sys; print("%d.%d.%d" % sys.version_info[:3]); print(sys.prefix)'
    )" || die "the staged interpreter at '$interpreter' does not run"

    version="$(printf '%s\n' "$report" | sed -n 1p)"
    prefix="$(printf '%s\n' "$report" | sed -n 2p)"

    staged_root="$(cd -- "$runtime_dir" && pwd -P)"
    resolved_prefix="$(cd -- "$prefix" 2>/dev/null && pwd -P || printf '%s' "$prefix")"
    case "$resolved_prefix" in
        "$staged_root" | "$staged_root"/*) ;;
        *)
            die "the staged interpreter reports sys.prefix '$prefix', outside '$staged_root'.
  That runtime is not relocatable and would resolve back into a system prefix,
  which is exactly the system-wide dependency bundling exists to remove."
            ;;
    esac

    note "staged CPython $version at $runtime_dir (sys.prefix $prefix)"
else
    # Cross-staging a Windows runtime from a Unix build host: the interpreter
    # cannot be executed here, so it is not claimed to have been validated.
    note "staged a Windows CPython at $runtime_dir (not probed: it cannot run on this host)"
fi

staged_ok=1
