//! Profiles: human-readable TOML that selects from MinWin's registered
//! changes.
//!
//! # The security property
//!
//! A profile cannot express an action. It can only name a change id that
//! MinWin already implements, and say whether it is enabled. There is no field
//! for a command, a script, a registry path, a service name or a file. A
//! malicious profile's entire capability is "enable or disable one of four
//! known changes", which is not a privilege escalation — it is a menu.
//!
//! Validation rejects unknown ids by name, so a typo fails loudly at load time
//! rather than silently applying a shorter profile than the user expected.
//!
//! # Where profiles come from
//!
//! The two shipped profiles live in `profiles/*.toml` in the repository and are
//! embedded into the binary with `include_str!`. They are the same text a
//! reader of the repo sees, so the documentation cannot drift from the
//! behaviour, and `minwin apply minimal` works regardless of the working
//! directory. `--profile-file` loads an external file for authoring, and goes
//! through exactly the same validation.

pub mod loader;
pub mod model;

pub use loader::{BUILT_IN_PROFILE_IDS, load_builtin, load_from_file, parse};
pub use model::{Profile, ProfileChangeSelection};
