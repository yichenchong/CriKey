#!/usr/bin/env bash
#
# Assemble CriKey.app.
#
# WHY THIS EXISTS
#
# A macOS build of CriKey is not a binary. `sdk_root()` in crikey-python-host
# and `shim_root()` in crikey-legacy-compat both look for a directory *beside
# the running executable* -- `modern-sdk` and `legacy-shim` respectively -- and
# fall back to a repository-relative development path that does not exist on a
# user's machine. Ship `target/release/crikey` on its own and modern Python
# plugins and every legacy Keypirinha package stop loading, with no error until
# a plugin is actually invoked. Assembling the bundle is therefore the only
# supported way to produce something that behaves like the program under test.
#
# This script does the assembling and the signing. It does not notarize; that
# is `notarize.sh`, because notarization is a network round trip against an
# Apple service that can take minutes and must be retryable on its own.
#
# WHAT IT PRODUCES
#
#   <output>/CriKey.app
#     Contents/Info.plist              from packaging/macos/Info.plist
#     Contents/MacOS/crikey-launcher   the GUI entry point; CFBundleExecutable
#     Contents/MacOS/crikey            the command line, beside it
#     Contents/MacOS/crikey-wasm-host  the supervised WASM runtime host
#     Contents/MacOS/crikey-cabi-host  the supervised C-ABI runtime host
#     Contents/MacOS/modern-sdk/       sdk/python, minus __pycache__
#     Contents/MacOS/legacy-shim/      crates/crikey-legacy-compat/python
#     Contents/MacOS/python-runtime/   only with --python-runtime
#     Contents/Resources/LICENSE       Apache-2.0, spec 14.13
#     Contents/Resources/NOTICE.md     attribution and non-affiliation
#     Contents/Resources/CriKey.icns   only with --icon
#
# The payload directories and runtime host executables live under Contents/MacOS
# rather than Contents/Resources because every resolver looks beside the
# running executable. The executable's parent inside a bundle is Contents/MacOS.
#
# WHERE THIS MUST RUN
#
# A real macOS host. `codesign`, `lipo` and `plutil` ship with the Xcode command
# line tools and exist nowhere else; there is no cross-platform substitute and
# this script does not pretend there is -- it names the missing tool and exits.
#
# USAGE
#   packaging/macos/build.sh --identity "Developer ID Application: Example (TEAMID)"
#   packaging/macos/build.sh --binary a/crikey --binary b/crikey --identity ...
#   packaging/macos/build.sh --unsigned            # local testing only
#   packaging/macos/build.sh --help
#
# OPTIONS
#   --binary PATH        A `crikey` executable to bundle. May be repeated; two
#                        or more slices are combined with `lipo -create` into a
#                        universal binary. Default: target/release/crikey.
#   --launcher-binary PATH
#                        A `crikey-launcher` executable to bundle, with the
#                        same repeat-for-universal rule. This is the bundle's
#                        main executable: Launch Services starts it with no
#                        arguments on a double click, which bare `crikey`
#                        answers with usage text.
#                        Default: target/release/crikey-launcher.
#   --version VERSION    Substituted for __CRIKEY_VERSION__ in Info.plist.
#                        Default: the workspace version in Cargo.toml.
#   --output DIR         Where CriKey.app is written.
#                        Default: target/packaging/macos.
#   --icon FILE.icns     Bundle icon. Without it the bundle ships no icon and
#                        no CFBundleIconFile key, rather than a dangling one.
#   --wasm-host-binary PATH
#                        A `crikey-wasm-host` slice, repeated for each
#                        architecture. It is staged beside the launcher because
#                        the WASM provider never searches PATH.
#                        Default: target/release/crikey-wasm-host.
#   --cabi-host-binary PATH
#                        A `crikey-cabi-host` slice, repeated for each
#                        architecture. It is staged beside the launcher because
#                        the C-ABI provider never searches PATH.
#                        Default: target/release/crikey-cabi-host.
#   --identity IDENTITY  Signing identity. Defaults to $CRIKEY_CODESIGN_IDENTITY.
#   --python-runtime ARCHIVE
#                        Stage a python-build-standalone archive as
#                        Contents/MacOS/python-runtime via
#                        packaging/stage-python-runtime.sh. Optional: without
#                        it the bundle carries no interpreter and CriKey falls
#                        back to whatever python3 discovery finds on PATH.
#   --unsigned           Assemble without signing. The result cannot be
#                        notarized and Gatekeeper will refuse it on any machine
#                        but the one that built it. Never used for a release.
#   --help               This text.
#
# CODE SIGNING
#
# The identity comes from the environment or the command line and nothing else.
# No certificate, no p12, no password and no keychain is checked into this
# repository, and none ever should be. The signature is verified immediately
# after it is applied; there is no flag in this script that weakens, skips or
# bypasses verification, and adding one would defeat the point of signing.

