//! A fake machine for tests.
//!
//! This exists so the test suite can exercise apply, verify, diff and rollback
//! — including failure paths that would be unethical to trigger on a real
//! machine — without touching the developer's Windows installation.
//!
//! Production code never constructs a `FakeMachine`; the CLI builds a
//! [`crate::sys::windows::WindowsMachine`] and nothing else.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::core::error::{MinWinError, Result};
use crate::sys::model::{
    ManagementState, MemorySnapshot, PowerScheme, PowerSchemeId, RegistryRoot, RegistryValue,
    ServiceConfig, ServiceRunState, ServiceStartType, ServiceStateSummary, WindowsVersion,
};
use crate::sys::traits::{
    CpuSampler, Machine, PowerManager, ProcessInspector, RegistryStore, ServiceManager, SystemInfo,
};

/// A scripted failure, so tests can assert that MinWin stops safely and keeps
/// its rollback knowledge when Windows says no.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailOn {
    ServiceWrite(String),
    RegistryWrite { subkey: String, value_name: String },
    PowerSchemeWrite,
}

#[derive(Debug, Default)]
pub struct FakeState {
    pub services: BTreeMap<String, ServiceConfig>,
    pub registry: BTreeMap<(RegistryRoot, String, String), RegistryValue>,
    pub power_schemes: Vec<PowerScheme>,
    pub active_power_scheme: Option<PowerSchemeId>,
    pub process_count: u32,
    pub service_summary: ServiceStateSummary,
    pub memory: MemorySnapshot,
    pub uptime_seconds: u64,
    pub windows: Option<WindowsVersion>,
    pub elevated: bool,
    pub management: ManagementState,
    pub cpu_series: Vec<f64>,
    pub failures: Vec<FailOn>,
    /// Every mutation MinWin performed, in order. Tests assert on this to prove
    /// that `--dry-run` wrote nothing.
    pub writes: Vec<String>,
}

/// A fake Windows machine whose state tests can inspect and script.
#[derive(Debug)]
pub struct FakeMachine {
    state: Mutex<FakeState>,
}

impl FakeMachine {
    /// A plausible Windows 11 24H2 desktop: not elevated, not managed.
    pub fn windows_11() -> Self {
        let mut state = FakeState {
            process_count: 162,
            service_summary: ServiceStateSummary {
                total: 290,
                running: 118,
            },
            memory: MemorySnapshot {
                total_physical_bytes: 32 * 1024 * 1024 * 1024,
                available_physical_bytes: 21 * 1024 * 1024 * 1024,
                load_percent: 34,
            },
            uptime_seconds: 7 * 3600,
            windows: Some(WindowsVersion {
                product_name: "Windows 10 Pro".into(),
                display_version: Some("24H2".into()),
                major: 10,
                minor: 0,
                build: 26100,
                revision: Some(1742),
            }),
            elevated: false,
            cpu_series: vec![1.8, 2.1, 1.6, 2.4, 1.9, 2.0, 1.7, 2.2, 1.8, 2.0],
            ..Default::default()
        };

        state.services.insert(
            "DiagTrack".into(),
            ServiceConfig {
                name: "DiagTrack".into(),
                display_name: "Connected User Experiences and Telemetry".into(),
                start_type: ServiceStartType::Automatic,
                run_state: ServiceRunState::Running,
            },
        );
        state.services.insert(
            "SysMain".into(),
            ServiceConfig {
                name: "SysMain".into(),
                display_name: "SysMain".into(),
                start_type: ServiceStartType::Automatic,
                run_state: ServiceRunState::Running,
            },
        );

        state.power_schemes = vec![
            PowerScheme {
                id: PowerSchemeId::parse("381b4222-f694-41f0-9685-ff5bb260df2e").unwrap(),
                name: "Balanced".into(),
            },
            PowerScheme {
                id: PowerSchemeId::parse("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c").unwrap(),
                name: "High performance".into(),
            },
        ];
        state.active_power_scheme =
            Some(PowerSchemeId::parse("381b4222-f694-41f0-9685-ff5bb260df2e").unwrap());

        Self {
            state: Mutex::new(state),
        }
    }

