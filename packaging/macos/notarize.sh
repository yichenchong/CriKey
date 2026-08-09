#!/usr/bin/env bash
#
# Submit a signed CriKey artefact to Apple's notary service and staple the
# ticket to it.
#
# WHY THIS IS SEPARATE FROM build.sh
#
# Notarization is a network round trip against an Apple service. It routinely
# takes minutes, it fails for reasons that have nothing to do with the bundle
# (an expired app-specific password, a service outage, a rate limit), and when
# it does fail the correct response is to run it again -- not to reassemble and
# re-sign a bundle that was already correct. Keeping it in its own script makes
# the retry free and keeps a slow, credentialed, online step out of the fast,
# offline one.
#
# WHAT NOTARIZATION IS FOR
#
# A Developer ID signature says who built the artefact. It does not, on its
# own, get past Gatekeeper on a machine that downloaded the artefact from the
# internet: that requires a notary ticket. Stapling attaches the ticket to the
# artefact so the check succeeds even offline. An artefact that is signed but
# not stapled works on the build machine and is refused on the user's, which is
# the single most common way a macOS release is discovered to be broken after
# it ships.
#
# WHERE THIS MUST RUN
#
# A real macOS host with the Xcode command line tools: `xcrun notarytool` and
# `xcrun stapler` are Apple binaries with no cross-platform equivalent. This
# script names the missing tool and exits rather than approximating one.
#
# USAGE
#   packaging/macos/notarize.sh --path target/packaging/macos/CriKey.app
#   packaging/macos/notarize.sh --path target/packaging/macos/CriKey.dmg
#   packaging/macos/notarize.sh --path CriKey.app --keychain-profile crikey-notary
#   packaging/macos/notarize.sh --help
#
# OPTIONS
#   --path PATH             The signed .app or .dmg to notarize. Required.
#   --keychain-profile NAME A profile previously stored with
#                           `xcrun notarytool store-credentials`. Defaults to
#                           $CRIKEY_NOTARY_PROFILE. Preferred on a developer
#                           machine: the password never appears in a process
#                           argument list or a shell history.
#   --apple-id ID           Apple ID for password authentication. Defaults to
#                           $CRIKEY_NOTARY_APPLE_ID.
#   --team-id TEAM          Developer team identifier. Defaults to
#                           $CRIKEY_NOTARY_TEAM_ID.
#   --timeout DURATION      How long to wait for the verdict. Default 30m.
#   --help                  This text.
#
# CREDENTIALS
#
# Either a keychain profile, or all three of Apple ID, team id and the
# app-specific password in $CRIKEY_NOTARY_PASSWORD. The password is read from
# the environment only: it is never a command-line option, because arguments
# are visible to every process on the machine through `ps`. Nothing
# credential-shaped is written to the log, and nothing credential-shaped is
# checked into this repository.
#
# WHAT THIS SCRIPT WILL NOT DO
#
# It does not disable, skip or work around any verification step. If the notary
# service rejects the submission the log is fetched and printed and the script
# fails; there is no flag that turns a rejection into a success, and
# `spctl --master-disable` and its relatives appear nowhere in this repository.

set -euo pipefail

readonly PROGRAM="crikey-packaging(macos/notarize)"

die() {
    printf '%s: error: %s\n' "${PROGRAM}" "$*" >&2
    exit 1
}

note() {
    printf '%s: %s\n' "${PROGRAM}" "$*"
}

require_tool() {
    local tool="$1"
    local provided_by="$2"
    command -v "${tool}" >/dev/null 2>&1 ||
        die "required tool '${tool}' was not found on PATH (provided by ${provided_by}). This script only runs on macOS."
}

usage() {
    sed -n '3,66p' "${BASH_SOURCE[0]}" | sed 's/^#\{0,1\} \{0,1\}//'
}

path=""
keychain_profile="${CRIKEY_NOTARY_PROFILE:-}"
apple_id="${CRIKEY_NOTARY_APPLE_ID:-}"
team_id="${CRIKEY_NOTARY_TEAM_ID:-}"
timeout="30m"

while [ "$#" -gt 0 ]; do
    case "$1" in
    --path)
        [ "$#" -ge 2 ] || die "--path needs a value"
        path="$2"
        shift 2
        ;;
    --keychain-profile)
        [ "$#" -ge 2 ] || die "--keychain-profile needs a value"
        keychain_profile="$2"
        shift 2
        ;;
    --apple-id)
        [ "$#" -ge 2 ] || die "--apple-id needs a value"
        apple_id="$2"
        shift 2
        ;;
    --team-id)
        [ "$#" -ge 2 ] || die "--team-id needs a value"
        team_id="$2"
        shift 2
        ;;
    --timeout)
        [ "$#" -ge 2 ] || die "--timeout needs a value"
        timeout="$2"
        shift 2
        ;;
    --help | -h)
        usage
        exit 0
        ;;
    *)
        die "unknown argument '$1' (try --help)"
        ;;
    esac
done