set -euo pipefail

readonly PROGRAM="crikey-packaging(macos/build)"

# The script may be invoked through any relative path, so the repository root
# is derived from the script's own location rather than the caller's directory.
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
readonly ROOT_DIR="${SCRIPT_DIR}/../.."

die() {
    printf '%s: error: %s\n' "${PROGRAM}" "$*" >&2
    exit 1
}

note() {
    printf '%s: %s\n' "${PROGRAM}" "$*"
}

# A missing tool is reported by name, with what provides it. "command not
# found" from a pipeline half way through assembly is not a diagnosis.
require_tool() {
    local tool="$1"
    local provided_by="$2"
    command -v "${tool}" >/dev/null 2>&1 ||
        die "required tool '${tool}' was not found on PATH (provided by ${provided_by}). This script only runs on macOS."
}

usage() {
    # The header comment is the documentation; printing it back keeps --help
    # and the file from drifting apart.
    sed -n '3,89p' "${BASH_SOURCE[0]}" | sed 's/^#\{0,1\} \{0,1\}//'
}

binaries=()
launcher_binaries=()
wasm_host_binaries=()
cabi_host_binaries=()
version=""
output=""
icon=""
identity="${CRIKEY_CODESIGN_IDENTITY:-}"
python_runtime=""
unsigned="no"

while [ "$#" -gt 0 ]; do
    case "$1" in
    --binary)
        [ "$#" -ge 2 ] || die "--binary needs a path"
        binaries+=("$2")
        shift 2
        ;;
    --launcher-binary)
        [ "$#" -ge 2 ] || die "--launcher-binary needs a path"
        launcher_binaries+=("$2")
        shift 2
        ;;
    --wasm-host-binary)
        [ "$#" -ge 2 ] || die "--wasm-host-binary needs a path"
        wasm_host_binaries+=("$2")
        shift 2
        ;;
    --cabi-host-binary)
        [ "$#" -ge 2 ] || die "--cabi-host-binary needs a path"
        cabi_host_binaries+=("$2")
        shift 2
        ;;
    --version)
        [ "$#" -ge 2 ] || die "--version needs a value"
        version="$2"
        shift 2
        ;;
    --output)
        [ "$#" -ge 2 ] || die "--output needs a directory"
        output="$2"
        shift 2
        ;;
    --icon)
        [ "$#" -ge 2 ] || die "--icon needs a path to an .icns file"
        icon="$2"
        shift 2
        ;;
    --identity)
        [ "$#" -ge 2 ] || die "--identity needs a signing identity"
        identity="$2"
        shift 2
        ;;
    --python-runtime)
        [ "$#" -ge 2 ] || die "--python-runtime needs an archive path"
        python_runtime="$2"
        shift 2
        ;;
    --unsigned)
        unsigned="yes"
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

if [ "${#binaries[@]}" -eq 0 ]; then
    binaries=("${ROOT_DIR}/target/release/crikey")
fi

if [ "${#launcher_binaries[@]}" -eq 0 ]; then
    launcher_binaries=("${ROOT_DIR}/target/release/crikey-launcher")
fi
if [ "${#wasm_host_binaries[@]}" -eq 0 ]; then
    wasm_host_binaries=("${ROOT_DIR}/target/release/crikey-wasm-host")
fi