    /// Marks the fake process as elevated.
    pub fn elevated(self) -> Self {
        self.mutate(|state| state.elevated = true);
        self
    }

    pub fn managed(self, management: ManagementState) -> Self {
        self.mutate(|state| state.management = management);
        self
    }

    /// Removes a service, so applicability checks can be tested against a
    /// machine where it was never installed.
    pub fn without_service(self, name: &str) -> Self {
        self.mutate(|state| {
            state.services.remove(name);
        });
        self
    }

    /// Removes a power plan, mirroring Windows 11 systems where the High
    /// performance plan is not created.
    pub fn without_power_scheme(self, id: &PowerSchemeId) -> Self {
        self.mutate(|state| state.power_schemes.retain(|scheme| &scheme.id != id));
        self
    }

    pub fn failing(self, failure: FailOn) -> Self {
        self.mutate(|state| state.failures.push(failure));
        self
    }

    pub fn with_cpu_series(self, series: Vec<f64>) -> Self {
        self.mutate(|state| state.cpu_series = series);
        self
    }

    pub fn with_process_count(self, count: u32) -> Self {
        self.mutate(|state| state.process_count = count);
        self
    }

    pub fn with_available_memory(self, bytes: u64) -> Self {
        self.mutate(|state| state.memory.available_physical_bytes = bytes);
        self
    }

    pub fn with_registry_dword(
        self,
        root: RegistryRoot,
        subkey: &str,
        value_name: &str,
        value: u32,
    ) -> Self {
        self.mutate(|state| {
            state.registry.insert(
                (root, subkey.to_string(), value_name.to_string()),
                RegistryValue::Dword(value),
            );
        });
        self
    }

    /// Seeds a value of a type MinWin does not model, so the "something else
    /// owns this setting" path can be tested.
    pub fn with_registry_text(
        self,
        root: RegistryRoot,
        subkey: &str,
        value_name: &str,
        value: &str,
    ) -> Self {
        self.mutate(|state| {
            state.registry.insert(
                (root, subkey.to_string(), value_name.to_string()),
                RegistryValue::Text(value.to_string()),
            );
        });
        self
    }

    pub fn with_service_start_type(self, name: &str, start_type: ServiceStartType) -> Self {
        self.mutate(|state| {
            if let Some(service) = state.services.get_mut(name) {
                service.start_type = start_type;
            }
        });
        self
    }

    /// Simulates a user (or another tool) changing a service behind MinWin's
    /// back, which is what the external-modification detection must notice.
    pub fn change_service_externally(&self, name: &str, start_type: ServiceStartType) {
        self.mutate(|state| {
            if let Some(service) = state.services.get_mut(name) {
                service.start_type = start_type;
            }
        });
    }

    pub fn change_registry_externally(
        &self,
        root: RegistryRoot,
        subkey: &str,
        value_name: &str,
        value: Option<u32>,
    ) {
        self.mutate(|state| {
            let key = (root, subkey.to_string(), value_name.to_string());
            match value {
                Some(value) => {
                    state.registry.insert(key, RegistryValue::Dword(value));
                }
                None => {
                    state.registry.remove(&key);
                }
            }
        });
    }

    /// The ordered log of every mutation MinWin performed.
    pub fn writes(&self) -> Vec<String> {
        self.lock().writes.clone()
    }

    pub fn service_start_type(&self, name: &str) -> Option<ServiceStartType> {
        self.lock()
            .services
            .get(name)
            .map(|service| service.start_type)
    }

    pub fn registry_dword(
        &self,
        root: RegistryRoot,
        subkey: &str,
        value_name: &str,
    ) -> Option<u32> {
        self.lock()
            .registry
            .get(&(root, subkey.to_string(), value_name.to_string()))
            .and_then(RegistryValue::as_dword)
    }

    pub fn active_power_scheme_id(&self) -> Option<PowerSchemeId> {
        self.lock().active_power_scheme.clone()
    }

    fn mutate(&self, action: impl FnOnce(&mut FakeState)) {
        action(&mut self.lock());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(|poisoned| {
            // A poisoned fake means a test panicked mid-mutation; the state is
            // still readable and the original panic is the real failure.
            poisoned.into_inner()
        })
    }

