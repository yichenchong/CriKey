//! Probes for the WPE engine and compiles the C shim against it.
//!
//! This script is where acceptance criterion 30 is actually enforced. The
//! launcher must never link anything that can `dlopen` a browser engine, and
//! the way that stays true is that exactly one crate in this workspace knows
//! how to find one -- this one, which is a separate executable. If the engine
//! is missing the build stops here with an error naming the packages, rather
//! than silently producing a host that cannot host anything.
//!
//! Nothing is probed and nothing is compiled unless the `engine` feature is
//! on, so the engine-independent half of the crate builds and tests anywhere.

use std::path::PathBuf;

/// The engine, its backend and the platform library, as their `.pc` files name
/// them. `wayland-server` is needed for `wl_shm_buffer_*`, which is how the
/// FDO backend hands over a rendered frame, and `glib-2.0` for the main loop
/// and the GObject subclass the input method context has to be.
const REQUIRED: &[(&str, &str)] = &[
    ("wpe-webkit-2.0", "WPE WebKit"),
    ("wpebackend-fdo-1.0", "WPEBackend-fdo"),
    ("wpe-1.0", "libwpe"),
    ("wayland-server", "Wayland server library"),
    ("glib-2.0", "GLib"),
];

fn main() {
    println!("cargo:rerun-if-changed=csrc/ck_web_shim.c");
    println!("cargo:rerun-if-changed=csrc/ck_web_shim.h");

    if std::env::var_os("CARGO_FEATURE_ENGINE").is_none() {
        return;
    }

    let mut include_paths: Vec<PathBuf> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for (package, human) in REQUIRED {
        match pkg_config::Config::new().cargo_metadata(true).probe(package) {
            Ok(library) => include_paths.extend(library.include_paths),
            Err(error) => missing.push(format!("  {package} ({human}): {error}")),
        }
    }

    if !missing.is_empty() {
        // Written as a build-script error, not a warning plus a stub: a host
        // that reports "no engine" at run time would be discovered by a user
        // opening a page, and this is discoverable by whoever builds it.
        panic!(
            "crikey-web-host needs the WPE WebKit development packages, and pkg-config \
             could not find:\n{}\n\nOn Debian and Ubuntu these come from \
             libwpewebkit-2.0-dev, libwpebackend-fdo-1.0-dev, libwpe-1.0-dev and \
             libwayland-dev. CriKey's own tarball builds them from source. To build \
             only the engine-independent half of this crate -- frame conversion, input \
             translation, the IME ordering state machine and message handling, all of \
             which are unit-tested without an engine -- use \
             `cargo test -p crikey-web-host --no-default-features`.",
            missing.join("\n")
        );
    }

    let mut build = cc::Build::new();
    build.file("csrc/ck_web_shim.c").std("c11").warnings(true);
    for path in include_paths {
        build.include(path);
    }
    build.compile("ck_web_shim");
}
