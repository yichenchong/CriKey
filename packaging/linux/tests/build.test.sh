#!/usr/bin/env bash
#
# Contract tests for `packaging/linux/build.sh`.
#
# WHY THIS EXISTS
#
# The build script's promises are the kind that rot silently: NOTICE.md is in
# every artefact (spec 14.13), a package that ships no interpreter says so in
# its dependencies, and a missing tool is a loud failure rather than a missing
# file. Nothing about a successful build reveals that one of those stopped
# being true, so each is asserted here against a real artefact.
#
# Cargo is replaced by a recording fixture for the engine-build path; supplied
# executable fixtures cover the staging path without requiring WPE on CI.
#
# USAGE
#   packaging/linux/tests/build.test.sh

set -euo pipefail

tests_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repository_root="$(cd "$tests_root/../../.." && pwd)"
build="$tests_root/../build.sh"
workspace="$(mktemp -d)"
in_tree_workspace="$(mktemp -d "$repository_root/.crikey-flatpak-test.XXXXXX")"
trap 'rm -rf "$workspace" "$in_tree_workspace"' EXIT

failures=0
passes=0

# Read from the one place a release bump edits, the same way build.sh reads it.
# Spelling the version out here made every artefact name in this file a second
# thing to remember on a version bump, and the first symptom was this suite
# failing on a release commit that was perfectly correct.
workspace_version() {
	sed -n '/^\[workspace\.package\]/,/^\[/ s/^version = "\(.*\)"$/\1/p' \
		"$repository_root/Cargo.toml"
}

check() {
	local description="$1"
	shift
	if "$@"; then
		passes=$((passes + 1))
		printf 'ok   %s\n' "$description"
	else
		failures=$((failures + 1))
		printf 'FAIL %s\n' "$description" >&2
	fi
}

contains() {
	grep -qF -- "$1" <<<"$2"
}

# Executable fixtures cover staging without requiring a Rust toolchain or WPE.
# The web host script is admitted only through build.sh's explicit test-fixture
# switch; production packaging requires an ELF and validates its dependencies.
fixture_bin="$workspace/executables"
mkdir -p "$fixture_bin"

cat >"$fixture_bin/crikey" <<'EOF'
#!/usr/bin/env bash
printf 'crikey fixture\n'
EOF
cat >"$fixture_bin/crikey-launcher" <<'EOF'
#!/usr/bin/env bash
printf 'launcher fixture\n'
EOF
cat >"$fixture_bin/crikey-wasm-host" <<'EOF'
#!/usr/bin/env bash
printf 'wasm host fixture\n'
EOF
cat >"$fixture_bin/crikey-cabi-host" <<'EOF'
#!/usr/bin/env bash
printf 'c-abi host fixture\n'
EOF
cat >"$fixture_bin/crikey-web-host" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
--help)
	printf 'Usage: crikey-web-host --endpoint <unix:/absolute/socket> [--storage-dir <absolute/path>]\n'
	;;