    fn check_failure(&self, failure: &FailOn, operation: &str) -> Result<()> {
        if self.lock().failures.contains(failure) {
            return Err(MinWinError::windows(
                operation,
                "Access is denied.",
                5, // ERROR_ACCESS_DENIED
            ));
        }
        Ok(())
    }
}

impl SystemInfo for FakeMachine {
    fn windows_version(&self) -> Result<WindowsVersion> {
        self.lock().windows.clone().ok_or_else(|| {
            MinWinError::Unsupported("the fake machine has no Windows version".into())
        })
    }

    fn is_elevated(&self) -> Result<bool> {
        Ok(self.lock().elevated)
    }

    fn management_state(&self) -> Result<ManagementState> {
        Ok(self.lock().management)
    }

    fn memory(&self) -> Result<MemorySnapshot> {
        Ok(self.lock().memory)
    }

    fn uptime_seconds(&self) -> Result<u64> {
        Ok(self.lock().uptime_seconds)
    }
}

impl ServiceManager for FakeMachine {
    fn query(&self, service_name: &str) -> Result<ServiceConfig> {
        self.lock()
            .services
            .get(service_name)
            .cloned()
            .ok_or_else(|| MinWinError::ServiceNotFound(service_name.to_string()))
    }

    fn set_start_type(&self, service_name: &str, start_type: ServiceStartType) -> Result<()> {
        self.check_failure(
            &FailOn::ServiceWrite(service_name.to_string()),
            &format!(r#"set the start type of service "{service_name}""#),
        )?;

        let mut state = self.lock();
        let service = state
            .services
            .get_mut(service_name)
            .ok_or_else(|| MinWinError::ServiceNotFound(service_name.to_string()))?;
        service.start_type = start_type;
        state
            .writes
            .push(format!("service:{service_name}={}", start_type.label()));
        Ok(())
    }

    fn summarise(&self) -> Result<ServiceStateSummary> {
        Ok(self.lock().service_summary)
    }
}

impl RegistryStore for FakeMachine {
    fn read_value(
        &self,
        root: RegistryRoot,
        subkey: &str,
        value_name: &str,
    ) -> Result<Option<RegistryValue>> {
        Ok(self
            .lock()
            .registry
            .get(&(root, subkey.to_string(), value_name.to_string()))
            .cloned())
    }

    fn write_dword(
        &self,
        root: RegistryRoot,
        subkey: &str,
        value_name: &str,
        value: u32,
    ) -> Result<()> {
        self.check_failure(
            &FailOn::RegistryWrite {
                subkey: subkey.to_string(),
                value_name: value_name.to_string(),
            },
            &format!(
                r#"write {}\{subkey} value "{value_name}""#,
                root.short_name()
            ),
        )?;

        let mut state = self.lock();
        state.registry.insert(
            (root, subkey.to_string(), value_name.to_string()),
            RegistryValue::Dword(value),
        );
        state.writes.push(format!(
            "registry:{}\\{subkey}\\{value_name}={value}",
            root.short_name()
        ));
        Ok(())
    }

    fn delete_value(&self, root: RegistryRoot, subkey: &str, value_name: &str) -> Result<()> {
        self.check_failure(
            &FailOn::RegistryWrite {
                subkey: subkey.to_string(),
                value_name: value_name.to_string(),
            },
            &format!(
                r#"delete {}\{subkey} value "{value_name}""#,
                root.short_name()
            ),
        )?;

        let mut state = self.lock();
        state
            .registry
            .remove(&(root, subkey.to_string(), value_name.to_string()));
        state.writes.push(format!(
            "registry-delete:{}\\{subkey}\\{value_name}",
            root.short_name()
        ));
        Ok(())
    }
}

impl PowerManager for FakeMachine {
    fn active_scheme(&self) -> Result<PowerScheme> {
        let state = self.lock();
        let active = state.active_power_scheme.clone().ok_or_else(|| {
            MinWinError::Unsupported("the fake machine has no active power plan".into())
        })?;
        state
            .power_schemes
            .iter()
            .find(|scheme| scheme.id == active)
            .cloned()
            .ok_or_else(|| {
                MinWinError::Unsupported("the active power plan is not in the scheme list".into())
            })
    }

