//! Input and query scheduling (spec 7, 8, 9).
//!
//! Three scheduling profiles exist and they are deliberately not unified:
//!
//! * `legacy-strict`  - obsolete-work replacement, never time debounced.
//! * `legacy-optimized` - opt-in, potentially behaviour changing.
//! * `modern`         - manifest driven debouncing with leading/trailing edges.
//!
//! Every decision function here is pure and takes an explicit timestamp in
//! milliseconds so scheduling can be tested without wall-clock flakiness.

pub mod debounce;
pub mod legacy;
pub mod profile;

pub use debounce::{DebouncePolicy, Debouncer, Dispatch};
pub use legacy::{LegacyDispatch, ObsoleteWorkManager};
pub use profile::SchedulingProfile;

/// Monotonic milliseconds since an arbitrary epoch chosen by the caller.
pub type Millis = u64;
