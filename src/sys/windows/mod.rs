//! The real machine.
//!
//! This is the only place in MinWin that calls Windows. Each submodule owns one
//! subsystem and returns the plain types from [`crate::sys::model`].

mod info;
mod power;
mod process;
mod registry;
mod service;

use crate::core::error::Result;
use crate::sys::model::{ManagementState, MemorySnapshot, WindowsVersion};
use crate::sys::traits::{
    CpuSampler, Machine, PowerManager, ProcessInspector, RegistryStore, ServiceManager, SystemInfo,
};

pub use power::well_known as well_known_power_plans;

/// The live system, assembled from the per-subsystem adapters.
#[derive(Debug, Default)]
pub struct WindowsMachine {
    registry: registry::WindowsRegistry,
    services: service::WindowsServiceManager,
    power: power::WindowsPowerManager,
    processes: process::WindowsProcessInspector,
    info: WindowsSystemInfo,
}

impl WindowsMachine {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, Default)]
struct WindowsSystemInfo {
    registry: registry::WindowsRegistry,
}

impl SystemInfo for WindowsSystemInfo {
    fn windows_version(&self) -> Result<WindowsVersion> {
        info::windows_version(&self.registry)
    }

    fn is_elevated(&self) -> Result<bool> {
        info::is_elevated()
    }

    fn management_state(&self) -> Result<ManagementState> {
        info::management_state(&self.registry)
    }

    fn memory(&self) -> Result<MemorySnapshot> {
        info::memory()
    }

    fn uptime_seconds(&self) -> Result<u64> {
        Ok(info::uptime_seconds())
    }
}

impl Machine for WindowsMachine {
    fn info(&self) -> &dyn SystemInfo {
        &self.info
    }

    fn services(&self) -> &dyn ServiceManager {
        &self.services
    }

    fn registry(&self) -> &dyn RegistryStore {
        &self.registry
    }

    fn power(&self) -> &dyn PowerManager {
        &self.power
    }

    fn processes(&self) -> &dyn ProcessInspector {
        &self.processes
    }

    fn cpu_sampler(&self) -> Result<Box<dyn CpuSampler>> {
        Ok(Box::new(process::WindowsCpuSampler::default()))
    }
}
