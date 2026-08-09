#!/usr/bin/env bash
#
# Materialises the referenced real-plugin corpus and runs the legacy
# conformance suite against every package in it (spec 27.4).
#
# The corpus deliberately references packages by URL and pinned revision
# instead of vendoring them, so no third-party licence enters this repository.
# The consequence is that the published classifications in corpus.toml have,
# until now, rested on static source audits: honest, and clearly labelled as
# such in the corpus header, but not an execution record. This script closes
# that gap by fetching each pinned revision into an ignored working directory
# and running the real conformance command against it.
#
# What it deliberately does NOT do is edit corpus.toml. A classification in
# that file carries the evidence that produced it, and evidence is written by
# whoever read the run, not by the runner. This prints the evidence.
#
# Two honesty rules are enforced here rather than left to the reader:
#
#   * A package whose fetched tree does not match its pinned revision is
#     refused outright. A classification is only reproducible against the exact
#     tree it describes.
#   * A Windows-only package cannot yield a portability verdict on this host.
#     Its run is reported as environment-limited, never as a compatibility
#     result, because reporting it as one is precisely the misrepresentation
#     acceptance criterion 31.31 forbids.

set -euo pipefail

readonly CORPUS="compatibility/real-plugin-corpus/corpus.toml"
readonly DEFAULT_WORKDIR="compatibility/real-plugin-corpus/packages"

usage() {
    cat <<'USAGE'
run-real-plugin-corpus.sh - fetch and execute the real-plugin corpus (spec 27.4)

USAGE:
    scripts/run-real-plugin-corpus.sh [OPTIONS]

OPTIONS:
    --workdir DIR   Where packages are checked out.
                    Default: compatibility/real-plugin-corpus/packages
                    (already ignored by .gitignore; never committed)
    --package ID    Run one corpus entry instead of all of them.
    --offline       Never reach the network. A package that is not already
                    checked out is reported as skipped rather than fetched.
    --results DIR   Where per-package output is written.
                    Default: <workdir>/../results
    -h, --help      This text.

EXIT STATUS:
    0  every package that could run, ran
    1  usage error, or the corpus could not be read
    2  at least one package could not be fetched or failed its revision check

Requires: git, python3 (>= 3.11, for tomllib), and a built `crikey` binary,
which it builds with `cargo build -j 1 -p crikey-cli` if one is not present.
USAGE
}

workdir="$DEFAULT_WORKDIR"
results=""
only=""
offline=0

while [ $# -gt 0 ]; do
    case "$1" in
        --workdir) workdir="${2:?--workdir needs a directory}"; shift 2 ;;
        --results) results="${2:?--results needs a directory}"; shift 2 ;;
        --package) only="${2:?--package needs a corpus id}"; shift 2 ;;
        --offline) offline=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown option: %s\n\n' "$1" >&2; usage >&2; exit 1 ;;
    esac
done

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

[ -f "$CORPUS" ] || { printf 'cannot read %s from %s\n' "$CORPUS" "$root" >&2; exit 1; }
command -v git >/dev/null || { printf 'git is required and is not installed\n' >&2; exit 1; }
command -v python3 >/dev/null || { printf 'python3 is required and is not installed\n' >&2; exit 1; }

[ -n "$results" ] || results="$(dirname "$workdir")/results"
mkdir -p "$workdir" "$results"

# The corpus is parsed rather than pattern-matched: it is also read by Rust
# with strict key handling, and two readers that disagree about the format is
# how a corpus quietly stops describing what it claims to. tomllib is stdlib
# from 3.11 and the repository already requires a modern interpreter.
entries="$(python3 - "$CORPUS" <<'PARSE'
import sys, tomllib

with open(sys.argv[1], "rb") as handle:
    corpus = tomllib.load(handle)

for package in corpus.get("package", []):
    print("\t".join([
        package["id"],
        package["source"],
        package["revision"],
        package.get("classification", "untested"),
    ]))
PARSE
)"

[ -n "$entries" ] || { printf 'corpus %s references no packages\n' "$CORPUS" >&2; exit 1; }

