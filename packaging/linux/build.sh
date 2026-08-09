#!/usr/bin/env bash
#
# Build the Linux distribution artefacts for CriKey.
#
# WHY THIS EXISTS
#
# Every Linux artefact shares the install contract -- payload trees, runtime
# hosts, notices and layout -- but Flatpak is built from the repository source
# rather than the staged tree. Flatpak also has a stricter Python contract:
# freedesktop.Platform does not promise a usable `python3`, so the Flatpak
# target injects and validates the pinned runtime archive below.
# Four hand-maintained recipes drift within a release or two, and the first
# symptom is a package missing NOTICE.md, which spec 14.13 requires in every
# artefact. The staging step remains the source of truth for the other Linux
# formats; the Flatpak manifest mirrors that contract explicitly.
#
# The staged tree is prefix-relative (`bin/`, `share/`, optionally
# `lib/crikey/`) rather than rooted at `/usr`, because a tarball is unpacked
# over `/usr/local` at least as often as a distribution package is installed
# under `/usr`, and only the steps that genuinely care about the prefix should
# have to know it.
#
# USAGE
#   packaging/linux/build.sh [OPTIONS] [OUTPUT_DIRECTORY]
#
#   OUTPUT_DIRECTORY defaults to `target/packaging/linux` in the repository.
#   Everything the script writes goes underneath it; it writes nowhere else and
#   never needs root.
#
# OPTIONS
#   --targets LIST        Comma-separated artefacts to produce. One or more of
#                         `stage`, `tarball`, `deb`, `rpm`, `flatpak`, or
#                         `all`. Default: `stage,tarball`. Staging always runs
#                         first, because every other target consumes it.
#   --binary PATH         Package this already-built `crikey` executable
#                         instead of invoking Cargo. Use it in CI, where the
#                         binaries were built and tested in an earlier job.
#                         Must be given together with --launcher-binary,
#                         --wasm-host-binary and --cabi-host-binary.
#   --launcher-binary P   The matching already-built `crikey-launcher`, the
#                         no-argument executable the desktop entry runs.
#                         An installation needs both, so this is not optional.
#   --wasm-host-binary P  The matching `crikey-wasm-host` worker. It must sit
#                         beside the launcher because the WASM provider never
#                         searches PATH.
#   --cabi-host-binary P  The matching `crikey-cabi-host` worker. It must sit
#                         beside the launcher because the C-ABI provider never
#                         searches PATH.
#   --prefix PATH         Install prefix the artefacts are built for.
#   --python-archive PATH A python-build-standalone archive to bundle as the
#                         plugin runtime, handed to
#                         `packaging/stage-python-runtime.sh`. Defaults to
#                         $CRIKEY_PYTHON_STANDALONE_ARCHIVE. Tarball, deb and
#                         rpm fall back to the system's `python3` when no
#                         archive is supplied; the Flatpak target requires
#                         this archive because its sandbox runtime may not
#                         contain a usable interpreter.
#   -h, --help            Print this section and exit.
#
# REQUIRED TOOLS, PER TARGET
#   stage     `cargo`, unless all four executable paths are given.
#             `desktop-file-validate` (package desktop-file-utils) is used when present and skipped
#             with a warning when not: it checks the entry, it does not produce
#             it, so its absence cannot corrupt an artefact.
#   tarball   `tar` and `gzip`.
#   deb       `dpkg-deb` (package dpkg). `dpkg-shlibdeps` (package dpkg-dev) is
#             used when present to compute shared-library dependencies from
#             the ELF; without it the conservative list below is used and the
#             script says which one you got.
#   rpm       `rpmbuild` (package rpm-build).
#   flatpak   `flatpak-builder`, `tar` and `sha256sum` (coreutils), plus the
#             runtime and SDK named in `flatpak/org.crikey.CriKey.yaml` from Flathub.
#
# Every target checks for its tool up front and fails with the tool's name and
# the package providing it. No target degrades quietly into producing nothing.
#
# REPRODUCIBILITY
#
# Timestamps in the tarball come from $SOURCE_DATE_EPOCH, defaulting to the
# commit time of HEAD, so two builds of one commit produce identical archives.
# Running the script twice over one output directory is safe: each target
# clears its own working tree first rather than merging into it.

