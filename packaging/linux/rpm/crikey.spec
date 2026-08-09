# RPM recipe for CriKey. Required tool: rpmbuild, from the rpm-build package.
#
# This spec deliberately does not build anything. `packaging/linux/build.sh`
# has already produced one staged tree, and the tarball, `.deb` and `.rpm` are
# translations of that same tree; compiling a second time here would let the
# package formats disagree about what CriKey 0.1.0 is. Flatpak is built from
# the repository source in its freedesktop SDK sandbox and is documented
# separately in the Flatpak manifest.
# The `.rpm` must be invoked with the two defines below -- which is what
# `packaging/linux/build.sh` does:
#
#   rpmbuild -bb packaging/linux/rpm/crikey.spec \
#       --define "crikey_version 0.1.0" \
#       --define "crikey_stagedir /path/to/target/packaging/linux/stage"
#
# Optionally --define "crikey_python_requires python3 >= 3.8", which build.sh
# passes exactly when the staged tree carries no bundled interpreter.

%{!?crikey_version:%{error:crikey_version is not defined; build through packaging/linux/build.sh}}
%{!?crikey_stagedir:%{error:crikey_stagedir is not defined; build through packaging/linux/build.sh}}

# There are no sources to extract debug information against, because the binary
# arrives prebuilt. Without this, rpmbuild fails trying to produce a
# -debuginfo subpackage from a stripped-of-context executable.
%global debug_package %{nil}

Name:           crikey
Version:        %{crikey_version}
Release:        1%{?dist}
Summary:        Fast, keyboard-driven application launcher

License:        Apache-2.0
URL:            https://github.com/crikey-launcher/crikey

# Python plugins, modern and legacy alike, run in a CPython worker process, so
# an interpreter is a hard requirement rather than a suggestion. The define is
# absent when the staged tree bundles its own runtime, and the requirement then
# correctly disappears.
%{?crikey_python_requires:Requires:       %{crikey_python_requires}}

%description
CriKey finds and runs applications, files and plugin commands from a single
keyboard-driven prompt. Plugins run as supervised subprocesses, so a slow or
crashing plugin cannot stall the launcher.

%prep
# Nothing to unpack: the payload is the staged tree named by crikey_stagedir.

%build
# Nothing to build: see the header.

%install
rm -rf %{buildroot}
mkdir -p %{buildroot}%{_prefix}
cp -a %{crikey_stagedir}/. %{buildroot}%{_prefix}/

%files
# Both symlinks into %{_prefix}/lib/crikey: `crikey` is the command line and
# `crikey-launcher` is what the desktop entry runs.
%{_bindir}/crikey
%{_bindir}/crikey-launcher
%{_datadir}/applications/crikey.desktop
%{_datadir}/icons/hicolor/scalable/apps/crikey.svg
%{_datadir}/metainfo/org.crikey.CriKey.metainfo.xml
# Spec 14.13 requires the licence and the attribution notice in every artefact.
# `%%license` and `%%doc` mark files already installed under %{_docdir}; they do
# not move them, so the same paths exist in the .deb and the tarball.
%license %{_datadir}/doc/crikey/LICENSE
%doc %{_datadir}/doc/crikey/NOTICE.md
%dir %{_datadir}/doc/crikey
# The real executable, the `modern-sdk` and `legacy-shim` trees it resolves
# beside itself, and a bundled interpreter when one was staged. `%{_bindir}/crikey`
# above is a symlink into here. Not %{_libdir}: that is /usr/lib64 on 64-bit
# Fedora, and the path has to be the same one the .deb and the tarball use.
%{_prefix}/lib/crikey

%changelog
# Intentionally empty: release notes live in the repository, and a changelog
# duplicated here would be the copy that goes stale.