binary="target/debug/crikey"
if [ ! -x "$binary" ]; then
    printf '== building crikey-cli (no %s present)\n' "$binary"
    cargo build -j 1 -p crikey-cli
fi

# A Keypirinha package is the directory holding the plugin modules. These
# repositories overwhelmingly keep that in src/; a few are flat. Guessing wrong
# would report "no plugin found" as though it were a compatibility finding, so
# the choice is made by looking rather than assuming, and is printed with the
# result.
package_dir() {
    local checkout="$1"
    if compgen -G "$checkout/src/*.py" >/dev/null; then
        printf '%s/src' "$checkout"
    elif compgen -G "$checkout/*.py" >/dev/null; then
        printf '%s' "$checkout"
    else
        return 1
    fi
}

failures=0
ran=0
skipped=0

printf '== corpus run: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
printf '== workdir: %s\n== results: %s\n\n' "$workdir" "$results"

while IFS=$'\t' read -r id source revision declared; do
    [ -n "$id" ] || continue
    if [ -n "$only" ] && [ "$only" != "$id" ]; then
        continue
    fi

    checkout="$workdir/$id"
    printf -- '--- %s\n    source:   %s\n    revision: %s\n    declared: %s\n' \
        "$id" "$source" "$revision" "$declared"

    if [ ! -d "$checkout/.git" ]; then
        if [ "$offline" -eq 1 ]; then
            printf '    result:   SKIPPED (not checked out, --offline)\n\n'
            skipped=$((skipped + 1))
            continue
        fi
        rm -rf "$checkout"
        mkdir -p "$checkout"
        git -C "$checkout" init --quiet
        git -C "$checkout" remote add origin "$source"
    fi

    if [ "$offline" -eq 0 ]; then
        # Fetching the pinned revision directly keeps the checkout to one
        # commit and makes the revision check below a condition to satisfy
        # rather than a search through history.
        if ! git -C "$checkout" fetch --quiet --depth 1 origin "$revision" 2>/dev/null; then
            printf '    result:   FETCH FAILED (%s at %s)\n\n' "$source" "$revision"
            failures=$((failures + 1))
            continue
        fi
        git -C "$checkout" checkout --quiet FETCH_HEAD
    fi

    head="$(git -C "$checkout" rev-parse HEAD)"
    if [ "$head" != "$revision" ]; then
        printf '    result:   REVISION MISMATCH (have %s, corpus pins %s)\n\n' "$head" "$revision"
        failures=$((failures + 1))
        continue
    fi

    if ! target="$(package_dir "$checkout")"; then
        printf '    result:   NO PACKAGE LAYOUT FOUND (no *.py in root or src/)\n'
        printf '              This repository may hold several packages; each needs\n'
        printf '              its own corpus entry before either can be run.\n\n'
        skipped=$((skipped + 1))
        continue
    fi
    printf '    package:  %s\n' "$target"

    log="$results/$id.txt"
    status=0
    "$binary" dev test-legacy-compat --package "$target" >"$log" 2>&1 || status=$?
    ran=$((ran + 1))

    verdict="$(sed -n 's/^verdict=//p' "$log" | head -1)"
    [ -n "$verdict" ] || verdict="(no verdict line; see $log)"

    case "$declared" in
        windows-only-but-compatible)
            printf '    result:   ENVIRONMENT-LIMITED (exit %s) verdict=%s\n' "$status" "$verdict"
            printf '              Windows-only by dependency. This host cannot produce a\n'
            printf '              portability verdict for it; the run is evidence about\n'
            printf '              the layer, not about the package (acceptance 31.31).\n'
            ;;
        *)
            printf '    result:   exit %s verdict=%s\n' "$status" "$verdict"
            ;;
    esac
    printf '    log:      %s\n\n' "$log"
done <<<"$entries"

printf '== ran %d, skipped %d, failed to obtain %d\n' "$ran" "$skipped" "$failures"
printf '== corpus.toml is NOT modified by this script. Read the logs above and\n'
printf '   write each classification with the evidence that produced it.\n'

[ "$failures" -eq 0 ] || exit 2