set -euo pipefail

usage() {
	awk '/^# USAGE$/ { show = 1 }
	     show { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "$0"
}

fail() {
	printf 'build.sh: %s\n' "$*" >&2
	exit 1
}

note() {
	printf 'build.sh: %s\n' "$*"
}

# A missing packaging tool is a hard failure with a name attached, never a
# silently skipped artefact: a build that reports success must have produced
# everything it was asked for.
require_tool() {
	local tool="$1" package="$2" target="$3"
	command -v "$tool" >/dev/null 2>&1 ||
		fail "the \`$target\` target needs \`$tool\`, from the $package package; install it and run again"
}

# Literal placeholder substitution. `sed` is the obvious tool and the wrong one:
# a Debian Depends field contains commas, slashes, pipes and parentheses, so
# every plausible sed delimiter appears in the data.
substitute() {
	awk -v version="$1" -v architecture="$2" -v depends="$3" \
		-v installed_size="$4" -v maintainer="$5" '
		function put(line, key, value,   at) {
			while ((at = index(line, key)) > 0) {
				line = substr(line, 1, at - 1) value substr(line, at + length(key))
			}
			return line
		}
		{
			$0 = put($0, "@VERSION@", version)
			$0 = put($0, "@ARCHITECTURE@", architecture)
			$0 = put($0, "@DEPENDS@", depends)
			$0 = put($0, "@INSTALLED_SIZE@", installed_size)
			$0 = put($0, "@MAINTAINER@", maintainer)
			print
		}
	' "$6"
}
targets="stage,tarball"
binary=""
launcher_binary=""
wasm_host_binary=""
cabi_host_binary=""
prefix="/usr"
python_archive="${CRIKEY_PYTHON_STANDALONE_ARCHIVE:-}"
output=""

while (($#)); do
	case "$1" in
	--targets)
		[[ $# -ge 2 ]] || fail "--targets needs a value"
		targets="$2"
		shift 2
		;;
	--targets=*)
		targets="${1#*=}"
		shift
		;;
	--binary)
		[[ $# -ge 2 ]] || fail "--binary needs a value"
		binary="$2"
		shift 2
		;;
	--binary=*)
		binary="${1#*=}"
		shift
		;;
	--wasm-host-binary)
		[[ $# -ge 2 ]] || fail "--wasm-host-binary needs a value"
		wasm_host_binary="$2"
		shift 2
		;;
	--wasm-host-binary=*)
		wasm_host_binary="${1#*=}"
		shift
		;;
	--cabi-host-binary)
		[[ $# -ge 2 ]] || fail "--cabi-host-binary needs a value"
		cabi_host_binary="$2"
		shift 2
		;;
	--cabi-host-binary=*)
		cabi_host_binary="${1#*=}"
		shift
		;;
	--launcher-binary)
		[[ $# -ge 2 ]] || fail "--launcher-binary needs a value"
		launcher_binary="$2"
		shift 2
		;;
	--launcher-binary=*)
		launcher_binary="${1#*=}"
		shift
		;;
	--prefix)
		[[ $# -ge 2 ]] || fail "--prefix needs a value"
		prefix="$2"
		shift 2
		;;
	--prefix=*)
		prefix="${1#*=}"
		shift
		;;
	--python-archive)
		[[ $# -ge 2 ]] || fail "--python-archive needs a value"
		python_archive="$2"
		shift 2
		;;
	--python-archive=*)
		python_archive="${1#*=}"
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	-*)
		printf 'build.sh: unknown option: %s\n\n' "$1" >&2
		usage >&2
		exit 2
		;;
	*)
		[[ -z $output ]] || fail "only one output directory may be given (got \`$output\` and \`$1\`)"
		output="$1"
		shift
		;;
	esac
done

[[ $prefix == /* ]] || fail "--prefix must be absolute (got \`$prefix\`)"

for requested in ${targets//,/ }; do
	case "$requested" in
	stage | tarball | deb | rpm | flatpak | all) ;;
	*) fail "unknown target \`$requested\`; expected stage, tarball, deb, rpm, flatpak or all" ;;
	esac
done

packaging_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repository_root="$(cd "$packaging_root/../.." && pwd)"
output="${output:-$repository_root/target/packaging/linux}"
mkdir -p "$output"
output="$(cd "$output" && pwd)"

# The version lives in one place, `[workspace.package]`, and is read rather
# than repeated here so a release bump cannot leave one artefact behind.
version="$(sed -n '/^\[workspace\.package\]/,/^\[/ s/^version = "\(.*\)"$/\1/p' \
	"$repository_root/Cargo.toml")"
[[ -n $version ]] || fail "no \`version\` under [workspace.package] in $repository_root/Cargo.toml"

machine="$(uname -m)"
case "$machine" in
x86_64)
	debian_architecture="amd64"
	rpm_architecture="x86_64"
	;;
aarch64 | arm64)
	debian_architecture="arm64"
	rpm_architecture="aarch64"
	machine="aarch64"
	;;
*)
	# Refusing beats mislabelling: a package whose Architecture field lies is
	# worse than no package at all, because the archive will install it anyway.
	fail "unsupported machine \`$machine\`; add its Debian and RPM architecture names here first"
	;;
esac

if [[ -z ${SOURCE_DATE_EPOCH:-} ]]; then
	if git -C "$repository_root" rev-parse --git-dir >/dev/null 2>&1; then
		SOURCE_DATE_EPOCH="$(git -C "$repository_root" log -1 --pretty=%ct 2>/dev/null || echo 0)"
	else
		SOURCE_DATE_EPOCH=0
	fi
fi
export SOURCE_DATE_EPOCH

stage_directory="$output/stage"
deb_workspace="$output/deb"
rpm_workspace="$output/rpm"
flatpak_workspace="$output/flatpak"

# Whether the artefacts carry their own interpreter. Written by `stage`, read
# by the package targets, and the single reason a `python3` dependency is or is
# not declared -- so the declaration always describes what was actually built.
bundles_python="no"
flatpak_archive_name=""
flatpak_archive_sha256=""

wanted() {
	local target="$1"
	[[ ",$targets," == *",all,"* || ",$targets," == *",$target,"* ]]
}

# The launcher and its three supervised worker executables must share one
# directory. The WASM and C-ABI providers intentionally do not search PATH:
# an installed package must carry the exact hosts it is going to supervise.
resolve_binaries() {
	local given=0
	for name in binary launcher_binary wasm_host_binary cabi_host_binary; do
		if [[ -n ${!name} ]]; then given=$((given + 1)); fi
	done
	if [[ $given -ne 0 && $given -ne 4 ]]; then
		fail "--binary, --launcher-binary, --wasm-host-binary and --cabi-host-binary must be given together, or omitted so Cargo builds all four"
	fi
	if [[ $given -eq 0 ]]; then
		require_tool cargo "Rust toolchain" stage
		note "building crikey and runtime hosts $version in release mode"
		(
			cd "$repository_root" &&
			cargo build --release --locked \
				--package crikey-cli \
				--package crikey-wasm-host \
				--package crikey-cabi-host
		)
		local target_directory="${CARGO_TARGET_DIR:-$repository_root/target}"
		binary="$target_directory/release/crikey"
		launcher_binary="$target_directory/release/crikey-launcher"
		wasm_host_binary="$target_directory/release/crikey-wasm-host"
		cabi_host_binary="$target_directory/release/crikey-cabi-host"
	fi

	local name path
	for name in binary launcher_binary wasm_host_binary cabi_host_binary; do
		path="${!name}"
		[[ -f $path ]] || fail "no such executable: $path"
		[[ -x $path ]] || fail "not executable: $path"
		printf -v "$name" '%s' "$(cd "$(dirname "$path")" && pwd)/$(basename "$path")"
	done
	note "packaging $binary, $launcher_binary, $wasm_host_binary and $cabi_host_binary"
}

# The interpreter is a hard runtime requirement, not a nicety: `crikey run`
# spawns a CPython worker for every Python plugin, modern and legacy alike. An
# artefact therefore either carries an interpreter or declares one, and this is
# what decides which.
stage_python_runtime() {
	local stager="$repository_root/packaging/stage-python-runtime.sh"
	if [[ -z $python_archive ]]; then
		note "no python-build-standalone archive given; artefacts will depend on the system python3"
		return
	fi
	[[ -x $stager ]] ||
		fail "--python-archive was given but $stager is missing or not executable"
	# `crikey` looks for `python-runtime/bin/python3` beside its own
	# executable, which after the symlink layout in `stage` is
	# `lib/crikey/python-runtime`.
	"$stager" --dest "$stage_directory/lib/crikey" --archive "$python_archive" ||
		fail "staging the bundled Python runtime failed; refusing to ship an artefact that claims one"
	[[ -x "$stage_directory/lib/crikey/python-runtime/bin/python3" ]] ||
		fail "the runtime stager reported success but no interpreter is at $stage_directory/lib/crikey/python-runtime/bin/python3"
	bundles_python="yes"
	note "bundled the Python runtime from $python_archive"
}

# The Python trees `crikey` loads its workers from: `sdk_root` and `shim_root`
# look for `modern-sdk` and `legacy-shim` beside the running executable and
# otherwise fall back to a source path baked in at compile time, which does not
# exist on an installed system. A package without these two directories
# therefore has no working Python plugin support at all -- modern or legacy.
stage_python_trees() {
	local destination="$stage_directory/lib/crikey"
	cp -a "$repository_root/sdk/python" "$destination/modern-sdk"
	cp -a "$repository_root/crates/crikey-legacy-compat/python" "$destination/legacy-shim"
	# Byte-compiled caches from a developer's interpreter are the wrong
	# version for the user's, and they would make the artefact depend on who
	# built it.
	find "$destination/modern-sdk" "$destination/legacy-shim" \
		-name '__pycache__' -type d -prune -exec rm -rf {} +
	# A developer's umask is not a packaging decision: the repository copies
	# are group-writable, which every distribution's package checker rejects.
	find "$destination/modern-sdk" "$destination/legacy-shim" -type d -exec chmod 755 {} +
	find "$destination/modern-sdk" "$destination/legacy-shim" -type f -exec chmod 644 {} +
	[[ -f "$destination/modern-sdk/_crikey_modern_worker.py" ]] ||
		fail "the modern worker entrypoint is missing from the staged modern-sdk"
	[[ -f "$destination/legacy-shim/_crikey_legacy_worker.py" ]] ||
		fail "the legacy worker entrypoint is missing from the staged legacy-shim"
}

stage() {
	resolve_binaries

	# Idempotence: the tree is rebuilt, never merged into. A file dropped from
	# this script must disappear from the next build's artefacts, and a merged
	# tree would keep shipping it forever.
	rm -rf "$stage_directory"
	# The executable lives beside the trees it resolves relative to itself
	# (`modern-sdk`, `legacy-shim`, and any bundled `python-runtime`), and
	# `bin/crikey` is a symlink into that directory. This works, rather than
	# needing those directories inside `bin/`, because `std::env::current_exe`
	# reads `/proc/self/exe` and so reports the resolved target: the parent
	# directory the launcher sees is `lib/crikey`, not `bin`.
	install -Dm755 "$binary" "$stage_directory/lib/crikey/crikey"
	install -Dm755 "$launcher_binary" "$stage_directory/lib/crikey/crikey-launcher"
	mkdir -p "$stage_directory/bin"
	ln -sf ../lib/crikey/crikey "$stage_directory/bin/crikey"
	# The desktop entry runs this one, and it takes no arguments.
	ln -sf ../lib/crikey/crikey-launcher "$stage_directory/bin/crikey-launcher"
	stage_python_trees
	install -Dm644 "$packaging_root/crikey.desktop" \
		"$stage_directory/share/applications/crikey.desktop"
	install -Dm644 "$packaging_root/icons/hicolor/scalable/apps/crikey.svg" \
		"$stage_directory/share/icons/hicolor/scalable/apps/crikey.svg"
	install -Dm644 "$packaging_root/org.crikey.CriKey.metainfo.xml" \
		"$stage_directory/share/metainfo/org.crikey.CriKey.metainfo.xml"

	# Spec 14.13: the licence and the attribution notice travel with every
	# artefact, not only with the source tree.
	install -Dm644 "$repository_root/LICENSE" "$stage_directory/share/doc/crikey/LICENSE"
	install -Dm755 "$wasm_host_binary" "$stage_directory/lib/crikey/crikey-wasm-host"
	install -Dm755 "$cabi_host_binary" "$stage_directory/lib/crikey/crikey-cabi-host"
	install -Dm644 "$repository_root/NOTICE.md" "$stage_directory/share/doc/crikey/NOTICE.md"

	stage_python_runtime

	if command -v desktop-file-validate >/dev/null 2>&1; then
		desktop-file-validate "$stage_directory/share/applications/crikey.desktop" ||
			fail "the desktop entry is invalid"
	else
		note "desktop-file-validate not installed; the desktop entry was not checked"
	fi

	note "staged $stage_directory for prefix $prefix"
}

require_stage() {
	[[ -d $stage_directory ]] || fail "internal error: the staged tree is missing"
}

tarball() {
	require_stage
	local archive="$output/crikey-$version-$machine-linux.tar.gz"
	rm -f "$archive"
	# `--sort=name`, a fixed mtime, fixed ownership and `gzip -n` are what make
	# two builds of one commit compare equal; without them the archive embeds
	# the build host's clock, uid and directory order.
	tar --create \
		--directory "$stage_directory" \
		--transform "s,^\\.,crikey-$version," \
		--owner=0 --group=0 --numeric-owner \
		--sort=name \
		--mtime="@$SOURCE_DATE_EPOCH" \
		--format=gnu \
		. |
		gzip -n -9 >"$archive"
	note "wrote $archive"
	note "install it with: sudo tar --strip-components=1 -C $prefix -xf $archive"
}

# Debian's shared-library dependencies, computed from the ELF when the tool for
# it exists. The fallback in `deb/control.in` is deliberately unversioned,
# because a guessed `libc6 (>= 2.x)` is either wrong or needlessly restrictive;
# the note tells the operator which of the two they got.
debian_shared_library_depends() {
	local work="$1"
	command -v dpkg-shlibdeps >/dev/null 2>&1 || return 1
	local scratch computed=""
	scratch="$(mktemp -d)"
	mkdir -p "$scratch/debian"
	printf 'Source: crikey\n\nPackage: crikey\nArchitecture: any\n' >"$scratch/debian/control"
	if ! computed="$(cd "$scratch" && dpkg-shlibdeps -O --ignore-missing-info \
		"$work/usr/bin/crikey" 2>/dev/null)"; then
		computed=""
	fi
	rm -rf "$scratch"
	[[ -n $computed ]] || return 1
	printf '%s' "${computed#shlibs:Depends=}"
}

deb() {
	require_stage

	local work="$deb_workspace/crikey_${version}_${debian_architecture}"
	rm -rf "$work"
	mkdir -p "$work/usr" "$work/DEBIAN"
	cp -a "$stage_directory/." "$work/usr/"

	local depends
	if depends="$(debian_shared_library_depends "$work")"; then
		note "shared-library dependencies computed from the ELF: $depends"
	else
		depends="libc6, libgcc-s1"
		note "dpkg-shlibdeps unavailable or unable to read the ELF; falling back to \`$depends\`"
	fi
	if [[ $bundles_python == "no" ]]; then
		# Not a Recommends: with no interpreter every Python plugin fails to
		# start, and that is most of the plugin ecosystem.
		depends="$depends, python3 (>= 3.8)"
	fi

	local installed_size
	installed_size="$(du -sk "$work/usr" | cut -f1)"

	substitute \
		"$version" \
		"$debian_architecture" \
		"$depends" \
		"$installed_size" \
		"${CRIKEY_MAINTAINER:-The CriKey Authors <crikey@localhost>}" \
		"$packaging_root/deb/control.in" >"$work/DEBIAN/control"

	# `dpkg --audit` and every archive tool expect these; their absence is the
	# difference between a package a distribution will consider and one it will
	# not.
	(cd "$work" && find usr -type f -print0 | LC_ALL=C sort -z |
		xargs -0 md5sum) >"$work/DEBIAN/md5sums"

	find "$work" -type d -exec chmod 755 {} +
	# Working-copy mtimes differ between two checkouts of the same commit, and
	# dpkg-deb records them, so without this the .deb is reproducible only on
	# the machine that first built it.
	find "$work" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +
	local archive="$output/crikey_${version}_${debian_architecture}.deb"
	rm -f "$archive"
	# `--root-owner-group` is why this needs neither root nor fakeroot: the
	# archive records uid/gid 0 whoever ran the build.
	dpkg-deb --root-owner-group --build "$work" "$archive" >/dev/null
	note "wrote $archive"
}

rpm() {
	require_stage

	rm -rf "$rpm_workspace"
	mkdir -p "$rpm_workspace"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}

	local -a defines=(
		--define "_topdir $rpm_workspace"
		--define "_rpmdir $output"
		--define "_build_id_links none"
		--define "crikey_version $version"
		--define "crikey_stagedir $stage_directory"
	)
	if [[ $bundles_python == "no" ]]; then
		defines+=(--define "crikey_python_requires python3 >= 3.8")
	fi

	rpmbuild -bb "$packaging_root/rpm/crikey.spec" \
		"${defines[@]}" \
		--target "$rpm_architecture"
	note "wrote $output/$rpm_architecture/crikey-$version-1.$rpm_architecture.rpm"
}

require_flatpak_python_archive() {
	if [[ -z $python_archive ]]; then
		fail "the \`flatpak\` target requires --python-archive (freedesktop.Platform does not guarantee python3)"
	fi
	[[ -f $python_archive ]] ||
		fail "the Flatpak Python archive does not exist: $python_archive"
	[[ -r $python_archive ]] ||
		fail "the Flatpak Python archive is not readable: $python_archive"
	case "$python_archive" in
	*.tar.gz | *.tgz) flatpak_archive_name="python-runtime.tar.gz" ;;
	*.tar.zst) flatpak_archive_name="python-runtime.tar.zst" ;;
	*.tar) flatpak_archive_name="python-runtime.tar" ;;
	*) fail "the Flatpak Python archive must end in .tar.gz, .tgz, .tar.zst or .tar" ;;
	esac
	python_archive="$(cd "$(dirname "$python_archive")" && pwd)/$(basename "$python_archive")"
	flatpak_archive_sha256="$(sha256sum -- "$python_archive" | cut -d ' ' -f1)"
	[[ ${#flatpak_archive_sha256} -eq 64 ]] ||
		fail "could not compute a SHA-256 checksum for the Flatpak Python archive"
}

flatpak() {
	require_flatpak_python_archive
	rm -rf "$flatpak_workspace"
	mkdir -p "$flatpak_workspace"

	# Validate the exact archive before handing it to flatpak-builder. The
	# stager rejects unsafe tar members and proves that the interpreter's
	# sys.prefix stays inside the relocatable runtime tree.
	local validation="$flatpak_workspace/runtime-validation"
	"$repository_root/packaging/stage-python-runtime.sh" \
		--dest "$validation" \
		--archive "$python_archive" ||
		fail "the Flatpak Python archive failed relocatability validation"
	[[ -x "$validation/python-runtime/bin/python3" ]] ||
		fail "Flatpak runtime validation produced no python3 interpreter"
	rm -rf "$validation"

	# Flatpak-builder requires local sources to remain below the manifest
	# directory. Stage a release-sized source tree (without build output or
	# VCS metadata) and the validated archive beneath that directory rather
	# than naming caller-owned absolute paths.
	local source_root="$flatpak_workspace/sources"
	local repository_source="$source_root/crikey"
	local archive_source="$source_root/$flatpak_archive_name"
	local -a tar_excludes=(
		--exclude=./target
		--exclude=./.git
		--exclude=./.flatpak-builder
	)
	# A caller may choose an output directory inside the checkout (including
	# `.`). Exclude the workspace itself or the source archive would recurse
	# into the copy being made.
	case "$flatpak_workspace/" in
	"$repository_root/"*)
		tar_excludes+=("--exclude=./${flatpak_workspace#"$repository_root"/}")
		;;
	esac
	mkdir -p "$repository_source"
	tar "${tar_excludes[@]}" -C "$repository_root" -cf - . |
		tar -xf - -C "$repository_source" ||
		fail "staging the Flatpak source tree under the manifest failed"
	cp -- "$python_archive" "$archive_source" ||
		fail "copying the Flatpak Python archive under the manifest failed"

	# The checked-in manifest uses placeholders for these in-tree sources.
	local manifest="$flatpak_workspace/org.crikey.CriKey.yaml"
	awk -v repository_source="sources/crikey" \
		-v python_archive_source="sources/$flatpak_archive_name" \
		-v python_archive_name="$flatpak_archive_name" \
		-v python_archive_sha256="$flatpak_archive_sha256" '
		function put(line, key, value, at) {
			while ((at = index(line, key)) > 0)
				line = substr(line, 1, at - 1) value substr(line, at + length(key))
			return line
		}
		{
			$0 = put($0, "__CRIKEY_REPOSITORY_SOURCE__", repository_source)
			$0 = put($0, "__CRIKEY_PYTHON_ARCHIVE_SOURCE__", python_archive_source)
			$0 = put($0, "__CRIKEY_PYTHON_ARCHIVE_NAME__", python_archive_name)
			$0 = put($0, "__CRIKEY_PYTHON_ARCHIVE_SHA256__", python_archive_sha256)
			print
		}
	' "$packaging_root/flatpak/org.crikey.CriKey.yaml" >"$manifest"

	flatpak-builder \
		--force-clean \
		--state-dir "$flatpak_workspace/state" \
		--repo "$flatpak_workspace/repo" \
		"$flatpak_workspace/build" \
		"$manifest"
	note "wrote the Flatpak repository $flatpak_workspace/repo"
	note "bundle it with: flatpak build-bundle $flatpak_workspace/repo crikey.flatpak org.crikey.CriKey"
}

# Every tool and policy check happens here, before anything is built. A release
# build takes minutes, and discovering afterwards that `rpmbuild` is not
# installed wastes all of them.
preflight() {
	if wanted tarball; then
		require_tool tar tar tarball
		require_tool gzip gzip tarball
	fi
	if wanted deb; then
		require_tool dpkg-deb dpkg deb
		[[ $prefix == "/usr" ]] ||
			fail "the \`deb\` target requires --prefix /usr (Debian policy); got \`$prefix\`"
	fi
	if wanted rpm; then
		require_tool rpmbuild rpm-build rpm
		[[ $prefix == "/usr" ]] ||
			fail "the \`rpm\` target requires --prefix /usr; got \`$prefix\`"
	fi
	if wanted flatpak; then
		require_tool flatpak-builder flatpak-builder flatpak
		require_tool tar tar flatpak
		require_tool sha256sum coreutils flatpak
		require_flatpak_python_archive
	fi
}

preflight

# `flatpak` is the one target that does not consume the staged tree, so asking
# for it alone must not trigger a release build of the host binary.
if wanted stage || wanted tarball || wanted deb || wanted rpm; then
	stage
fi
if wanted tarball; then tarball; fi
if wanted deb; then deb; fi
if wanted rpm; then rpm; fi
if wanted flatpak; then flatpak; fi

exit 0