[ -n "${path}" ] || die "--path is required (try --help)"
[ -e "${path}" ] || die "'${path}' does not exist"

require_tool xcrun "the Xcode command line tools"
require_tool ditto "macOS"
if [ "${path##*.}" = "dmg" ]; then
    require_tool hdiutil "macOS"
fi
require_tool codesign "the Xcode command line tools"
xcrun --find notarytool >/dev/null 2>&1 ||
    die "'notarytool' was not found; it needs Xcode 13 or newer command line tools"
xcrun --find stapler >/dev/null 2>&1 ||
    die "'stapler' was not found; it needs the Xcode command line tools"

# Build the credential arguments once. An array, not a string, so a team name
# or Apple ID containing a space cannot be re-split into two arguments.
credentials=()
if [ -n "${keychain_profile}" ]; then
    credentials=(--keychain-profile "${keychain_profile}")
    note "authenticating with keychain profile '${keychain_profile}'"
elif [ -n "${apple_id}" ] && [ -n "${team_id}" ] && [ -n "${CRIKEY_NOTARY_PASSWORD:-}" ]; then
    credentials=(--apple-id "${apple_id}" --team-id "${team_id}" --password "${CRIKEY_NOTARY_PASSWORD}")
    note "authenticating as ${apple_id} (team ${team_id})"
else
    die "no notary credentials: set CRIKEY_NOTARY_PROFILE (or --keychain-profile), or set all of CRIKEY_NOTARY_APPLE_ID, CRIKEY_NOTARY_TEAM_ID and CRIKEY_NOTARY_PASSWORD"
fi

case "${path}" in
*.app)
    kind="app"
    ;;
*.dmg)
    kind="dmg"
    ;;
*.zip)
    # A zip is a transport container. The notary service accepts one, but
    # `stapler` cannot write a ticket into it, so the ticket would live only in
    # Apple's database and an offline first launch would still be refused.
    # Staple the bundle, then create the zip from the stapled bundle.
    die "a .zip cannot be stapled: notarize the .app itself, then rebuild the zip from the stapled bundle with packaging/macos/dist.sh"
    ;;
*)
    die "'${path}' is neither a .app nor a .dmg"
    ;;
esac

# A submission that was never signed, or was signed without the hardened
# runtime, is rejected by the service several minutes later with a log the
# caller then has to go and read. Checking an app locally turns that into an
# immediate and specific failure. A DMG is a disk-image container, not a code
# object, so `codesign --verify` is invalid for it; its nested app was checked
# before the image was created.
if [ "${kind}" = "app" ]; then
    note "checking the local signature before spending a submission on it"
    codesign --verify --strict --verbose=2 "${path}" ||
        die "'${path}' is not correctly signed; run packaging/macos/build.sh with an --identity first"
else
    note "checking the disk image before spending a submission on it"
    hdiutil imageinfo "${path}" >/dev/null ||
        die "'${path}' is not a valid disk image"
fi

submission="${path}"
scratch=""
if [ "${kind}" = "app" ]; then
    # notarytool takes an archive, not a bundle directory. `ditto -c -k
    # --keepParent` is the form Apple documents: it preserves the symlinks,
    # extended attributes and the enclosing CriKey.app directory that the
    # signature covers, none of which survive a plain `zip`.
    scratch="$(mktemp -d)"
    submission="${scratch}/$(basename -- "${path}").zip"
    note "archiving the bundle for submission"
    ditto -c -k --keepParent "${path}" "${submission}"
fi

cleanup() {
    [ -n "${scratch}" ] && rm -rf -- "${scratch}"
}
trap cleanup EXIT

note "submitting ${submission} (waiting up to ${timeout})"
if ! output="$(xcrun notarytool submit "${submission}" "${credentials[@]}" --wait --timeout "${timeout}" 2>&1)"; then
    printf '%s\n' "${output}" >&2
    # The verdict line names the submission id; fetching the log is the only
    # way to learn *which* nested binary was unsigned or which entitlement was
    # refused, so it is done here rather than left as an exercise.
    id="$(printf '%s\n' "${output}" | sed -n 's/^ *id: \([0-9a-fA-F-]*\)$/\1/p' | head -n 1)"
    if [ -n "${id}" ]; then
        note "fetching the notary log for submission ${id}"
        xcrun notarytool log "${id}" "${credentials[@]}" >&2 || true
    fi
    die "notarization failed"
fi
printf '%s\n' "${output}"

printf '%s\n' "${output}" | grep -q "status: Accepted" ||
    die "the notary service did not accept the submission"

note "stapling the ticket to ${path}"
xcrun stapler staple "${path}"

# `stapler validate` reads the ticket back out of the artefact. Without it a
# stapling that silently no-ops -- which is what happens when the ticket has
# not propagated yet -- ships as if it had worked.
note "validating the stapled ticket"
xcrun stapler validate "${path}"

note "notarized and stapled: ${path}"
if [ "${kind}" = "app" ]; then
    note "next: packaging/macos/dist.sh --app '${path}' to build the distributable zip or dmg from the stapled bundle"
fi