if [ "${#cabi_host_binaries[@]}" -eq 0 ]; then
    cabi_host_binaries=("${ROOT_DIR}/target/release/crikey-cabi-host")
fi

if [ -z "${version}" ]; then
    # The workspace version every crate inherits. Read out of the
    # [workspace.package] table rather than by running cargo, so packaging does
    # not require a Rust toolchain on the signing host.
    version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "${ROOT_DIR}/Cargo.toml" | head -n 1)"
    [ -n "${version}" ] || die "could not read the workspace version from ${ROOT_DIR}/Cargo.toml; pass --version"
fi

if [ -z "${output}" ]; then
    output="${ROOT_DIR}/target/packaging/macos"
fi

if [ "${unsigned}" = "no" ] && [ -z "${identity}" ]; then
    die "no signing identity: pass --identity or set CRIKEY_CODESIGN_IDENTITY, or pass --unsigned to build a bundle Gatekeeper will refuse"
fi

require_tool plutil "the Xcode command line tools"
require_tool ditto "macOS"
require_tool file "macOS"
if [ "${unsigned}" = "no" ]; then
    require_tool codesign "the Xcode command line tools"
fi
if [ "${#binaries[@]}" -gt 1 ] || [ "${#launcher_binaries[@]}" -gt 1 ] ||
    [ "${#wasm_host_binaries[@]}" -gt 1 ] || [ "${#cabi_host_binaries[@]}" -gt 1 ]; then
    require_tool lipo "the Xcode command line tools"
fi

for binary in "${binaries[@]}" "${launcher_binaries[@]}" \
    "${wasm_host_binaries[@]}" "${cabi_host_binaries[@]}"; do
    [ -f "${binary}" ] ||
        die "binary '${binary}' does not exist; build it with cargo build --release --package crikey-cli --package crikey-wasm-host --package crikey-cabi-host"
done

APP="${output}/CriKey.app"
readonly APP
readonly MACOS_DIR="${APP}/Contents/MacOS"
readonly RESOURCES_DIR="${APP}/Contents/Resources"

# A stale bundle merged with a new one is how a file nobody meant to ship gets
# shipped, and how a signature ends up covering a tree that no longer matches.
# Assembly always starts from nothing.
rm -rf -- "${APP}"
mkdir -p -- "${MACOS_DIR}" "${RESOURCES_DIR}"

note "assembling ${APP} (version ${version})"

# One or more architecture slices become one executable in the bundle.
install_universal() {
    local destination="$1"
    shift
    if [ "$#" -gt 1 ]; then
        note "combining $# architecture slices of $(basename "${destination}") with lipo"
        lipo -create -output "${destination}" "$@"
    else
        cp -- "$1" "${destination}"
    fi
    chmod 755 "${destination}"
}

# Both binaries, side by side in Contents/MacOS. `crikey-launcher` is
# CFBundleExecutable — the one Launch Services runs, with no arguments, when
# the bundle is double-clicked — and `crikey` is the command line, which a
# double click could never usefully reach because bare `crikey` prints usage.
# They must share a directory: `sdk_root()` and `shim_root()` resolve
# `modern-sdk`/`legacy-shim` beside the running executable, so whichever one
# started has to find the payload trees next to itself.
install_universal "${MACOS_DIR}/crikey-launcher" "${launcher_binaries[@]}"
install_universal "${MACOS_DIR}/crikey" "${binaries[@]}"
install_universal "${MACOS_DIR}/crikey-wasm-host" "${wasm_host_binaries[@]}"
install_universal "${MACOS_DIR}/crikey-cabi-host" "${cabi_host_binaries[@]}"

# Info.plist, with the version token replaced. Linted immediately: a plist that
# fails to parse produces a directory macOS refuses to treat as an application,
# and the user-visible symptom ("the application is damaged") points nowhere
# near this file.
sed "s/__CRIKEY_VERSION__/${version}/g" "${SCRIPT_DIR}/Info.plist" >"${APP}/Contents/Info.plist"
plutil -lint "${APP}/Contents/Info.plist" >/dev/null ||
    die "the generated Info.plist is not a valid property list"

