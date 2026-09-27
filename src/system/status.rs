use super::{processes, services};

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemStatus {
    pub ram_usage_percent: Option<u8>,
    pub cpu_usage_percent: Option<u8>,
    pub process_count: Option<u32>,
    pub service_count: Option<u32>,
    pub uptime: Option<&'static str>,
}

/// Reads the values shown on the dashboard.
///
/// PLACEHOLDER: RAM, CPU, and uptime readers will be implemented in the system
/// backend. Process and service values already have their backend entry points.
pub fn get_system_status() -> SystemStatus {
    SystemStatus {
        ram_usage_percent: None,
        cpu_usage_percent: None,
        process_count: processes::get_process_count(),
        service_count: services::get_running_service_count(),
        uptime: None,
    }
}
