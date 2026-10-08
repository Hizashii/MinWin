//! Process count and CPU utilisation.

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::ProcessStatus::EnumProcesses;
use windows::Win32::System::Threading::GetSystemTimes;

use crate::core::error::{Result, from_win32};
use crate::sys::traits::{CpuSampler, ProcessInspector};

#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsProcessInspector;

impl ProcessInspector for WindowsProcessInspector {
    fn process_count(&self) -> Result<u32> {
        // EnumProcesses silently truncates when the buffer is too small, so
        // grow until the returned byte count is strictly less than the buffer.
        let mut capacity = 1024usize;
        loop {
            let mut pids = vec![0u32; capacity];
            let bytes = (capacity * std::mem::size_of::<u32>()) as u32;
            let mut needed = 0u32;
            // SAFETY: the buffer holds `capacity` u32s and `bytes` describes
            // it accurately.
            unsafe { EnumProcesses(pids.as_mut_ptr(), bytes, &mut needed) }
                .map_err(|e| from_win32("enumerate running processes", e))?;

            if needed < bytes {
                return Ok(needed / std::mem::size_of::<u32>() as u32);
            }
            capacity *= 2;
            if capacity > 1 << 20 {
                // ~1M processes is not a real system state; report what we have
                // rather than looping.
                return Ok(needed / std::mem::size_of::<u32>() as u32);
            }
        }
    }
}

/// CPU utilisation derived from `GetSystemTimes`.
///
/// Windows exposes cumulative idle/kernel/user tick counts, not a utilisation
/// percentage. Busy time over an interval is
/// `(total_delta - idle_delta) / total_delta`, where `total` is kernel + user
/// (kernel already includes idle, which is why idle is subtracted rather than
/// added).
#[derive(Debug, Default)]
pub struct WindowsCpuSampler {
    previous: Option<CpuTimes>,
}

#[derive(Debug, Clone, Copy)]
struct CpuTimes {
    idle: u64,
    total: u64,
}

impl CpuSampler for WindowsCpuSampler {
    fn sample_busy_percent(&mut self) -> Result<Option<f64>> {
        let current = read_system_times()?;
        let previous = self.previous.replace(current);
        Ok(previous.and_then(|previous| busy_percent(previous, current)))
    }
}

fn read_system_times() -> Result<CpuTimes> {
    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: all three out-parameters are valid FILETIME slots.
    unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }
        .map_err(|e| from_win32("read system CPU times", e))?;

    let idle = filetime_to_u64(idle);
    // Kernel time already includes idle time, so kernel + user is the total.
    let total = filetime_to_u64(kernel) + filetime_to_u64(user);
    Ok(CpuTimes { idle, total })
}

fn filetime_to_u64(value: FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
}

/// Pure interval arithmetic, separated so it can be tested without Windows.
fn busy_percent(previous: CpuTimes, current: CpuTimes) -> Option<f64> {
    let total_delta = current.total.checked_sub(previous.total)?;
    let idle_delta = current.idle.checked_sub(previous.idle)?;
    if total_delta == 0 {
        return None;
    }
    let busy = total_delta.saturating_sub(idle_delta) as f64 / total_delta as f64 * 100.0;
    Some(busy.clamp(0.0, 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fully_idle_interval_reports_zero() {
        let previous = CpuTimes { idle: 0, total: 0 };
        let current = CpuTimes {
            idle: 1000,
            total: 1000,
        };
        assert_eq!(busy_percent(previous, current), Some(0.0));
    }

    #[test]
    fn a_fully_busy_interval_reports_one_hundred() {
        let previous = CpuTimes { idle: 0, total: 0 };
        let current = CpuTimes {
            idle: 0,
            total: 1000,
        };
        assert_eq!(busy_percent(previous, current), Some(100.0));
    }

    #[test]
    fn a_quarter_busy_interval_reports_twenty_five() {
        let previous = CpuTimes {
            idle: 100,
            total: 200,
        };
        let current = CpuTimes {
            idle: 400,
            total: 600,
        };
        assert_eq!(busy_percent(previous, current), Some(25.0));
    }

    #[test]
    fn a_zero_length_interval_has_no_answer_rather_than_a_made_up_one() {
        let times = CpuTimes {
            idle: 10,
            total: 20,
        };
        assert_eq!(busy_percent(times, times), None);
    }

    #[test]
    fn counters_going_backwards_yield_no_sample() {
        let previous = CpuTimes {
            idle: 500,
            total: 1000,
        };
        let current = CpuTimes {
            idle: 100,
            total: 200,
        };
        assert_eq!(busy_percent(previous, current), None);
    }

    #[test]
    fn filetime_halves_are_combined_correctly() {
        let value = FILETIME {
            dwLowDateTime: 0x8765_4321,
            dwHighDateTime: 0x0000_00AB,
        };
        assert_eq!(filetime_to_u64(value), 0x0000_00AB_8765_4321);
    }
}
