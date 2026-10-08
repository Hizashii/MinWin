//! Service Control Manager access.
//!
//! MinWin changes exactly one field of a service's configuration: the start
//! type. Everything else (`SERVICE_NO_CHANGE`) is left alone, so MinWin cannot
//! corrupt a binary path, account or dependency list even by accident.

use windows::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA, ERROR_SERVICE_DOES_NOT_EXIST,
};
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ENUM_SERVICE_STATUS_PROCESSW,
    EnumServicesStatusExW, OpenSCManagerW, OpenServiceW, QUERY_SERVICE_CONFIGW,
    QueryServiceConfig2W, QueryServiceConfigW, SC_ENUM_PROCESS_INFO, SC_HANDLE, SC_MANAGER_CONNECT,
    SC_MANAGER_ENUMERATE_SERVICE, SERVICE_AUTO_START, SERVICE_BOOT_START, SERVICE_CHANGE_CONFIG,
    SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_CONTINUE_PENDING,
    SERVICE_DELAYED_AUTO_START_INFO, SERVICE_DEMAND_START, SERVICE_DISABLED, SERVICE_NO_CHANGE,
    SERVICE_PAUSE_PENDING, SERVICE_PAUSED, SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS,
    SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_START_TYPE, SERVICE_STATE_ALL,
    SERVICE_STATUS_CURRENT_STATE, SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_SYSTEM_START,
    SERVICE_WIN32,
};
use windows::core::{HSTRING, PCWSTR};

use crate::core::error::{MinWinError, Result, from_win32};
use crate::sys::model::{ServiceConfig, ServiceRunState, ServiceStartType, ServiceStateSummary};
use crate::sys::traits::ServiceManager;

#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsServiceManager;

/// Closes an SCM or service handle on drop.
struct ScHandle(SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenSCManagerW/OpenServiceW and is
        // closed exactly once.
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

fn open_manager(access: u32, operation: &str) -> Result<ScHandle> {
    // SAFETY: null machine and database names select the local, active
    // database, as documented.
    let handle =
        unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access) }.map_err(|e| {
            from_win32(
                format!("open the service control manager to {operation}"),
                e,
            )
        })?;
    Ok(ScHandle(handle))
}

