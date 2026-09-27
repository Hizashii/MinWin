#[derive(Debug, Clone, Copy)]
pub struct BenchmarkResults {
    pub ram: &'static str,
    pub cpu: &'static str,
    pub processes: &'static str,
    pub services: &'static str,
    pub boot_time: &'static str,
}

/// Returns values for the first UI and CLI version.
///
/// PLACEHOLDER: benchmark modules will provide measured values later.
pub fn placeholder_results() -> BenchmarkResults {
    BenchmarkResults {
        ram: "-- (placeholder)",
        cpu: "-- (placeholder)",
        processes: "-- (placeholder)",
        services: "-- (placeholder)",
        boot_time: "-- (placeholder)",
    }
}
