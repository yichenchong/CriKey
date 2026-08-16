//! Contracts about what `crikey-app` exposes, not about what it does.
//!
//! An integration test links this crate exactly as an outside consumer does:
//! it can name public items and nothing else. That makes it the only place
//! where "this type is publicly usable" is actually checked, because every
//! unit test in `src/` can reach private items and would pass either way.

use crikey_app::{IconFetch, PluginResourceSource};

/// A publicly exported trait must be implementable from outside the crate.
///
/// `PluginResourceSource` is exported so a host embedding CriKey can serve a
/// plugin's icons itself. That is only true if every type in its signature is
/// exported too: when `fetch` returned `Option<Vec<u8>>` the point was moot,
/// but it now returns [`IconFetch`], and an unexported return type would leave
/// the trait nameable, documented, and impossible to implement.
///
/// This is a compile-time assertion. If `IconFetch` stops being exported the
/// test does not fail, it fails to build, which is the same signal a consumer
/// would get.
#[derive(Debug)]
struct ExternalSource;

impl PluginResourceSource for ExternalSource {
    fn fetch(&self, _reference: &str) -> IconFetch {
        IconFetch::Absent
    }
}

#[test]
fn a_host_outside_this_crate_can_implement_the_resource_source_trait() {
    let source: Box<dyn PluginResourceSource> = Box::new(ExternalSource);
    assert!(
        matches!(source.fetch("icon.svg"), IconFetch::Absent),
        "the trait object answers through the publicly exported outcome type"
    );
}
