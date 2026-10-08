//! Plain data describing machine state.
//!
//! Nothing in this module calls Windows. These types are the only vocabulary
//! the rest of MinWin uses, which is what lets the whole engine be tested
//! against a fake machine.

use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// Windows identity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsVersion {
    /// e.g. `Windows 11 Pro`, read from the registry's `ProductName`. On
    /// Windows 11 this still reads "Windows 10 ..." in some builds, so
    /// `product_line` below is derived from the build number instead.
    pub product_name: String,
    /// e.g. `24H2`, from `DisplayVersion`. `None` on builds that predate it.
    pub display_version: Option<String>,
    pub major: u32,
    pub minor: u32,
    pub build: u32,
    /// Update Build Revision — the fourth component of the full version.
    pub revision: Option<u32>,
}

impl WindowsVersion {
    /// Windows 11 is NT 10.0 with a build number of 22000 or higher. Microsoft
    /// did not bump the major version, so the build is the only reliable
    /// discriminator.
    pub fn is_windows_11(&self) -> bool {
        self.major == 10 && self.build >= 22000
    }

    pub fn is_windows_10_or_newer(&self) -> bool {
        self.major > 10 || (self.major == 10 && self.build >= 10240)
    }

    /// The marketing line, derived from the build rather than trusted from
    /// `ProductName`.
    pub fn product_line(&self) -> &'static str {
        if self.is_windows_11() {
            "Windows 11"
        } else if self.is_windows_10_or_newer() {
            "Windows 10"
        } else {
            "Windows (pre-10)"
        }
    }

    /// `Windows 11 24H2` — what `minwin status` prints.
    pub fn label(&self) -> String {
        match &self.display_version {
            Some(display) => format!("{} {}", self.product_line(), display),
            None => format!("{} (build {})", self.product_line(), self.build),
        }
    }
}

impl fmt::Display for WindowsVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (build {})", self.label(), self.build)
    }
}

/// Facts that determine whether a change may be applied at all, beyond the
/// Windows version. All three are best-effort reads; MinWin treats a positive
/// detection as a reason to *decline* a change, never as a reason to proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ManagementState {
    pub domain_joined: bool,
    /// Microsoft Defender for Endpoint reports an onboarded sensor.
    pub defender_for_endpoint_onboarded: bool,
}

impl ManagementState {
    /// True when something other than the local user is responsible for this
    /// machine's configuration.
    pub fn is_centrally_managed(&self) -> bool {
        self.domain_joined || self.defender_for_endpoint_onboarded
    }

    pub fn describe(&self) -> String {
        let mut reasons = Vec::new();
        if self.domain_joined {
            reasons.push("the device is joined to an Active Directory domain");
        }
        if self.defender_for_endpoint_onboarded {
            reasons.push("Microsoft Defender for Endpoint is onboarded on this device");
        }
        if reasons.is_empty() {
            "not centrally managed".to_string()
        } else {
            reasons.join(" and ")
        }
    }
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MemorySnapshot {
    pub total_physical_bytes: u64,
    pub available_physical_bytes: u64,
    /// Windows' own `dwMemoryLoad`: approximate percentage of physical memory
    /// in use.
    pub load_percent: u8,
}

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceStartType {
    Boot,
    System,
    Automatic,
    /// `Automatic` plus the delayed-start flag. Modelled separately because
    /// restoring a delayed-auto service as plain `Automatic` would silently
    /// change its boot behaviour — a rollback bug MinWin refuses to have.
    AutomaticDelayed,
    Manual,
    Disabled,
}

impl ServiceStartType {
    pub fn label(self) -> &'static str {
        match self {
            Self::Boot => "Boot",
            Self::System => "System",
            Self::Automatic => "Automatic",
            Self::AutomaticDelayed => "Automatic (delayed start)",
            Self::Manual => "Manual",
            Self::Disabled => "Disabled",
        }
    }

    /// Start types MinWin will not reconfigure: a driver or kernel-time
    /// service is outside the product's remit.
    pub fn is_kernel_stage(self) -> bool {
        matches!(self, Self::Boot | Self::System)
    }
}

impl fmt::Display for ServiceStartType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceRunState {
    Stopped,
    StartPending,
    StopPending,
    Running,
    ContinuePending,
    PausePending,
    Paused,
    Unknown,
}

