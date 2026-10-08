//! Registry access via the documented `Reg*` APIs.
//!
//! Scope is deliberately narrow: read a value, write a DWORD, delete a value.
//! There is no "write whatever this string says" entry point, because the only
//! callers are change implementations holding `&'static str` key paths.

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD,
    REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW,
    RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::HSTRING;

use crate::core::error::{Result, from_win32_code};
use crate::sys::model::{RegistryRoot, RegistryValue};
use crate::sys::traits::RegistryStore;

#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsRegistry;

/// Closes an `HKEY` on drop so that every early return releases the handle.
struct OpenKey(HKEY);

impl Drop for OpenKey {
    fn drop(&mut self) {
        // SAFETY: the handle came from RegOpenKeyExW/RegCreateKeyExW and is
        // closed exactly once.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn root_handle(root: RegistryRoot) -> HKEY {
    match root {
        RegistryRoot::LocalMachine => HKEY_LOCAL_MACHINE,
        RegistryRoot::CurrentUser => HKEY_CURRENT_USER,
    }
}

fn describe(root: RegistryRoot, subkey: &str, value_name: &str) -> String {
    format!(r#"{}\{} value "{}""#, root.short_name(), subkey, value_name)
}

impl RegistryStore for WindowsRegistry {
    fn read_value(
        &self,
        root: RegistryRoot,
        subkey: &str,
        value_name: &str,
    ) -> Result<Option<RegistryValue>> {
        let mut key = HKEY::default();
        let subkey_wide = HSTRING::from(subkey);
        // SAFETY: `subkey_wide` outlives the call; `key` receives the handle.
        let code = unsafe {
            RegOpenKeyExW(
                root_handle(root),
                &subkey_wide,
                None,
                KEY_QUERY_VALUE,
                &mut key,
            )
        };
        if code == ERROR_FILE_NOT_FOUND {
            // An absent key means an absent value, which is a normal state for
            // policy keys that have never been configured.
            return Ok(None);
        }
        if code != ERROR_SUCCESS {
            return Err(from_win32_code(
                format!(r"open registry key {}\{}", root.short_name(), subkey),
                code.0,
            ));
        }
        let key = OpenKey(key);
        let name_wide = HSTRING::from(value_name);

        let mut value_type = REG_VALUE_TYPE::default();
        let mut size = 0u32;
        // SAFETY: a null data pointer with a valid size pointer is the
        // documented way to ask for the required buffer size.
        let code = unsafe {
            RegQueryValueExW(
                key.0,
                &name_wide,
                None,
                Some(&mut value_type),
                None,
                Some(&mut size),
            )
        };
        if code == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if code != ERROR_SUCCESS {
            return Err(from_win32_code(
                format!("size {}", describe(root, subkey, value_name)),
                code.0,
            ));
        }

        let mut buffer = vec![0u8; size as usize];
        // SAFETY: the buffer is exactly the size Windows just reported.
        let code = unsafe {
            RegQueryValueExW(
                key.0,
                &name_wide,
                None,
                Some(&mut value_type),
                Some(buffer.as_mut_ptr()),
                Some(&mut size),
            )
        };
        if code != ERROR_SUCCESS {
            return Err(from_win32_code(
                format!("read {}", describe(root, subkey, value_name)),
                code.0,
            ));
        }
        buffer.truncate(size as usize);

        Ok(Some(decode_value(value_type, &buffer)))
    }

    fn write_dword(
        &self,
        root: RegistryRoot,
        subkey: &str,
        value_name: &str,
        value: u32,
    ) -> Result<()> {
        let mut key = HKEY::default();
        let subkey_wide = HSTRING::from(subkey);
        // SAFETY: out-parameter `key` receives the created/opened handle.
        let code = unsafe {
            RegCreateKeyExW(
                root_handle(root),
                &subkey_wide,
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(from_win32_code(
                format!(
                    r"create or open registry key {}\{}",
                    root.short_name(),
                    subkey
                ),
                code.0,
            ));
        }
        let key = OpenKey(key);

        let name_wide = HSTRING::from(value_name);
        let bytes = value.to_le_bytes();
        // SAFETY: a 4-byte little-endian buffer is the required layout for
        // REG_DWORD.
        let code = unsafe { RegSetValueExW(key.0, &name_wide, None, REG_DWORD, Some(&bytes)) };
        if code != ERROR_SUCCESS {
            return Err(from_win32_code(
                format!("write {}", describe(root, subkey, value_name)),
                code.0,
            ));
        }
        Ok(())
    }

    fn delete_value(&self, root: RegistryRoot, subkey: &str, value_name: &str) -> Result<()> {
        let mut key = HKEY::default();
        let subkey_wide = HSTRING::from(subkey);
        // SAFETY: out-parameter `key` receives the handle.
        let code = unsafe {
            RegOpenKeyExW(
                root_handle(root),
                &subkey_wide,
                None,
                KEY_SET_VALUE,
                &mut key,
            )
        };
        if code == ERROR_FILE_NOT_FOUND {
            // Nothing to delete: restoring "absent" is already satisfied.
            return Ok(());
        }
        if code != ERROR_SUCCESS {
            return Err(from_win32_code(
                format!(
                    r"open registry key {}\{} for writing",
                    root.short_name(),
                    subkey
                ),
                code.0,
            ));
        }
        let key = OpenKey(key);

        let name_wide = HSTRING::from(value_name);
        // SAFETY: `name_wide` outlives the call.
        let code = unsafe { RegDeleteValueW(key.0, &name_wide) };
        if code == ERROR_SUCCESS || code == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        Err(from_win32_code(
            format!("delete {}", describe(root, subkey, value_name)),
            code.0,
        ))
    }
}

fn decode_value(value_type: REG_VALUE_TYPE, buffer: &[u8]) -> RegistryValue {
    if value_type == REG_DWORD {
        let mut bytes = [0u8; 4];
        let len = buffer.len().min(4);
        bytes[..len].copy_from_slice(&buffer[..len]);
        return RegistryValue::Dword(u32::from_le_bytes(bytes));
    }
    if value_type == REG_SZ || value_type == REG_EXPAND_SZ {
        return RegistryValue::Text(decode_utf16(buffer));
    }
    RegistryValue::Other {
        type_id: value_type.0,
    }
}

/// Decodes a `REG_SZ` payload, which is UTF-16LE and may or may not include a
/// trailing NUL.
fn decode_utf16(buffer: &[u8]) -> String {
    let units: Vec<u16> = buffer
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dword_payloads_are_decoded_little_endian() {
        assert_eq!(
            decode_value(REG_DWORD, &1u32.to_le_bytes()),
            RegistryValue::Dword(1)
        );
        assert_eq!(
            decode_value(REG_DWORD, &0u32.to_le_bytes()),
            RegistryValue::Dword(0)
        );
    }

    #[test]
    fn strings_stop_at_the_trailing_nul() {
        let mut bytes = Vec::new();
        for unit in "24H2".encode_utf16().chain(std::iter::once(0)) {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(
            decode_value(REG_SZ, &bytes),
            RegistryValue::Text("24H2".into())
        );
    }

    #[test]
    fn unmodelled_types_are_reported_rather_than_dropped() {
        // REG_BINARY is 3.
        assert_eq!(
            decode_value(REG_VALUE_TYPE(3), &[1, 2, 3]),
            RegistryValue::Other { type_id: 3 }
        );
    }
}
