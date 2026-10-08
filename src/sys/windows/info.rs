//! Windows identity, elevation, memory and uptime.

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::SystemInformation::{
    GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX, OSVERSIONINFOW,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::core::error::{MinWinError, Result, from_win32};
use crate::sys::model::{ManagementState, MemorySnapshot, RegistryRoot, RegistryValue};
use crate::sys::traits::RegistryStore;
use crate::sys::windows::registry::WindowsRegistry;

const CURRENT_VERSION_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

/// Reads the Windows version.
///
/// `RtlGetVersion` is used for the numeric version because, unlike
/// `GetVersionEx`, it is not subject to application-compatibility shimming and
/// reports the real build. The human-facing strings (`DisplayVersion` such as
/// `24H2`, `ProductName`, and the UBR) have no API and are read from the
/// documented `CurrentVersion` registry key.
pub fn windows_version(registry: &WindowsRegistry) -> Result<crate::sys::model::WindowsVersion> {
    let mut raw = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: `raw` is a correctly sized, zero-initialised OSVERSIONINFOW.
    let status = unsafe { windows::Wdk::System::SystemServices::RtlGetVersion(&mut raw) };
    if status.is_err() {
        return Err(MinWinError::windows(
            "read the Windows version with RtlGetVersion",
            "RtlGetVersion returned a failure status",
            status.0 as u32,
        ));
    }

    let display_version = match registry.read_value(
        RegistryRoot::LocalMachine,
        CURRENT_VERSION_KEY,
        "DisplayVersion",
    )? {
        Some(RegistryValue::Text(value)) if !value.is_empty() => Some(value),
        _ => None,
    };
    let product_name = match registry.read_value(
        RegistryRoot::LocalMachine,
        CURRENT_VERSION_KEY,
        "ProductName",
    )? {
        Some(RegistryValue::Text(value)) if !value.is_empty() => value,
        _ => "Windows".to_string(),
    };
    let revision = registry
        .read_value(RegistryRoot::LocalMachine, CURRENT_VERSION_KEY, "UBR")?
        .and_then(|value| value.as_dword());

    Ok(crate::sys::model::WindowsVersion {
        product_name,
        display_version,
        major: raw.dwMajorVersion,
        minor: raw.dwMinorVersion,
        build: raw.dwBuildNumber,
        revision,
    })
}

/// Whether this process runs with an elevated token.
pub fn is_elevated() -> Result<bool> {
    let mut token = HANDLE::default();
    // SAFETY: pseudo-handle from GetCurrentProcess needs no closing; `token`
    // is closed below on both paths.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|e| from_win32("open this process's access token", e))?;

    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    // SAFETY: the buffer matches the TokenElevation information class.
    let result = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    // SAFETY: `token` came from OpenProcessToken and is closed exactly once.
    unsafe {
        let _ = CloseHandle(token);
    }
    result.map_err(|e| from_win32("query the elevation state of this process", e))?;
    Ok(elevation.TokenIsElevated != 0)
}

pub fn memory() -> Result<MemorySnapshot> {
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` is correctly sized per the API contract.
    unsafe { GlobalMemoryStatusEx(&mut status) }
        .map_err(|e| from_win32("read system memory status", e))?;

    Ok(MemorySnapshot {
        total_physical_bytes: status.ullTotalPhys,
        available_physical_bytes: status.ullAvailPhys,
        load_percent: status.dwMemoryLoad.min(100) as u8,
    })
}

pub fn uptime_seconds() -> u64 {
    // SAFETY: GetTickCount64 takes no arguments and cannot fail.
    unsafe { GetTickCount64() / 1000 }
}

/// Best-effort detection of central management.
///
/// Both signals are used only to *decline* changes, never to enable them, so a
/// false positive costs the user a skipped optimisation while a false negative
/// is the case MinWin must avoid. Where a read fails, MinWin propagates the
/// error rather than assuming the machine is unmanaged.
pub fn management_state(registry: &WindowsRegistry) -> Result<ManagementState> {
    Ok(ManagementState {
        domain_joined: is_domain_joined()?,
        defender_for_endpoint_onboarded: is_mde_onboarded(registry)?,
    })
}

fn is_domain_joined() -> Result<bool> {
    use windows::Win32::NetworkManagement::NetManagement::{
        NetApiBufferFree, NetGetJoinInformation, NetSetupDomainName,
    };
    use windows::core::PWSTR;

    let mut name = PWSTR::null();
    let mut join_status = Default::default();
    // SAFETY: both out-parameters are valid; the returned buffer is freed
    // below regardless of the status value.
    let code = unsafe { NetGetJoinInformation(None, &mut name, &mut join_status) };
    if !name.is_null() {
        // SAFETY: the buffer was allocated by NetGetJoinInformation.
        unsafe { NetApiBufferFree(Some(name.as_ptr().cast())) };
    }
    if code != 0 {
        return Err(MinWinError::windows(
            "determine whether this device is domain joined",
            "NetGetJoinInformation failed",
            code,
        ));
    }
    Ok(join_status == NetSetupDomainName)
}

/// Microsoft Defender for Endpoint writes its onboarding state under this key.
///
/// MinWin checks it because Microsoft's own Defender for Endpoint
/// troubleshooting guidance expects the Connected User Experiences and
/// Telemetry service to be set to automatic start on onboarded devices.
fn is_mde_onboarded(registry: &WindowsRegistry) -> Result<bool> {
    const KEYS: [&str; 2] = [
        r"SOFTWARE\Microsoft\Windows Advanced Threat Protection\Status",
        r"SOFTWARE\Microsoft\Windows Advanced Threat Protection",
    ];
    for key in KEYS {
        if let Some(value) =
            registry.read_value(RegistryRoot::LocalMachine, key, "OnboardingState")?
            && value.as_dword() == Some(1)
        {
            return Ok(true);
        }
    }
    Ok(false)
}
