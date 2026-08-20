#!/usr/bin/env bash
#
# Clear the development profile, then run a Cargo command in it.
#
# WHY THIS EXISTS
#
# `scripts/prune-build-cache.sh` reclaims disk when asked, and the user timer in
# `docs/development.md` asks on a schedule. Neither is tied to a build, so
# between two runs of the timer a heavy editing session still accumulates
# superseded artefacts. This wrapper is the other half: it makes clearing part
# of the build itself, for the case where you want a rebuild to leave nothing
# behind rather than to be fast.
#
# It is deliberately a separate entry point rather than anything that changes
# what `cargo` does. Cargo offers no post-build hook on the stable toolchain —
# aliases in `.cargo/config.toml` may only name Cargo subcommands, and a
# `build.rs` cannot delete the profile the compiler invoking it is writing to —
# so a wrapper is the only honest mechanism. Keeping it separate also keeps the
# cost opt-in: plain `cargo build`, `cargo test` and `cargo clippy` are entirely
# unaffected by this file existing.
#
# THE COST, STATED PLAINLY
#
# Every invocation is a cold build of everything the command needs, because the
# artefacts a previous build left are exactly the incremental cache this deletes
# first. On this workspace that is minutes, not seconds. Use it when disk
# matters more than latency — before stepping away, or when a session has been
# churning through unrelated feature sets — and use plain `cargo` the rest of
# the time.
#
# USAGE
#   scripts/dev-rebuild.sh                     # prune, then `cargo build`
#   scripts/dev-rebuild.sh test --workspace    # prune, then `cargo test --workspace`
#   scripts/dev-rebuild.sh clippy --all-targets
#   scripts/dev-rebuild.sh --help              # this text
#
# Every argument is forwarded to Cargo unchanged. With no arguments the command
# is `build`, because that is the one this exists to wrap.
#
# Pruning runs with --force, so it does not consult the size threshold: an
# explicit request to clear the profile is not a request to think about how big
# it is. If another build holds Cargo's lock the prune declines and says so,
# then the requested command runs anyway against the artefacts that survived;
# that is the safe order, because deleting output from under a live compiler
# produces failures that look like source errors.

set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ "$#" -eq 1 && ( "$1" == "-h" || "$1" == "--help" ) ]]; then
	sed -n '3,45p' "$0" | sed 's/^# \{0,1\}//'
	exit 0
fi

# Cargo is normally on PATH, but this script is also a reasonable thing to put
# in a timer, an editor task or a hook, and those often run with a PATH that
# rustup's shim directory never reached. Falling back to the standard location
# turns a confusing "command not found" into a working invocation.
cargo_command="cargo"
if ! command -v cargo >/dev/null 2>&1; then
	if [[ -x "${CARGO_HOME:-$HOME/.cargo}/bin/cargo" ]]; then
		cargo_command="${CARGO_HOME:-$HOME/.cargo}/bin/cargo"
	else
		printf 'dev-rebuild: cargo is not on PATH and %s/bin/cargo is not executable\n' \
			"${CARGO_HOME:-$HOME/.cargo}" >&2
		exit 127
	fi
fi

# The prune stage reports on stderr, not stdout: a forwarded Cargo command may
# have machine-readable output (`metadata`, `--message-format json`) and a
# caller piping that is entitled to receive it unmixed with our own progress.
"$repository_root/scripts/prune-build-cache.sh" --force >&2

# `exec` so the exit status is Cargo's own and a Ctrl-C reaches the compiler
# rather than this wrapper.
if [[ "$#" -eq 0 ]]; then
	exec "$cargo_command" build
fi
exec "$cargo_command" "$@"