# `__pycache__` holds bytecode compiled by whatever interpreter last ran in the
# source tree: stale by construction on a user's machine, and unsignable churn
# inside a notarized bundle.
copy_python_payload() {
    local source="$1"
    local destination="$2"
    [ -d "${source}" ] || die "payload directory '${source}' is missing"
    ditto --norsrc --noextattr "${source}" "${destination}"
    find "${destination}" -name '__pycache__' -type d -prune -exec rm -rf -- {} +
}

copy_python_payload "${ROOT_DIR}/sdk/python" "${MACOS_DIR}/modern-sdk"
[ -f "${MACOS_DIR}/modern-sdk/_crikey_modern_worker.py" ] ||
    die "sdk/python has no _crikey_modern_worker.py; sdk_root() would reject the staged directory"

copy_python_payload "${ROOT_DIR}/crates/crikey-legacy-compat/python" "${MACOS_DIR}/legacy-shim"
[ -f "${MACOS_DIR}/legacy-shim/_crikey_legacy_worker.py" ] ||
    die "the legacy shim has no _crikey_legacy_worker.py; shim_root() would reject the staged directory"

# Spec 14.13: the licence and the attribution notice travel with the artefact,
# not only with the source repository.
cp -- "${ROOT_DIR}/LICENSE" "${RESOURCES_DIR}/LICENSE"
cp -- "${ROOT_DIR}/NOTICE.md" "${RESOURCES_DIR}/NOTICE.md"

if [ -n "${icon}" ]; then
    [ -f "${icon}" ] || die "icon '${icon}' does not exist"
    cp -- "${icon}" "${RESOURCES_DIR}/CriKey.icns"
    plutil -replace CFBundleIconFile -string "CriKey" "${APP}/Contents/Info.plist"
else
    note "no --icon given: the bundle ships without an icon, and without a CFBundleIconFile key"
fi

if [ -n "${python_runtime}" ]; then
    stager="${ROOT_DIR}/packaging/stage-python-runtime.sh"
    [ -x "${stager}" ] ||
        die "--python-runtime was given but ${stager} is missing or not executable"
    "${stager}" --dest "${MACOS_DIR}" --archive "${python_runtime}"
    [ -x "${MACOS_DIR}/python-runtime/bin/python3" ] ||
        die "the stager did not produce ${MACOS_DIR}/python-runtime/bin/python3"
else
    note "no --python-runtime given: no interpreter is bundled, so Python plugins need an interpreter already on the target machine"
fi

if [ "${unsigned}" = "yes" ]; then
    note "built UNSIGNED at ${APP}"
    note "an unsigned bundle cannot be notarized and Gatekeeper will refuse it elsewhere"
    exit 0
fi

# Sign inside out. `codesign --deep` is Apple-deprecated and, worse, applies
# the top-level entitlements to nested code that never asked for them; signing
# nested Mach-O objects first and the bundle last gives each exactly the
# signature it should carry. `crikey` is nested code by this definition: the
# bundle's main executable is `crikey-launcher`, and only that one is covered
# by the bundle signature below, so the command line beside it must be signed
# here or it ships unsigned.
note "signing nested executables"
while IFS= read -r nested; do
    note "  ${nested}"
    codesign --force --timestamp --options runtime --sign "${identity}" "${nested}"
done < <(find "${MACOS_DIR}" -type f -perm -u+x ! -path "${MACOS_DIR}/crikey-launcher" -exec sh -c 'file -b "$1" | grep -q Mach-O' _ {} \; -print)

note "signing ${APP}"
codesign --force --timestamp --options runtime \
    --entitlements "${SCRIPT_DIR}/Entitlements.plist" \
    --sign "${identity}" "${APP}"

# Verification is neither optional nor configurable here. A signature nobody
# checked is a signature nobody can rely on, and the failure mode -- a bundle
# that works everywhere except a customer's machine -- surfaces far too late.
note "verifying signature"
codesign --verify --strict --deep --verbose=2 "${APP}"

note "built ${APP}"
note "next: packaging/macos/notarize.sh --path '${APP}'"