--endpoint)
	[[ $# -eq 2 && $2 == unix:/* ]]
	;;
*)
	printf 'crikey-web-host: --endpoint is required\n' >&2
	exit 2
	;;
esac
EOF
chmod +x "$fixture_bin"/*

stand_in="$fixture_bin/crikey"
launcher_stand_in="$fixture_bin/crikey-launcher"
wasm_host_stand_in="$fixture_bin/crikey-wasm-host"
cabi_host_stand_in="$fixture_bin/crikey-cabi-host"
web_host_stand_in="$fixture_bin/crikey-web-host"
staging_build() {
	CRIKEY_PACKAGING_TEST_ALLOW_SCRIPT_WEB_HOST=1 "$build" \
		--binary "$stand_in" \
		--launcher-binary "$launcher_stand_in" \
		--wasm-host-binary "$wasm_host_stand_in" \
		--cabi-host-binary "$cabi_host_stand_in" \
		--web-host-binary "$web_host_stand_in" \
		"$@"
}

# Release CI normally supplies the four long-standing binaries without a web
# host. Keep that path successful and prove it does not accidentally inherit a
# host from a previous web-enabled stage.
staging_build_without_web() {
	"$build" \
		--binary "$stand_in" \
		--launcher-binary "$launcher_stand_in" \
		--wasm-host-binary "$wasm_host_stand_in" \
		--cabi-host-binary "$cabi_host_stand_in" \
		"$@"
}

out="$workspace/out"
build_log="$workspace/build.log"
staging_build --targets stage,tarball,deb "$out" >"$build_log" 2>&1

staged="$out/stage"

no_web_out="$workspace/no-web-out"
# First leave a web host in this exact output tree, then rebuild it in normal
# no-web mode. `stage` promises replacement rather than merging; a stale host
# would make a package claim support the operator did not request.
staging_build --targets stage "$no_web_out" >"$workspace/prior-web-build.log" 2>&1
staging_build_without_web --targets stage "$no_web_out" >"$workspace/no-web-build.log" 2>&1
check "a normal four-binary rebuild removes a previously staged web host" \
	test -x "$no_web_out/stage/lib/crikey/crikey" \
	-a ! -e "$no_web_out/stage/lib/crikey/crikey-web-host"

check "the staged tree carries the licence and the attribution notice" \
	test -f "$staged/share/doc/crikey/LICENSE" -a -f "$staged/share/doc/crikey/NOTICE.md"

check "the staged tree installs the desktop entry and a hicolor icon" \
	test -f "$staged/share/applications/crikey.desktop" \
	-a -f "$staged/share/icons/hicolor/scalable/apps/crikey.svg"

check "the staged executable is executable through the bin/ symlink" \
	test -x "$staged/bin/crikey"

# The whole point of the symlink layout: `current_exe` resolves /proc/self/exe,
# so the launcher's own directory is lib/crikey, and that is where sdk_root and
# shim_root look. Ship these anywhere else and every Python plugin, modern and
# legacy, fails to start on an installed system.
check "bin/crikey resolves to the real executable under lib/crikey" \
	bash -c 'test "$(readlink -f "$1/bin/crikey")" = "$(readlink -f "$1/lib/crikey/crikey")"' \
	_ "$staged"

# The desktop entry runs `crikey-launcher`, so the package has to contain one,
# under that name, and it must not be the command-line binary wearing a
# different hat.
check "the no-argument launcher is installed and reachable from bin/" \
	test -x "$staged/bin/crikey-launcher"
check "bin/crikey-launcher resolves to the launcher, not to the command line" \
	bash -c 'test "$(readlink -f "$1/bin/crikey-launcher")" = "$(readlink -f "$1/lib/crikey/crikey-launcher")" &&
		! cmp -s "$1/lib/crikey/crikey-launcher" "$1/lib/crikey/crikey"' _ "$staged"
check "the staged WASM host sits beside the launcher" \
	test -x "$staged/lib/crikey/crikey-wasm-host"
check "the staged C-ABI host sits beside the launcher" \
	test -x "$staged/lib/crikey/crikey-cabi-host"
check "a web-enabled package stages its supervised web host" \
	test -x "$staged/lib/crikey/crikey-web-host"
check "the staged web host is the supplied executable" \
	cmp -s "$web_host_stand_in" "$staged/lib/crikey/crikey-web-host"
check "the modern SDK worker sits beside the real executable" \
	test -f "$staged/lib/crikey/modern-sdk/_crikey_modern_worker.py"
check "the legacy shim worker sits beside the real executable" \
	test -f "$staged/lib/crikey/legacy-shim/_crikey_legacy_worker.py"
check "the legacy shim ships the Keypirinha-compatible modules it emulates" \
	test -f "$staged/lib/crikey/legacy-shim/keypirinha.py" \
	-a -f "$staged/lib/crikey/legacy-shim/keypirinha_util.py"
check "no byte-compiled cache from the build host is shipped" \
	bash -c 'test -z "$(find "$1" -name "__pycache__" -o -name "*.pyc")"' _ "$staged"
check "staged files carry packaging modes, not the builder's umask" \
	bash -c 'test -z "$(find "$1" -type f -perm /022)"' _ "$staged"

desktop_entry="$(cat "$staged/share/applications/crikey.desktop")"
# A menu entry cannot pass arguments, and bare `crikey` prints usage, so the
# entry must name the no-argument launcher -- and the file it names must be the
# one the packager installed, which is why the last check reads the tree rather
# than the entry.
check "the desktop entry starts the launcher rather than printing usage" \
	contains "Exec=crikey-launcher" "$desktop_entry"
check "the desktop entry probes for the same executable it runs" \
	contains "TryExec=crikey-launcher" "$desktop_entry"
check "the desktop entry declares the window class winit derives from argv[0]" \
	contains "StartupWMClass=crikey-launcher" "$desktop_entry"
check "the executable the desktop entry names is the one that was installed" \
	bash -c 'test -x "$2/bin/$(printf "%s" "$1" | sed -n "s,^Exec=,,p")"' \
	_ "$desktop_entry" "$staged"
check "the desktop entry names an icon that is installed beside it" \
	contains "Icon=crikey" "$desktop_entry"

tarball="$out/crikey-$(workspace_version)-$(uname -m | sed 's,^arm64$,aarch64,')-linux.tar.gz"
tarball_listing="$(tar tzf "$tarball")"
check "the tarball ships the attribution notice" \
	contains "share/doc/crikey/NOTICE.md" "$tarball_listing"
check "the tarball is prefix-relative, so it unpacks over any prefix" \
	bash -c '! printf "%s" "$1" | grep -q "^crikey-[^/]*/usr/"' _ "$tarball_listing"

# Every other check here reads the staging tree, where the links were always
# right. The archive is a separate artefact and it was wrong for at least two
# releases: the member-renaming transform also rewrote link *targets*, so
# `../lib/crikey/crikey` shipped as `crikey-<version>./lib/crikey/crikey` and
# an installed `bin/crikey` was a dangling link. Nothing caught it because
# nothing had ever unpacked the tarball and followed them.
extracted="$workspace/extracted"
mkdir -p "$extracted"
tar xzf "$tarball" -C "$extracted"
unpacked="$extracted/$(printf "%s" "$tarball_listing" | head -1 | cut -d/ -f1)"
for link in crikey crikey-launcher; do
	check "the unpacked bin/$link resolves to the executable it names" \
		bash -c 'test -x "$(readlink -f "$1/bin/$2")" &&
			test "$(readlink -f "$1/bin/$2")" = "$(readlink -f "$1/lib/crikey/$2")"' _ "$unpacked" "$link"
done

# Reproducibility is the whole reason for the fixed mtime, ownership and sort
# order; a second build into a different directory must agree byte for byte.
second="$workspace/second"
staging_build --targets stage,tarball "$second" >/dev/null 2>&1
check "two builds of one commit produce an identical tarball" \
	cmp -s "$tarball" "$second/$(basename "$tarball")"

if command -v dpkg-deb >/dev/null 2>&1; then
	deb="$(find "$out" -maxdepth 1 -name 'crikey_*.deb' -print -quit)"
	check "the deb target produced a package" test -n "$deb"
	control="$(dpkg-deb --field "$deb")"
	contents="$(dpkg-deb --contents "$deb")"
	check "a package with no bundled interpreter depends on the system python3" \
		contains "python3 (>= 3.8)" "$control"
	check "the package declares the version from the workspace manifest" \
		contains "Version: $(workspace_version)" "$control"
	check "the package ships the attribution notice" \
		contains "usr/share/doc/crikey/NOTICE.md" "$contents"
	check "the package installs the binary onto the default search path" \
		contains "/usr/bin/crikey" "$contents"
	check "package contents are owned by root, not by whoever built them" \
		bash -c '! printf "%s" "$1" | grep -qv "root/root"' _ "$contents"
	check "the package links bin/crikey to the real executable under lib" \
		contains "/usr/bin/crikey -> ../lib/crikey/crikey" "$contents"
	check "the package ships the worker trees the launcher resolves beside itself" \
		bash -c 'printf "%s" "$1" | grep -q "/usr/lib/crikey/modern-sdk/_crikey_modern_worker.py" &&
			printf "%s" "$1" | grep -q "/usr/lib/crikey/legacy-shim/_crikey_legacy_worker.py"' \
		_ "$contents"
	check "the package ships both supervised runtime hosts" \
		bash -c 'printf "%s" "$1" | grep -q "/usr/lib/crikey/crikey-wasm-host" &&
			printf "%s" "$1" | grep -q "/usr/lib/crikey/crikey-cabi-host"' \
		_ "$contents"
	check "a web-enabled Debian package declares the WPE runtime when ELF scanning is unavailable" \
		bash -c 'for dependency in libwpewebkit-2.0-1 libwpe-1.0-1 libwpebackend-fdo-1.0-1 libwayland-server0 libglib2.0-0; do
			printf "%s" "$1" | grep -q "$dependency" || exit 1
		done' _ "$control"
	# Working-copy mtimes vary between checkouts, so the deb target clamps
	# them; without that clamp this is the check that fails.
	staging_build --targets deb "$second" >/dev/null 2>&1
	check "two builds of one commit produce an identical package" \
		cmp -s "$deb" "$second/$(basename "$deb")"
else
	printf 'skip dpkg-deb is not installed; the .deb contracts were not checked\n'
fi

# Failure modes. Each of these has to be loud: a packaging run that reports
# success while producing nothing is the bug this whole script exists to avoid.
refuses() {
	local description="$1"
	shift
	local output status=0
	output="$("$@" 2>&1)" || status=$?
	if [[ $status -eq 0 ]]; then
		failures=$((failures + 1))
		printf 'FAIL %s (exited 0)\n' "$description" >&2
		return
	fi
	passes=$((passes + 1))
	printf 'ok   %s\n' "$description"
	# The message has to name the thing that is wrong, not just fail.
	printf '%s' "$output" | grep -q 'build.sh:' ||
		printf 'note %s: the failure carried no build.sh message\n' "$description" >&2
}

flatpak_stub_dir="$workspace/fake-bin"
mkdir -p "$flatpak_stub_dir"
cat >"$flatpak_stub_dir/flatpak-builder" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
chmod +x "$flatpak_stub_dir/flatpak-builder"
refuses "Flatpak without a bundled interpreter is refused" \
	env PATH="$flatpak_stub_dir:$PATH" "$build" --targets flatpak "$workspace/refused"

refuses "an unknown target is refused" \
	"$build" --targets nonsense "$workspace/refused"
refuses "an empty target list is refused" \
	"$build" --targets "" "$workspace/refused"
refuses "an empty target element is refused" \
	"$build" --targets stage,,tarball "$workspace/refused"
refuses "a trailing empty target element is refused" \
	"$build" --targets stage, "$workspace/refused"
refuses "a newline cannot silently discard later targets" \
	staging_build_without_web --targets $'stage\nnonsense' "$workspace/refused"
refuses "an unknown option is refused" \
	"$build" --nonsense "$workspace/refused"
refuses "the deb target refuses a prefix Debian policy forbids" \
	staging_build --prefix /opt/crikey --targets deb "$workspace/refused"
refuses "a bundled runtime that cannot be staged fails the build" \
	staging_build --python-archive "$workspace/absent.tar.zst" \
	--targets stage "$workspace/refused"
refuses "two output directories are refused" \
	"$build" "$workspace/one" "$workspace/two"
refuses "one of the two executables without the other is refused" \
	"$build" --binary "$stand_in" --targets stage "$workspace/refused"
refuses "a missing launcher executable is refused rather than shipped without" \
	"$build" --binary "$stand_in" --launcher-binary "$workspace/absent" \
	--targets stage "$workspace/refused"
refuses "runtime hosts are required with custom release binaries" \
	"$build" --binary "$stand_in" --launcher-binary "$launcher_stand_in" \
	--targets stage "$workspace/refused"
refuses "a missing supplied web host is refused rather than silently omitted" \
	"$build" --binary "$stand_in" --launcher-binary "$launcher_stand_in" \
	--wasm-host-binary "$wasm_host_stand_in" --cabi-host-binary "$cabi_host_stand_in" \
	--web-host-binary "$workspace/absent" --targets stage "$workspace/refused"
refuses "a web host cannot be mixed with Cargo-built launcher binaries" \
	"$build" --web-host-binary "$web_host_stand_in" \
	--targets stage "$workspace/refused"

invalid_web_host="$fixture_bin/not-a-web-host"
cat >"$invalid_web_host" <<'EOF'
#!/usr/bin/env bash
# This deliberately mimics the old human-readable checks. It is still not a
# production host: scripts cannot establish a native host's structural contract.
if [[ ${1:-} == --help ]]; then
	printf 'Usage: crikey-web-host --endpoint SOCKET --storage-dir DIRECTORY\n'
	exit 0
fi
exit 2
EOF
chmod +x "$invalid_web_host"
refuses "a help-mimicking script is refused as a structurally incompatible supplied web host" \
	"$build" --binary "$stand_in" --launcher-binary "$launcher_stand_in" \
	--wasm-host-binary "$wasm_host_stand_in" --cabi-host-binary "$cabi_host_stand_in" \
	--web-host-binary "$invalid_web_host" --targets stage "$workspace/refused"

localized_ldd_tools="$workspace/localized-ldd-tools"
mkdir -p "$localized_ldd_tools"
cat >"$localized_ldd_tools/ldd" <<'EOF'
#!/usr/bin/env bash
if [[ ${LC_ALL:-} == C ]]; then
	printf 'libmissing-for-test.so => not found\n'
else
	printf 'libmissing-for-test.so => nicht gefunden\n'
fi
EOF
chmod +x "$localized_ldd_tools/ldd"
elf_web_host="$workspace/crikey-web-host-elf"
cp /bin/true "$elf_web_host"
refuses "missing ELF libraries are refused under a forced C locale" \
	env PATH="$localized_ldd_tools:$PATH" "$build" \
	--binary "$stand_in" --launcher-binary "$launcher_stand_in" \
	--wasm-host-binary "$wasm_host_stand_in" --cabi-host-binary "$cabi_host_stand_in" \
	--web-host-binary "$elf_web_host" --targets stage "$workspace/refused"
refuses "engine mode with supplied release binaries also requires a supplied web host" \
	"$build" --binary "$stand_in" --launcher-binary "$launcher_stand_in" \
	--wasm-host-binary "$wasm_host_stand_in" --cabi-host-binary "$cabi_host_stand_in" \
	--with-web-surface --targets stage "$workspace/refused"
refuses "Flatpak refuses web mode because its recipe has no engine host or pinned WPE dependencies" \
	env PATH="$flatpak_stub_dir:$PATH" "$build" --with-web-surface \
	--targets flatpak --python-archive "$workspace/absent.tar" "$workspace/refused"

# Exercise the genuine Cargo mode without requiring WPE on the test machine.
# The recording tool supplies the exact executables Cargo would have produced;
# the assertions below prove that build.sh selects the engine feature and uses
# the target directory reported by Cargo metadata.
recording_tools="$workspace/recording-tools"
mkdir -p "$recording_tools"
cat >"$recording_tools/pkg-config" <<'EOF'
#!/usr/bin/env bash
[[ ${CRIKEY_TEST_MISSING_WPE:-} != 1 || ${2:-} != wpe-webkit-2.0 ]]
EOF
cat >"$recording_tools/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${CRIKEY_TEST_CARGO_LOG:?}"
target="${CRIKEY_TEST_CARGO_TARGET_DIR:-${CARGO_TARGET_DIR:-}}"
[[ -n $target ]]
if [[ $target != /* ]]; then
	target="$PWD/$target"
fi
if [[ ${1:-} == metadata ]]; then
	escaped_target="${target//\\/\\\\}"
	escaped_target="${escaped_target//\"/\\\"}"
	printf '{"target_directory":"%s"}\n' "$escaped_target"
	exit 0
fi
destination="$target/release"
mkdir -p "$destination"
for executable in crikey crikey-launcher crikey-wasm-host crikey-cabi-host crikey-web-host; do
	cp "${CRIKEY_TEST_FIXTURE_BIN:?}/$executable" "$destination/$executable"
done
EOF
chmod +x "$recording_tools"/*

cargo_mode_out="$workspace/cargo-mode"
cargo_log="$workspace/cargo.log"
env PATH="$recording_tools:$PATH" \
	CARGO_TARGET_DIR="$workspace/cargo-target" \
	CRIKEY_TEST_CARGO_LOG="$cargo_log" \
	CRIKEY_TEST_FIXTURE_BIN="$fixture_bin" \
	CRIKEY_PACKAGING_TEST_ALLOW_SCRIPT_WEB_HOST=1 \
	"$build" --with-web-surface --targets stage "$cargo_mode_out" >/dev/null
check "engine mode asks Cargo for the real web-host binary with its engine feature" \
	bash -c 'grep -q -- "--package crikey-web-host" "$1" &&
		grep -q -- "--features engine" "$1"' _ "$cargo_log"
check "engine mode stages the web host from Cargo's reported target directory" \
	cmp -s "$web_host_stand_in" "$cargo_mode_out/stage/lib/crikey/crikey-web-host"
check "Cargo metadata cannot rewrite the release lockfile" \
	grep -q -- "metadata --locked --format-version 1 --no-deps" "$cargo_log"

missing_wpe_preflight() {
	local log="$workspace/missing-wpe-cargo.log" status=0
	env PATH="$recording_tools:$PATH" \
		CRIKEY_TEST_MISSING_WPE=1 \
		CRIKEY_TEST_CARGO_LOG="$log" \
		"$build" --with-web-surface --targets stage \
		"$workspace/missing-wpe" >"$workspace/missing-wpe.log" 2>&1 || status=$?
	[[ $status -ne 0 && ! -e "$log" ]] &&
		grep -q 'wpe-webkit-2.0 development package' "$workspace/missing-wpe.log"
}
check "missing WPE dependencies fail before Cargo builds the launcher" missing_wpe_preflight

empty_web_host_preflight() {
	local form="$1" log="$workspace/empty-web-host-$1-cargo.log" status=0
	local -a option=(--web-host-binary "")
	if [[ $form == equals ]]; then option=(--web-host-binary=); fi
	env PATH="$recording_tools:$PATH" \
		CARGO_TARGET_DIR="$workspace/empty-web-host-target" \
		CRIKEY_TEST_CARGO_LOG="$log" \
		CRIKEY_TEST_FIXTURE_BIN="$fixture_bin" \
		CRIKEY_PACKAGING_TEST_ALLOW_SCRIPT_WEB_HOST=1 \
		"$build" "${option[@]}" --targets stage \
		"$workspace/empty-web-host" >"$workspace/empty-web-host-$form.log" 2>&1 || status=$?
	[[ $status -ne 0 && ! -e "$log" ]] &&
		grep -q -- '--web-host-binary needs a nonempty value' "$workspace/empty-web-host-$form.log"
}
check "an empty supplied web-host path fails before selecting Cargo" empty_web_host_preflight separate
check "an empty equals-form web-host path fails before selecting Cargo" empty_web_host_preflight equals

escaped_target_out="$workspace/escaped-target-out"
env PATH="$recording_tools:$PATH" \
	CARGO_TARGET_DIR="$workspace/cargo \"quoted\" \\target" \
	CRIKEY_TEST_CARGO_LOG="$workspace/escaped-target-cargo.log" \
	CRIKEY_TEST_FIXTURE_BIN="$fixture_bin" \
	"$build" --targets stage "$escaped_target_out" >/dev/null
check "JSON-escaped Cargo target paths stage the actual binaries" \
	cmp -s "$stand_in" "$escaped_target_out/stage/lib/crikey/crikey"

# The initial shell hosts force the real scanner down its fallback path.
# Separately exercise successful scanning, recording which installed files
# were inspected rather than asserting source text in build.sh.
cat >"$recording_tools/dpkg-shlibdeps" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$@" >"${CRIKEY_TEST_SHLIBDEPS_LOG:?}"
if [[ ${CRIKEY_TEST_MISSING_SHLIBS_INFO:-} == 1 ]]; then
	# Real dpkg-shlibdeps succeeds with incomplete output under this option.
	# Without it, missing metadata must select build.sh's explicit fallback.
	for argument in "$@"; do
		if [[ $argument == --ignore-missing-info ]]; then
			printf 'shlibs:Depends=libc6\n'
			exit 0
		fi
	done
	exit 2
fi
printf 'shlibs:Depends=libc6, libwpewebkit-2.0-1 (>= 2.48), libwpe-1.0-1, libwpebackend-fdo-1.0-1, libwayland-server0, libglib2.0-0\n'
EOF
chmod +x "$recording_tools/dpkg-shlibdeps"
scanned_out="$workspace/scanned-out"
scan_log="$workspace/shlibdeps.log"
env PATH="$recording_tools:$PATH" \
	CRIKEY_TEST_SHLIBDEPS_LOG="$scan_log" \
	CRIKEY_PACKAGING_TEST_ALLOW_SCRIPT_WEB_HOST=1 \
	"$build" --binary "$stand_in" --launcher-binary "$launcher_stand_in" \
	--wasm-host-binary "$wasm_host_stand_in" --cabi-host-binary "$cabi_host_stand_in" \
	--web-host-binary "$web_host_stand_in" --targets deb "$scanned_out" >/dev/null
scanned_deb="$(find "$scanned_out" -maxdepth 1 -name 'crikey_*.deb' -print -quit)"
for executable in crikey crikey-launcher crikey-wasm-host crikey-cabi-host crikey-web-host; do
	check "Debian dependency scanning inspects the installed $executable" \
		grep -qxF "$scanned_out/deb/crikey_$(workspace_version)_$(dpkg-deb --field "$scanned_deb" Architecture)/usr/lib/crikey/$executable" "$scan_log"
done
check "computed engine dependency versions reach the Debian control record" \
	contains "libwpewebkit-2.0-1 (>= 2.48)" "$(dpkg-deb --field "$scanned_deb" Depends)"

missing_info_out="$workspace/missing-shlibs-info"
env PATH="$recording_tools:$PATH" \
	CRIKEY_TEST_SHLIBDEPS_LOG="$workspace/missing-shlibs-info.log" \
	CRIKEY_TEST_MISSING_SHLIBS_INFO=1 \
	CRIKEY_PACKAGING_TEST_ALLOW_SCRIPT_WEB_HOST=1 \
	"$build" --binary "$stand_in" --launcher-binary "$launcher_stand_in" \
	--wasm-host-binary "$wasm_host_stand_in" --cabi-host-binary "$cabi_host_stand_in" \
	--web-host-binary "$web_host_stand_in" --targets deb "$missing_info_out" >/dev/null
missing_info_deb="$(find "$missing_info_out" -maxdepth 1 -name 'crikey_*.deb' -print -quit)"
check "incomplete Debian library metadata cannot silently omit the engine runtime" \
	bash -c 'for dependency in libwpewebkit-2.0-1 libwpe-1.0-1 libwpebackend-fdo-1.0-1 libwayland-server0 libglib2.0-0; do
		printf "%s" "$1" | grep -q "$dependency" || exit 1
	done' _ "$(dpkg-deb --field "$missing_info_deb" Depends)"

relative_target="${in_tree_workspace#"$repository_root/"/}/relative-cargo-target"
relative_target_out="$workspace/relative-target-out"
env PATH="$recording_tools:$PATH" \
	CARGO_TARGET_DIR="$relative_target" \
	CRIKEY_TEST_CARGO_LOG="$workspace/relative-cargo.log" \
	CRIKEY_TEST_FIXTURE_BIN="$fixture_bin" \
	"$build" --targets stage "$relative_target_out" >/dev/null
check "a relative Cargo target directory is resolved from the repository root" \
	cmp -s "$stand_in" "$relative_target_out/stage/lib/crikey/crikey"

configured_target="$workspace/configured-cargo-target"
configured_target_out="$workspace/configured-target-out"
env -u CARGO_TARGET_DIR PATH="$recording_tools:$PATH" \
	CRIKEY_TEST_CARGO_TARGET_DIR="$configured_target" \
	CRIKEY_TEST_CARGO_LOG="$workspace/configured-cargo.log" \
	CRIKEY_TEST_FIXTURE_BIN="$fixture_bin" \
	"$build" --targets stage "$configured_target_out" >/dev/null
check "Cargo's configured target directory is taken from metadata" \
	cmp -s "$stand_in" "$configured_target_out/stage/lib/crikey/crikey"

mixed_flatpak_preflight() {
	local out="$workspace/mixed-flatpak" status=0
	rm -rf "$out"
	env PATH="$flatpak_stub_dir:$PATH" "$build" \
		--binary "$stand_in" \
		--launcher-binary "$launcher_stand_in" \
		--wasm-host-binary "$wasm_host_stand_in" \
		--cabi-host-binary "$cabi_host_stand_in" \
		--targets stage,flatpak "$out" >/dev/null 2>&1 || status=$?
	[[ $status -ne 0 && ! -e "$out/stage" ]]
}
check "Flatpak archive rejection precedes mixed-target staging" mixed_flatpak_preflight

if python_bin="$(command -v python3 2>/dev/null)" && [[ -x $python_bin ]]; then
	python_bin="$(readlink -f "$python_bin")"
	python_home="${python_bin%/bin/*}"
	fixture="$workspace/python-fixture"
	mkdir -p "$fixture/python/bin"
	cp "$python_bin" "$fixture/python/bin/python3"
	printf 'home = %s\ninclude-system-site-packages = false\nversion = 3.8\n' \
		"$python_home" >"$fixture/python/pyvenv.cfg"
	tar -czf "$workspace/python-runtime.tar.gz" -C "$fixture" python
	flatpak_capture="$workspace/flatpak-manifest.yaml"
	cat >"$flatpak_stub_dir/flatpak-builder" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
manifest="${!#}"
base="$(cd "$(dirname "$manifest")" && pwd)"
! grep -Eq '^[[:space:]]+path: /' "$manifest"
test -d "$base/sources/crikey"
archive="$(find "$base/sources" -maxdepth 1 -type f -name 'python-runtime.tar*' -print -quit)"
test -n "$archive" && test -f "$archive"
cp "$manifest" "${CRIKEY_TEST_FLATPAK_MANIFEST:?}"
EOF
	chmod +x "$flatpak_stub_dir/flatpak-builder"
	env PATH="$flatpak_stub_dir:$PATH" \
		CRIKEY_TEST_FLATPAK_MANIFEST="$flatpak_capture" \
		"$build" --targets flatpak \
		--python-archive "$workspace/python-runtime.tar.gz" \
		"$workspace/flatpak" >/dev/null
	flatpak_manifest="$(cat "$flatpak_capture")"
	flatpak_manifest_root="$workspace/flatpak/flatpak"
	check "Flatpak generated manifest resolves all source placeholders" \
		bash -c '! grep -qF "__CRIKEY_" "$1"' _ "$flatpak_capture"
	check "Flatpak generated manifest uses an in-tree repository source" \
		contains "path: sources/crikey" "$flatpak_manifest"
	check "Flatpak repository source is staged below the manifest" \
		test -d "$flatpak_manifest_root/sources/crikey"
	check "Flatpak runtime archive is staged below the manifest" \
		test -f "$flatpak_manifest_root/sources/python-runtime.tar.gz"
	check "Flatpak generated manifest pins the runtime checksum" \
		bash -c 'grep -Eq "sha256: [[:xdigit:]]{64}" "$1"' _ "$flatpak_capture"
	check "Flatpak staging command uses the matching tar.gz name" \
		contains "--archive python-runtime.tar.gz" "$flatpak_manifest"
	in_tree_flatpak="$in_tree_workspace/output"
	env PATH="$flatpak_stub_dir:$PATH" \
		CRIKEY_TEST_FLATPAK_MANIFEST="$flatpak_capture" \
		"$build" --targets flatpak \
		--python-archive "$workspace/python-runtime.tar.gz" \
		"$in_tree_flatpak" >/dev/null
	check "Flatpak excludes an in-tree output workspace from source staging" \
		test -d "$in_tree_flatpak/flatpak/sources/crikey"


	tar -cf "$workspace/python-runtime.tar" -C "$fixture" python
	env PATH="$flatpak_stub_dir:$PATH" \
		CRIKEY_TEST_FLATPAK_MANIFEST="$flatpak_capture" \
		"$build" --targets flatpak \
		--python-archive "$workspace/python-runtime.tar" \
		"$workspace/flatpak-tar" >/dev/null
	flatpak_tar_manifest="$(cat "$flatpak_capture")"
	if command -v zstd >/dev/null 2>&1; then
		zstd -q -f "$workspace/python-runtime.tar" -o "$workspace/python-runtime.tar.zst"
		env PATH="$flatpak_stub_dir:$PATH" \
			CRIKEY_TEST_FLATPAK_MANIFEST="$flatpak_capture" \
			"$build" --targets flatpak \
			--python-archive "$workspace/python-runtime.tar.zst" \
			"$workspace/flatpak-zstd" >/dev/null
		flatpak_zstd_manifest="$(cat "$flatpak_capture")"
		check "Flatpak staging command preserves tar.zst extraction" \
			contains "--archive python-runtime.tar.zst" "$flatpak_zstd_manifest"
	else
		printf 'skip zstd is not installed; Flatpak tar.zst extraction was not exercised\n'
	fi
	check "Flatpak staging command preserves tar extraction" \
		contains "--archive python-runtime.tar" "$flatpak_tar_manifest"
else
	printf 'skip python3 is not installed; Flatpak runtime injection was not exercised\n'
fi

"$build" --help >"$workspace/help.txt"
check "--help documents the output directory argument" \
	grep -q "OUTPUT_DIRECTORY" "$workspace/help.txt"
check "--help names the tool each artefact needs" \
	grep -q "rpmbuild" "$workspace/help.txt"

printf '\n%s passed, %s failed\n' "$passes" "$failures"
[[ $failures -eq 0 ]]