fn open_service(manager: &ScHandle, name: &str, access: u32, operation: &str) -> Result<ScHandle> {
    let wide = HSTRING::from(name);
    // SAFETY: `wide` outlives the call.
    match unsafe { OpenServiceW(manager.0, &wide, access) } {
        Ok(handle) => Ok(ScHandle(handle)),
        Err(error) if error.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => {
            Err(MinWinError::ServiceNotFound(name.to_string()))
        }
        Err(error) => Err(from_win32(
            format!(r#"{operation} for service "{name}""#),
            error,
        )),
    }
}

impl ServiceManager for WindowsServiceManager {
    fn query(&self, service_name: &str) -> Result<ServiceConfig> {
        let manager = open_manager(SC_MANAGER_CONNECT, "read a service configuration")?;
        let service = open_service(
            &manager,
            service_name,
            SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS,
            "open the service configuration",
        )?;

        // QUERY_SERVICE_CONFIGW is variable length: it is a fixed header
        // followed by the strings its pointers refer into, so the buffer must
        // be sized by asking first.
        let mut needed = 0u32;
        // SAFETY: passing no buffer is the documented way to learn the size.
        let probe = unsafe { QueryServiceConfigW(service.0, None, 0, &mut needed) };
        if let Err(error) = probe
            && !is_buffer_too_small(&error)
        {
            return Err(from_win32(
                format!(r#"size the service configuration for "{service_name}""#),
                error,
            ));
        }

        let mut buffer =
            vec![0u8; needed.max(std::mem::size_of::<QUERY_SERVICE_CONFIGW>() as u32) as usize];
        // SAFETY: the buffer is at least as large as Windows reported and is
        // aligned for QUERY_SERVICE_CONFIGW because Vec<u8> allocations from
        // the system allocator are suitably aligned for this POD struct; the
        // cast is to a header Windows itself wrote.
        unsafe {
            QueryServiceConfigW(
                service.0,
                Some(buffer.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>()),
                buffer.len() as u32,
                &mut needed,
            )
        }
        .map_err(|e| {
            from_win32(
                format!(r#"read the service configuration for "{service_name}""#),
                e,
            )
        })?;

        // SAFETY: Windows has just populated the buffer with this layout.
        let config = unsafe { &*buffer.as_ptr().cast::<QUERY_SERVICE_CONFIGW>() };
        let display_name = unsafe { config.lpDisplayName.to_string() }
            .unwrap_or_else(|_| service_name.to_string());

        let start_type = if config.dwStartType == SERVICE_AUTO_START
            && delayed_auto_start(&service, service_name)?
        {
            ServiceStartType::AutomaticDelayed
        } else {
            decode_start_type(config.dwStartType)
        };

        Ok(ServiceConfig {
            name: service_name.to_string(),
            display_name,
            start_type,
            run_state: current_state(&service, service_name)?,
        })
    }

    fn set_start_type(&self, service_name: &str, start_type: ServiceStartType) -> Result<()> {
        if start_type.is_kernel_stage() {
            return Err(MinWinError::Unsupported(format!(
                "MinWin will not set service {service_name:?} to a kernel-stage start type"
            )));
        }

        let manager = open_manager(SC_MANAGER_CONNECT, "change a service configuration")?;
        let service = open_service(
            &manager,
            service_name,
            SERVICE_CHANGE_CONFIG,
            "open the service for configuration changes",
        )?;

        let encoded = match start_type {
            ServiceStartType::Automatic | ServiceStartType::AutomaticDelayed => SERVICE_AUTO_START,
            ServiceStartType::Manual => SERVICE_DEMAND_START,
            ServiceStartType::Disabled => SERVICE_DISABLED,
            // Rejected above.
            ServiceStartType::Boot | ServiceStartType::System => unreachable!(),
        };

        // Every other field is SERVICE_NO_CHANGE / null: MinWin alters the
        // start type and nothing else.
        // SAFETY: all string parameters are null, which the API documents as
        // "leave unchanged".
        unsafe {
            ChangeServiceConfigW(
                service.0,
                windows::Win32::System::Services::ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
                encoded,
                windows::Win32::System::Services::SERVICE_ERROR(SERVICE_NO_CHANGE),
                PCWSTR::null(),
                PCWSTR::null(),
                None,
                PCWSTR::null(),
                PCWSTR::null(),
                PCWSTR::null(),
                PCWSTR::null(),
            )
        }
        .map_err(|e| {
            from_win32(
                format!(
                    r#"set the start type of service "{service_name}" to {}"#,
                    start_type.label()
                ),
                e,
            )
        })?;

        // The delayed-auto flag lives in a separate information class, so an
        // Automatic/AutomaticDelayed distinction has to be written explicitly.
        // Windows ignores the flag for non-automatic start types, but writing
        // it keeps the stored configuration honest.
        if matches!(
            start_type,
            ServiceStartType::Automatic | ServiceStartType::AutomaticDelayed
        ) {
            let info = SERVICE_DELAYED_AUTO_START_INFO {
                fDelayedAutostart: (start_type == ServiceStartType::AutomaticDelayed).into(),
            };
            // SAFETY: `info` matches SERVICE_CONFIG_DELAYED_AUTO_START_INFO.
            unsafe {
                ChangeServiceConfig2W(
                    service.0,
                    SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
                    Some((&raw const info).cast()),
                )
            }
            .map_err(|e| {
                from_win32(
                    format!(r#"set the delayed-start flag of service "{service_name}""#),
                    e,
                )
            })?;
        }

        Ok(())
    }

    fn summarise(&self) -> Result<ServiceStateSummary> {
        let manager = open_manager(
            SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE,
            "enumerate services",
        )?;

        // The sizing probe fails with ERROR_MORE_DATA, not
        // ERROR_INSUFFICIENT_BUFFER, and `pcbBytesNeeded` then holds the
        // *additional* bytes required.
        let mut needed = 0u32;
        let mut returned = 0u32;
        let mut resume = 0u32;
        // SAFETY: passing no buffer asks for the required size.
        let probe = unsafe {
            EnumServicesStatusExW(
                manager.0,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_STATE_ALL,
                None,
                &mut needed,
                &mut returned,
                Some(&mut resume),
                PCWSTR::null(),
            )
        };
        if let Err(error) = probe
            && !is_buffer_too_small(&error)
        {
            return Err(from_win32("size the Windows service list", error));
        }
        if needed == 0 {
            return Ok(ServiceStateSummary::default());
        }

        // Services can be installed between the two calls, so grow and retry
        // rather than reporting a partial count as the whole truth.
        let mut capacity = needed as usize;
        for _ in 0..4 {
            let mut buffer = vec![0u8; capacity];
            returned = 0;
            resume = 0;
            needed = 0;
            // SAFETY: the buffer is at least the size Windows reported.
            let result = unsafe {
                EnumServicesStatusExW(
                    manager.0,
                    SC_ENUM_PROCESS_INFO,
                    SERVICE_WIN32,
                    SERVICE_STATE_ALL,
                    Some(&mut buffer),
                    &mut needed,
                    &mut returned,
                    Some(&mut resume),
                    PCWSTR::null(),
                )
            };

            match result {
                Ok(()) => {
                    // SAFETY: Windows wrote `returned`
                    // ENUM_SERVICE_STATUS_PROCESSW records at the start of the
                    // buffer, which is correctly sized and aligned for them.
                    let entries = unsafe {
                        std::slice::from_raw_parts(
                            buffer.as_ptr().cast::<ENUM_SERVICE_STATUS_PROCESSW>(),
                            returned as usize,
                        )
                    };
                    let running = entries
                        .iter()
                        .filter(|entry| {
                            entry.ServiceStatusProcess.dwCurrentState == SERVICE_RUNNING
                        })
                        .count() as u32;
                    return Ok(ServiceStateSummary {
                        total: returned,
                        running,
                    });
                }
                Err(error) if is_buffer_too_small(&error) => {
                    capacity = capacity.saturating_add(needed.max(4096) as usize);
                }
                Err(error) => return Err(from_win32("enumerate Windows services", error)),
            }
        }

        Err(MinWinError::windows(
            "enumerate Windows services",
            "the service list kept growing between sizing and reading",
            ERROR_MORE_DATA.0,
        ))
    }
}

/// The SCM reports "your buffer is too small" with two different codes
/// depending on the call: `QueryServiceConfig*` uses
/// `ERROR_INSUFFICIENT_BUFFER` while `EnumServicesStatusEx` uses
/// `ERROR_MORE_DATA`. Both mean "ask again with more room".
fn is_buffer_too_small(error: &windows::core::Error) -> bool {
    error.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult()
        || error.code() == ERROR_MORE_DATA.to_hresult()
}

fn delayed_auto_start(service: &ScHandle, service_name: &str) -> Result<bool> {
    let mut buffer = vec![0u8; std::mem::size_of::<SERVICE_DELAYED_AUTO_START_INFO>()];
    let mut needed = 0u32;
    // SAFETY: the buffer is sized for the requested information class.
    unsafe {
        QueryServiceConfig2W(
            service.0,
            SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
            Some(&mut buffer),
            &mut needed,
        )
    }
    .map_err(|e| {
        from_win32(
            format!(r#"read the delayed-start flag of service "{service_name}""#),
            e,
        )
    })?;

    // SAFETY: Windows populated the buffer with this single-field struct.
    let info = unsafe { &*buffer.as_ptr().cast::<SERVICE_DELAYED_AUTO_START_INFO>() };
    Ok(info.fDelayedAutostart.as_bool())
}

fn current_state(service: &ScHandle, service_name: &str) -> Result<ServiceRunState> {
    use windows::Win32::System::Services::{
        QueryServiceStatusEx, SC_STATUS_PROCESS_INFO, SERVICE_STATUS_PROCESS,
    };

    let mut buffer = vec![0u8; std::mem::size_of::<SERVICE_STATUS_PROCESS>()];
    let mut needed = 0u32;
    // SAFETY: the buffer is sized for SERVICE_STATUS_PROCESS.
    unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            Some(&mut buffer),
            &mut needed,
        )
    }
    .map_err(|e| from_win32(format!(r#"read the state of service "{service_name}""#), e))?;

    // SAFETY: Windows populated the buffer with this layout.
    let status = unsafe { &*buffer.as_ptr().cast::<SERVICE_STATUS_PROCESS>() };
    Ok(decode_run_state(status.dwCurrentState))
}

fn decode_start_type(raw: SERVICE_START_TYPE) -> ServiceStartType {
    match raw {
        SERVICE_BOOT_START => ServiceStartType::Boot,
        SERVICE_SYSTEM_START => ServiceStartType::System,
        SERVICE_AUTO_START => ServiceStartType::Automatic,
        SERVICE_DEMAND_START => ServiceStartType::Manual,
        SERVICE_DISABLED => ServiceStartType::Disabled,
        // The set is closed by the API contract; treat anything else as the
        // most restrictive interpretation rather than guessing.
        _ => ServiceStartType::Disabled,
    }
}

fn decode_run_state(raw: SERVICE_STATUS_CURRENT_STATE) -> ServiceRunState {
    match raw {
        SERVICE_STOPPED => ServiceRunState::Stopped,
        SERVICE_START_PENDING => ServiceRunState::StartPending,
        SERVICE_STOP_PENDING => ServiceRunState::StopPending,
        SERVICE_RUNNING => ServiceRunState::Running,
        SERVICE_CONTINUE_PENDING => ServiceRunState::ContinuePending,
        SERVICE_PAUSE_PENDING => ServiceRunState::PausePending,
        SERVICE_PAUSED => ServiceRunState::Paused,
        _ => ServiceRunState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_types_decode_from_the_documented_constants() {
        assert_eq!(
            decode_start_type(SERVICE_AUTO_START),
            ServiceStartType::Automatic
        );
        assert_eq!(
            decode_start_type(SERVICE_DEMAND_START),
            ServiceStartType::Manual
        );
        assert_eq!(
            decode_start_type(SERVICE_DISABLED),
            ServiceStartType::Disabled
        );
        assert_eq!(
            decode_start_type(SERVICE_BOOT_START),
            ServiceStartType::Boot
        );
    }

    #[test]
    fn run_states_decode_from_the_documented_constants() {
        assert_eq!(decode_run_state(SERVICE_RUNNING), ServiceRunState::Running);
        assert_eq!(decode_run_state(SERVICE_STOPPED), ServiceRunState::Stopped);
        assert_eq!(
            decode_run_state(SERVICE_STATUS_CURRENT_STATE(99)),
            ServiceRunState::Unknown
        );
    }
}