    fn list_schemes(&self) -> Result<Vec<PowerScheme>> {
        Ok(self.lock().power_schemes.clone())
    }

    fn set_active_scheme(&self, id: &PowerSchemeId) -> Result<()> {
        self.check_failure(
            &FailOn::PowerSchemeWrite,
            &format!("activate the power plan {id}"),
        )?;

        let mut state = self.lock();
        if !state.power_schemes.iter().any(|scheme| &scheme.id == id) {
            // ERROR_FILE_NOT_FOUND, which is what powrprof returns for a
            // scheme that does not exist.
            return Err(MinWinError::windows(
                format!("activate the power plan {id}"),
                "The system cannot find the file specified.",
                2,
            ));
        }
        state.active_power_scheme = Some(id.clone());
        state.writes.push(format!("power:{id}"));
        Ok(())
    }
}

impl ProcessInspector for FakeMachine {
    fn process_count(&self) -> Result<u32> {
        Ok(self.lock().process_count)
    }
}

/// Replays a scripted CPU series, so benchmark statistics are deterministic.
#[derive(Debug)]
struct FakeCpuSampler {
    series: Vec<f64>,
    index: usize,
}

impl CpuSampler for FakeCpuSampler {
    fn sample_busy_percent(&mut self) -> Result<Option<f64>> {
        let value = self.series.get(self.index).copied();
        self.index += 1;
        Ok(value)
    }
}

impl Machine for FakeMachine {
    fn info(&self) -> &dyn SystemInfo {
        self
    }

    fn services(&self) -> &dyn ServiceManager {
        self
    }

    fn registry(&self) -> &dyn RegistryStore {
        self
    }

    fn power(&self) -> &dyn PowerManager {
        self
    }

    fn processes(&self) -> &dyn ProcessInspector {
        self
    }

    fn cpu_sampler(&self) -> Result<Box<dyn CpuSampler>> {
        Ok(Box::new(FakeCpuSampler {
            series: self.lock().cpu_series.clone(),
            index: 0,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_fake_is_a_plausible_unmanaged_desktop() {
        let machine = FakeMachine::windows_11();
        let version = machine.info().windows_version().expect("version");
        assert_eq!(version.label(), "Windows 11 24H2");
        assert!(!machine.info().is_elevated().expect("elevation"));
        assert!(
            !machine
                .info()
                .management_state()
                .expect("management")
                .is_centrally_managed()
        );
    }

    #[test]
    fn writes_are_recorded_in_order() {
        let machine = FakeMachine::windows_11();
        machine
            .services()
            .set_start_type("DiagTrack", ServiceStartType::Manual)
            .expect("set");
        machine
            .registry()
            .write_dword(RegistryRoot::LocalMachine, r"SOFTWARE\Test", "Value", 7)
            .expect("write");

        assert_eq!(
            machine.writes(),
            vec![
                "service:DiagTrack=Manual".to_string(),
                r"registry:HKLM\SOFTWARE\Test\Value=7".to_string(),
            ]
        );
    }

    #[test]
    fn scripted_failures_surface_as_access_denied() {
        let machine = FakeMachine::windows_11().failing(FailOn::ServiceWrite("DiagTrack".into()));
        let error = machine
            .services()
            .set_start_type("DiagTrack", ServiceStartType::Manual)
            .expect_err("should fail");
        assert!(error.is_privilege_problem());
        assert!(machine.writes().is_empty());
    }

    #[test]
    fn activating_a_missing_power_plan_fails_like_windows_does() {
        let high = PowerSchemeId::parse("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c").unwrap();
        let machine = FakeMachine::windows_11().without_power_scheme(&high);
        assert!(machine.power().set_active_scheme(&high).is_err());
    }

    #[test]
    fn deleting_an_absent_registry_value_succeeds() {
        let machine = FakeMachine::windows_11();
        machine
            .registry()
            .delete_value(RegistryRoot::LocalMachine, r"SOFTWARE\Nope", "Missing")
            .expect("delete should be idempotent");
    }
}
