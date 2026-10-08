//! The seam between MinWin and Windows.
//!
//! Everything above this module (benchmarking, change planning, apply, diff,
//! rollback) talks only to these traits. That is what makes `cargo test`
//! incapable of touching the developer's machine: the test suite constructs a
//! [`crate::sys::fake::FakeMachine`] and the engine cannot tell the difference.

use crate::core::error::Result;
use crate::sys::model::{
    ManagementState, MemorySnapshot, PowerScheme, PowerSchemeId, RegistryRoot, RegistryValue,
    ServiceConfig, ServiceStartType, ServiceStateSummary, WindowsVersion,
};

/// Read-only facts about the running system.
pub trait SystemInfo {
    fn windows_version(&self) -> Result<WindowsVersion>;
    /// Whether the current process token is elevated.
    fn is_elevated(&self) -> Result<bool>;
    fn management_state(&self) -> Result<ManagementState>;
    fn memory(&self) -> Result<MemorySnapshot>;
    fn uptime_seconds(&self) -> Result<u64>;
}

/// Sampled CPU utilisation.
///
/// CPU time is a counter, so a single read means nothing. Implementations hold
/// the previous reading and report the busy percentage over the interval since
/// it, which is why this takes `&mut self` and why the first call after
/// construction has no interval to report.
pub trait CpuSampler {
    /// Busy percentage since the previous call, or `None` if this is the
    /// priming call.
    fn sample_busy_percent(&mut self) -> Result<Option<f64>>;
}

pub trait ProcessInspector {
    fn process_count(&self) -> Result<u32>;
}

pub trait ServiceManager {
    /// Returns [`crate::core::error::MinWinError::ServiceNotFound`] when the
    /// service is not installed, which is a normal, expected outcome that
    /// applicability checks rely on.
    fn query(&self, service_name: &str) -> Result<ServiceConfig>;

    /// Changes only the start type. MinWin deliberately does not expose the
    /// other `ChangeServiceConfig` fields: binary paths, accounts and
    /// dependencies are not things an optimisation tool should rewrite.
    fn set_start_type(&self, service_name: &str, start_type: ServiceStartType) -> Result<()>;

    fn summarise(&self) -> Result<ServiceStateSummary>;
}

/// Registry access restricted to what MinWin's changes need.
///
/// Subkey paths are always `&'static str` constants owned by a change
/// implementation. No profile, CLI argument or other external input ever
/// reaches these methods, which is how MinWin keeps a TOML file from
/// addressing arbitrary registry locations.
pub trait RegistryStore {
    fn read_value(
        &self,
        root: RegistryRoot,
        subkey: &str,
        value_name: &str,
    ) -> Result<Option<RegistryValue>>;

    /// Creates the key if it does not exist.
    fn write_dword(
        &self,
        root: RegistryRoot,
        subkey: &str,
        value_name: &str,
        value: u32,
    ) -> Result<()>;

    /// Succeeds if the value is already absent, so that restoring "this value
    /// did not exist before" is idempotent.
    fn delete_value(&self, root: RegistryRoot, subkey: &str, value_name: &str) -> Result<()>;
}

pub trait PowerManager {
    fn active_scheme(&self) -> Result<PowerScheme>;
    fn list_schemes(&self) -> Result<Vec<PowerScheme>>;
    fn set_active_scheme(&self, id: &PowerSchemeId) -> Result<()>;
}

/// The facade a change implementation receives. Bundling the capabilities
/// behind one object keeps [`crate::changes::SystemChange`] object-safe and
/// means a change can be handed a fake machine without any generics.
pub trait Machine: Send + Sync {
    fn info(&self) -> &dyn SystemInfo;
    fn services(&self) -> &dyn ServiceManager;
    fn registry(&self) -> &dyn RegistryStore;
    fn power(&self) -> &dyn PowerManager;
    fn processes(&self) -> &dyn ProcessInspector;
    /// A fresh CPU sampler. Each benchmark run gets its own so that one run's
    /// counters never leak into another's.
    fn cpu_sampler(&self) -> Result<Box<dyn CpuSampler>>;
}
