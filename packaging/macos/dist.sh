#!/usr/bin/env bash
#
# Turn a built CriKey.app into something distributable: a zip, a dmg, or both.
#
# WHY BOTH FORMATS
#
# They are not interchangeable.
#
# A zip is what `xcrun notarytool` accepts as a submission and what an
# auto-updater downloads: small, streamable, and produced by `ditto -c -k`,
# which is the only archiver that preserves the symlinks and extended
# attributes a code signature is computed over. A plain `zip -r` silently
# flattens symlinks and produces a bundle whose signature no longer verifies.
#
# A dmg is what a human downloads. It carries the drag-to-Applications gesture
# users expect, and unlike a zip it can hold a stapled notary ticket of its own,
# so the mounted volume passes Gatekeeper even if the enclosed bundle is copied
# somewhere unusual.
#
# ORDER MATTERS
#
# The distributable is built from the *stapled* bundle, never before it. A
# ticket stapled to CriKey.app after the zip was made is not in the zip, and the
# download the user gets is refused offline. The sequence is always:
#
#   build.sh  ->  notarize.sh --path CriKey.app  ->  dist.sh --app CriKey.app
#
# and, when a dmg is produced, the dmg is itself notarized and stapled
# afterwards:
#
#   dist.sh --app CriKey.app --dmg  ->  notarize.sh --path CriKey.dmg
#
# This script checks that the bundle it was handed is stapled and refuses
# otherwise, because that mistake is invisible until a user reports it.
#
# WHERE THIS MUST RUN
#
# A real macOS host: `ditto`, `hdiutil` and `xcrun stapler` are macOS-only.
#
# USAGE
#   packaging/macos/dist.sh --app target/packaging/macos/CriKey.app
#   packaging/macos/dist.sh --app CriKey.app --dmg
#   packaging/macos/dist.sh --app CriKey.app --dmg --no-zip
#   packaging/macos/dist.sh --help
#
# OPTIONS
#   --app PATH        The notarized and stapled CriKey.app. Required.
#   --output DIR      Where the artefacts are written. Default: the directory
#                     holding the bundle.
#   --version VERSION Appended to the artefact names. Default: the
#                     CFBundleShortVersionString read back out of the bundle.
#   --dmg             Also produce a .dmg.
#   --no-zip          Skip the .zip. Only meaningful together with --dmg.
#   --allow-unstapled Proceed even though the bundle carries no notary ticket.
#                     For local testing. The result is not distributable and
#                     the script says so on every run.
#   --help            This text.

set -euo pipefail

readonly PROGRAM="crikey-packaging(macos/dist)"

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
    sed -n '3,58p' "${BASH_SOURCE[0]}" | sed 's/^#\{0,1\} \{0,1\}//'
}

app=""
output=""
version=""
want_dmg="no"
want_zip="yes"
allow_unstapled="no"

while [ "$#" -gt 0 ]; do
    case "$1" in
    --app)
        [ "$#" -ge 2 ] || die "--app needs a path"
        app="$2"
        shift 2
        ;;
    --output)
        [ "$#" -ge 2 ] || die "--output needs a directory"
        output="$2"
        shift 2
        ;;
    --version)
        [ "$#" -ge 2 ] || die "--version needs a value"
        version="$2"
        shift 2
        ;;
    --dmg)
        want_dmg="yes"
        shift
        ;;
    --no-zip)
        want_zip="no"
        shift
        ;;
    --allow-unstapled)
        allow_unstapled="yes"
        shift
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

[ -n "${app}" ] || die "--app is required (try --help)"
[ -d "${app}" ] || die "'${app}' is not a bundle directory"
[ "${want_zip}" = "yes" ] || [ "${want_dmg}" = "yes" ] ||
    die "--no-zip without --dmg would produce nothing"

require_tool ditto "macOS"
require_tool plutil "the Xcode command line tools"
[ "${want_dmg}" = "no" ] || require_tool hdiutil "macOS"

if [ -z "${output}" ]; then
    output="$(cd -- "$(dirname -- "${app}")" && pwd)"
fi
mkdir -p -- "${output}"

if [ -z "${version}" ]; then
    # Read back from the bundle rather than from Cargo.toml: the artefact name
    # should describe the artefact, and a repository that has moved on since
    # the bundle was built would otherwise mislabel it.
    version="$(plutil -extract CFBundleShortVersionString raw -o - "${app}/Contents/Info.plist")" ||
        die "could not read CFBundleShortVersionString from ${app}/Contents/Info.plist"
fi

if command -v xcrun >/dev/null 2>&1 && xcrun stapler validate "${app}" >/dev/null 2>&1; then
    note "bundle carries a stapled notary ticket"
elif [ "${allow_unstapled}" = "yes" ]; then
    note "WARNING: ${app} has no stapled notary ticket; the artefacts below are for local testing and Gatekeeper will refuse them after download"
else
    die "'${app}' has no stapled notary ticket. Run packaging/macos/notarize.sh --path '${app}' first, or pass --allow-unstapled to build a non-distributable artefact anyway."
fi

readonly BASE="CriKey-${version}"

if [ "${want_zip}" = "yes" ]; then
    zip_path="${output}/${BASE}.zip"
    note "writing ${zip_path}"
    rm -f -- "${zip_path}"
    # --keepParent so the archive expands to CriKey.app and not to its
    # contents; --sequesterRsrc and the default attribute handling are what
    # keep the signature verifiable after a round trip.
    ditto -c -k --sequesterRsrc --keepParent "${app}" "${zip_path}"
fi

if [ "${want_dmg}" = "yes" ]; then
    dmg_path="${output}/${BASE}.dmg"
    note "writing ${dmg_path}"
    rm -f -- "${dmg_path}"

    # A staging directory rather than `hdiutil create -srcfolder` over the
    # bundle's own parent: the output directory also holds the zip and any
    # previous dmg, and none of that belongs on the volume.
    staging="$(mktemp -d)"
    trap 'rm -rf -- "${staging}"' EXIT
    ditto "${app}" "${staging}/$(basename -- "${app}")"
    # The customary drag target. A symlink, so it costs nothing and always
    # points at the real folder on the user's machine.
    ln -s /Applications "${staging}/Applications"

    # UDZO is the compressed read-only format; a read-write dmg would let the
    # contents be edited after signing.
    hdiutil create \
        -volname "CriKey ${version}" \
        -srcfolder "${staging}" \
        -ov \
        -format UDZO \
        "${dmg_path}"

    note "a dmg needs its own notarization: packaging/macos/notarize.sh --path '${dmg_path}'"
fi

note "done"
