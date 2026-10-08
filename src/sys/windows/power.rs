//! Power scheme access through `powrprof`.
//!
//! MinWin uses `PowerGetActiveScheme` / `PowerSetActiveScheme` rather than
//! shelling out to `powercfg`, because the API returns structured data and
//! avoids parsing localised console output.
//!
//! Only the *active scheme* is read and written. MinWin does not edit the
//! individual settings inside a scheme, so it cannot leave a user with a
//! modified "Balanced" plan that no longer matches its name.

use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Power::{
    ACCESS_SCHEME, PowerEnumerate, PowerGetActiveScheme, PowerReadFriendlyName,
    PowerSetActiveScheme,
};
use windows::core::GUID;

use crate::core::error::{Result, from_win32_code};
use crate::sys::model::{PowerScheme, PowerSchemeId};
use crate::sys::traits::PowerManager;

#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsPowerManager;

fn to_guid(id: &PowerSchemeId) -> GUID {
    let (data1, data2, data3, data4) = id.to_fields();
    GUID {
        data1,
        data2,
        data3,
        data4,
    }
}

fn from_guid(guid: &GUID) -> PowerSchemeId {
    PowerSchemeId::from_fields(guid.data1, guid.data2, guid.data3, guid.data4)
}

impl PowerManager for WindowsPowerManager {
    fn active_scheme(&self) -> Result<PowerScheme> {
        let mut raw: *mut GUID = std::ptr::null_mut();
        // SAFETY: `raw` receives a CoTaskMemAlloc'd GUID which is freed below.
        let code = unsafe { PowerGetActiveScheme(None, &mut raw) };
        if code != ERROR_SUCCESS {
            return Err(from_win32_code("read the active power plan", code.0));
        }
        if raw.is_null() {
            return Err(from_win32_code(
                "read the active power plan",
                ERROR_SUCCESS.0,
            ));
        }
        // SAFETY: Windows guarantees a valid GUID on success.
        let id = from_guid(unsafe { &*raw });
        // SAFETY: the buffer was allocated by PowerGetActiveScheme.
        unsafe { CoTaskMemFree(Some(raw.cast())) };

        let name = friendly_name(&id)?;
        Ok(PowerScheme { id, name })
    }

    fn list_schemes(&self) -> Result<Vec<PowerScheme>> {
        let mut schemes = Vec::new();
        let mut index = 0u32;
        loop {
            let mut buffer = [0u8; std::mem::size_of::<GUID>()];
            let mut size = buffer.len() as u32;
            // SAFETY: ACCESS_SCHEME enumerates scheme GUIDs into a
            // GUID-sized buffer, per the documented contract.
            let code = unsafe {
                PowerEnumerate(
                    None,
                    None,
                    None,
                    ACCESS_SCHEME,
                    index,
                    Some(buffer.as_mut_ptr()),
                    &mut size,
                )
            };
            if code == ERROR_NO_MORE_ITEMS {
                break;
            }
            if code != ERROR_SUCCESS {
                return Err(from_win32_code(
                    "enumerate the installed power plans",
                    code.0,
                ));
            }

            // SAFETY: Windows wrote a GUID into the buffer.
            let guid = unsafe { buffer.as_ptr().cast::<GUID>().read_unaligned() };
            let id = from_guid(&guid);
            let name = friendly_name(&id)?;
            schemes.push(PowerScheme { id, name });

            index += 1;
            // A machine with this many power plans means something is wrong;
            // refuse to spin forever.
            if index > 256 {
                break;
            }
        }
        Ok(schemes)
    }

    fn set_active_scheme(&self, id: &PowerSchemeId) -> Result<()> {
        let guid = to_guid(id);
        // SAFETY: `guid` outlives the call.
        let code = unsafe { PowerSetActiveScheme(None, Some(&guid)) };
        if code != ERROR_SUCCESS {
            return Err(from_win32_code(
                format!("activate the power plan {id}"),
                code.0,
            ));
        }
        Ok(())
    }
}

/// Reads a scheme's localised friendly name.
fn friendly_name(id: &PowerSchemeId) -> Result<String> {
    let guid = to_guid(id);
    let mut size = 0u32;
    // SAFETY: a null buffer with a valid size pointer asks for the size.
    let code = unsafe { PowerReadFriendlyName(None, Some(&guid), None, None, None, &mut size) };
    if code != ERROR_SUCCESS && code != ERROR_MORE_DATA {
        return Err(from_win32_code(
            format!("read the name of power plan {id}"),
            code.0,
        ));
    }

    let mut buffer = vec![0u8; size as usize];
    // SAFETY: the buffer is the size Windows just reported.
    let code = unsafe {
        PowerReadFriendlyName(
            None,
            Some(&guid),
            None,
            None,
            Some(buffer.as_mut_ptr()),
            &mut size,
        )
    };
    if code != ERROR_SUCCESS {
        return Err(from_win32_code(
            format!("read the name of power plan {id}"),
            code.0,
        ));
    }

    Ok(decode_utf16(&buffer))
}

fn decode_utf16(buffer: &[u8]) -> String {
    let units: Vec<u16> = buffer
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// The three power plans Windows ships, as documented constants rather than
/// literals copied from a forum post.
pub mod well_known {
    use crate::sys::model::PowerSchemeId;
    use windows::Win32::System::SystemServices::{
        GUID_MAX_POWER_SAVINGS, GUID_MIN_POWER_SAVINGS, GUID_TYPICAL_POWER_SAVINGS,
    };

    /// "High performance" — `GUID_MIN_POWER_SAVINGS`.
    pub fn high_performance() -> PowerSchemeId {
        from_guid(&GUID_MIN_POWER_SAVINGS)
    }

    /// "Balanced" — `GUID_TYPICAL_POWER_SAVINGS`.
    pub fn balanced() -> PowerSchemeId {
        from_guid(&GUID_TYPICAL_POWER_SAVINGS)
    }

    /// "Power saver" — `GUID_MAX_POWER_SAVINGS`.
    pub fn power_saver() -> PowerSchemeId {
        from_guid(&GUID_MAX_POWER_SAVINGS)
    }

    fn from_guid(guid: &windows::core::GUID) -> PowerSchemeId {
        PowerSchemeId::from_fields(guid.data1, guid.data2, guid.data3, guid.data4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_plans_match_the_documented_guids() {
        assert_eq!(
            well_known::high_performance().as_str(),
            "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c"
        );
        assert_eq!(
            well_known::balanced().as_str(),
            "381b4222-f694-41f0-9685-ff5bb260df2e"
        );
        assert_eq!(
            well_known::power_saver().as_str(),
            "a1841308-3541-4fab-bc81-f71556f20b4a"
        );
    }

    #[test]
    fn guid_conversion_round_trips() {
        let id = well_known::high_performance();
        assert_eq!(from_guid(&to_guid(&id)), id);
    }
}