impl ServiceRunState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::StartPending => "starting",
            Self::StopPending => "stopping",
            Self::Running => "running",
            Self::ContinuePending => "resuming",
            Self::PausePending => "pausing",
            Self::Paused => "paused",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub name: String,
    pub display_name: String,
    pub start_type: ServiceStartType,
    pub run_state: ServiceRunState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ServiceStateSummary {
    pub total: u32,
    pub running: u32,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryRoot {
    LocalMachine,
    CurrentUser,
}

impl RegistryRoot {
    pub fn short_name(self) -> &'static str {
        match self {
            Self::LocalMachine => "HKLM",
            Self::CurrentUser => "HKCU",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RegistryValue {
    Dword(u32),
    Text(String),
    /// A value type MinWin can read the existence of but does not model. Kept
    /// so that inspection reports "present but of an unexpected type" rather
    /// than pretending the value is absent.
    Other {
        type_id: u32,
    },
}

impl RegistryValue {
    pub fn as_dword(&self) -> Option<u32> {
        match self {
            Self::Dword(value) => Some(*value),
            _ => None,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Dword(value) => format!("{value} (DWORD)"),
            Self::Text(value) => format!("{value:?} (string)"),
            Self::Other { type_id } => format!("value of unmodelled registry type {type_id}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Power
// ---------------------------------------------------------------------------

/// A power scheme GUID in canonical lowercase hyphenated form.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PowerSchemeId(String);

impl PowerSchemeId {
    /// Parses a GUID, accepting optional surrounding braces and any case.
    pub fn parse(text: &str) -> Option<Self> {
        let trimmed = text.trim().trim_start_matches('{').trim_end_matches('}');
        let groups: Vec<&str> = trimmed.split('-').collect();
        const WIDTHS: [usize; 5] = [8, 4, 4, 4, 12];
        if groups.len() != WIDTHS.len() {
            return None;
        }
        for (group, width) in groups.iter().zip(WIDTHS) {
            if group.len() != width || !group.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
        }
        Some(Self(trimmed.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The 16 bytes in the field order Windows' `GUID` struct uses.
    pub fn to_fields(&self) -> (u32, u16, u16, [u8; 8]) {
        let groups: Vec<&str> = self.0.split('-').collect();
        // Already validated by `parse`, so these conversions cannot fail.
        let data1 = u32::from_str_radix(groups[0], 16).unwrap_or_default();
        let data2 = u16::from_str_radix(groups[1], 16).unwrap_or_default();
        let data3 = u16::from_str_radix(groups[2], 16).unwrap_or_default();
        let mut data4 = [0u8; 8];
        let tail: String = format!("{}{}", groups[3], groups[4]);
        for (index, slot) in data4.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&tail[index * 2..index * 2 + 2], 16).unwrap_or_default();
        }
        (data1, data2, data3, data4)
    }

    pub fn from_fields(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        let tail: String = data4.iter().map(|b| format!("{b:02x}")).collect();
        Self(format!(
            "{data1:08x}-{data2:04x}-{data3:04x}-{}-{}",
            &tail[0..4],
            &tail[4..16]
        ))
    }
}

impl fmt::Display for PowerSchemeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerScheme {
    pub id: PowerSchemeId,
    /// Windows' friendly name for the scheme, which is localised.
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(build: u32, display: Option<&str>) -> WindowsVersion {
        WindowsVersion {
            product_name: "Windows 10 Pro".into(),
            display_version: display.map(str::to_string),
            major: 10,
            minor: 0,
            build,
            revision: Some(1742),
        }
    }

    #[test]
    fn windows_11_is_detected_from_the_build_not_the_product_name() {
        let eleven = version(26100, Some("24H2"));
        assert!(eleven.is_windows_11());
        assert_eq!(eleven.product_line(), "Windows 11");
        assert_eq!(eleven.label(), "Windows 11 24H2");

        let ten = version(19045, Some("22H2"));
        assert!(!ten.is_windows_11());
        assert!(ten.is_windows_10_or_newer());
        assert_eq!(ten.product_line(), "Windows 10");
    }

    #[test]
    fn a_missing_display_version_falls_back_to_the_build() {
        assert_eq!(version(22000, None).label(), "Windows 11 (build 22000)");
    }

    #[test]
    fn management_state_explains_itself() {
        let clean = ManagementState::default();
        assert!(!clean.is_centrally_managed());
        assert_eq!(clean.describe(), "not centrally managed");

        let managed = ManagementState {
            domain_joined: true,
            defender_for_endpoint_onboarded: true,
        };
        assert!(managed.is_centrally_managed());
        assert!(managed.describe().contains("Active Directory"));
        assert!(managed.describe().contains("Defender for Endpoint"));
    }

    #[test]
    fn power_scheme_ids_round_trip_through_guid_fields() {
        // The documented High performance scheme.
        let id = PowerSchemeId::parse("8C5E7FDA-E8BF-4A96-9A85-A6E23A8C635C").expect("parse");
        assert_eq!(id.as_str(), "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c");

        let (d1, d2, d3, d4) = id.to_fields();
        assert_eq!(d1, 0x8C5E_7FDA);
        assert_eq!(d2, 0xE8BF);
        assert_eq!(d3, 0x4A96);
        assert_eq!(d4, [0x9A, 0x85, 0xA6, 0xE2, 0x3A, 0x8C, 0x63, 0x5C]);
        assert_eq!(PowerSchemeId::from_fields(d1, d2, d3, d4), id);
    }

    #[test]
    fn braced_guids_are_accepted_and_junk_is_rejected() {
        assert!(PowerSchemeId::parse("{381b4222-f694-41f0-9685-ff5bb260df2e}").is_some());
        assert!(PowerSchemeId::parse("not-a-guid").is_none());
        assert!(PowerSchemeId::parse("381b4222-f694-41f0-9685-ff5bb260df2").is_none());
        assert!(PowerSchemeId::parse("381b4222-f694-41f0-9685-ff5bb260dfzz").is_none());
        assert!(PowerSchemeId::parse("").is_none());
    }

    #[test]
    fn kernel_stage_start_types_are_flagged() {
        assert!(ServiceStartType::Boot.is_kernel_stage());
        assert!(ServiceStartType::System.is_kernel_stage());
        assert!(!ServiceStartType::Automatic.is_kernel_stage());
        assert!(!ServiceStartType::Manual.is_kernel_stage());
    }
}
