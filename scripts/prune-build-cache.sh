#!/usr/bin/env bash
#
# Reclaim disk from Cargo's build directory.
#
# WHY THIS EXISTS
#
# Cargo never removes old build output. Every time a source file changes,
# rustc writes a fresh set of artefacts under a new content hash and leaves the
# previous set behind. During ordinary development that is invisible; during a
# long editing session it is not. One session in this repository left 306 test
# executables in `target/debug/deps` for a workspace that only has about 98,
# including eight stale copies of the main binary totalling 459 MB, and grew
# `target/debug` from 2.0 GB to 5.6 GB. An earlier session filled the disk
# outright.
#
# There is no supported way to make Cargo garbage-collect a project's `target`
# directory on the stable toolchain, so this script does it.
#
# WHAT IT DOES
#
# If the build directory is larger than the threshold, it deletes the whole
# development profile directory (`target/debug`). That is deliberately blunt:
# deleting individual files by age risks removing something Cargo still
# considers current, whereas deleting the profile wholesale is always safe.
# The only cost is one rebuild.
#
# Deliberately NOT touched:
#   * `target/release`
#   * cross-compilation directories such as `target/x86_64-pc-windows-msvc`
#   * `target/native-conformance`, the out-of-tree plugin fixture
# Those are expensive to regenerate, are not rewritten on every edit, and so do
# not accumulate the same way.
#
# USAGE
#   scripts/prune-build-cache.sh            # prune if over the threshold
#   scripts/prune-build-cache.sh --force    # prune regardless of size
#   scripts/prune-build-cache.sh --dry-run  # report only, change nothing
#
# THRESHOLD_GB may be set in the environment to override the default.
#
# The threshold is measured against `target/debug` ALONE, not the whole build
# directory. That distinction matters: the cross-compilation directories,
# release output and the out-of-tree fixture together occupy several gigabytes
# on their own, so a threshold applied to the whole of `target` would be
# permanently exceeded and would delete the development profile after every
# single rebuild. Measuring the directory we actually delete gives the
# behaviour we want, and needs no hysteresis: a healthy full build is about
# 2 GB, so the default only fires once stale artefacts have roughly doubled it.

set -euo pipefail

THRESHOLD_GB="${THRESHOLD_GB:-5}"
FORCE=0
DRY_RUN=0

for argument in "$@"; do
	case "$argument" in
	--force) FORCE=1 ;;
	--dry-run) DRY_RUN=1 ;;
	-h | --help)
		sed -n '3,45p' "$0" | sed 's/^# \{0,1\}//'
		exit 0
		;;
	*)
		printf 'prune-build-cache: unknown argument: %s\n' "$argument" >&2
		exit 2
		;;
	esac
done

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_directory="${CARGO_TARGET_DIR:-$repository_root/target}"

if [[ ! -d "$target_directory" ]]; then
	printf 'prune-build-cache: nothing to do, no build directory at %s\n' "$target_directory"
	exit 0
fi

# Refuse to run while a build is in progress: deleting artefacts from underneath
# a live rustc produces confusing failures that look like source errors.
if pgrep -u "$(id -u)" -x cargo >/dev/null 2>&1 || pgrep -u "$(id -u)" -x rustc >/dev/null 2>&1; then
	printf 'prune-build-cache: a build is running, leaving it alone\n'
	exit 0
fi

development_profile="$target_directory/debug"
if [[ ! -d "$development_profile" ]]; then
	printf 'prune-build-cache: nothing to do, no development profile at %s\n' \
		"$development_profile"
	exit 0
fi

size_in_kilobytes="$(du -sk "$development_profile" | cut -f1)"
size_in_megabytes=$((size_in_kilobytes / 1024))
threshold_in_kilobytes=$((THRESHOLD_GB * 1048576))

printf 'prune-build-cache: %s is %s MB, threshold %s GB\n' \
	"$development_profile" "$size_in_megabytes" "$THRESHOLD_GB"

if [[ "$FORCE" -eq 0 && "$size_in_kilobytes" -le "$threshold_in_kilobytes" ]]; then
	printf 'prune-build-cache: under threshold, nothing to do\n'
	exit 0
fi

reclaimable_megabytes="$size_in_megabytes"

if [[ "$DRY_RUN" -eq 1 ]]; then
	printf 'prune-build-cache: would remove %s, reclaiming %s MB\n' \
		"$development_profile" "$reclaimable_megabytes"
	exit 0
fi

rm -rf "$development_profile"
printf 'prune-build-cache: removed %s, reclaimed %s MB\n' \
	"$development_profile" "$reclaimable_megabytes"
