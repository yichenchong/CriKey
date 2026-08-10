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
# No Cargo build happens: `--binary` packages a stand-in executable, because
# every contract under test is about the packaging, not about the program.
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

# The four executables are staged beside each other in a real package. Stand-ins
# keep this test about layout and packaging without a Cargo build.
stand_in="/usr/bin/true"
launcher_stand_in="/usr/bin/false"
wasm_host_stand_in="/usr/bin/true"
cabi_host_stand_in="/usr/bin/false"

staging_build() {
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
