//! The changes MinWin supports, and the contract they implement.
//!
//! # Why this list is short
//!
//! Every entry below had to clear the same bar: Microsoft documents the
//! mechanism, MinWin can read the current value, write the new one, read it
//! back to confirm, and restore the original exactly — including the case
//! where "the original" was *no value at all*. Anything that could not clear
//! that bar is listed in `docs/future-changes.md` with the reason, rather than
//! shipped with a disclaimer.
//!
//! Four changes is the honest size of that set for v0.1.
//!
//! # What MinWin will not do
//!
//! Not as a configuration option, not behind a flag, not with a warning:
//! Windows Defender, the firewall, Windows Update, UAC, SmartScreen, Memory
//! Integrity, credential protections, network stack services and driver
//! services are out of scope. Security is not bloat. There is no code in this
//! crate that can reach them, which is a stronger guarantee than a policy
//! statement: the only registry paths and service names MinWin can address are
//! the `&'static str` constants in this file.

pub mod model;
pub mod policy_dword;
pub mod power_plan;
pub mod service_start_type;

use crate::core::error::{MinWinError, Result};
use crate::sys::SystemFacts;
use crate::sys::model::{RegistryRoot, ServiceStartType};
use crate::sys::traits::Machine;

pub use model::{
    Applicability, ApplyResult, ChangeCategory, ChangeMetadata, ChangePlan, ObservedState,
    PlanAction, RebootRequirement, Risk, RollbackData, RollbackResult, VerificationResult,
};

use model::{ChangeCategory as Category, Risk as RiskLevel};
use policy_dword::PolicyDwordChange;
use power_plan::PowerPlanChange;
use service_start_type::{ManagementRule, ServiceStartTypeChange};

/// One reversible, explainable system change.
///
/// The stages are separate methods rather than one `apply()` because MinWin
/// needs to run some of them without the others: `--dry-run` stops after
/// `plan`, `diff` uses only `inspect`, and `rollback` needs `rollback` without
/// ever having called `plan` in this process.
///
/// Implementations must hold no mutable state: a change is a description of an
/// intention, and the machine is the only thing that changes.
pub trait SystemChange: Send + Sync {
    fn metadata(&self) -> &ChangeMetadata;

    fn id(&self) -> &'static str {
        self.metadata().id
    }

    /// May MinWin apply this on this machine, right now? Returning
    /// `NotApplicable` is a normal outcome and must not be an error.
    fn check_applicability(
        &self,
        machine: &dyn Machine,
        facts: &SystemFacts,
    ) -> Result<Applicability>;

    /// Read the current value. Must not write anything.
    fn inspect(&self, machine: &dyn Machine, facts: &SystemFacts) -> Result<ObservedState>;

    /// Decide what to do, and how to undo it. Must not write anything.
    fn plan(&self, machine: &dyn Machine, facts: &SystemFacts) -> Result<ChangePlan>;

    /// Perform the write described by `plan`.
    fn apply(
        &self,
        machine: &dyn Machine,
        facts: &SystemFacts,
        plan: &ChangePlan,
    ) -> Result<ApplyResult>;

    /// Read the machine back and say whether it matches `expected`.
    ///
    /// Returns `Ok(Unverifiable)` rather than `Err` when the read itself fails,
    /// so that a verification problem is recorded against the change instead of
    /// aborting the session.
    fn verify(
        &self,
        machine: &dyn Machine,
        facts: &SystemFacts,
        expected: &ObservedState,
    ) -> Result<VerificationResult>;

    /// Restore the state captured in `rollback`.
    fn rollback(
        &self,
        machine: &dyn Machine,
        facts: &SystemFacts,
        rollback: &RollbackData,
    ) -> Result<RollbackResult>;
}

// ---------------------------------------------------------------------------
// Change ids
//
// These strings are persisted in the state database and referenced by profile
// files, so they are part of MinWin's public surface. Renaming one orphans
// stored sessions and breaks existing profiles.
// ---------------------------------------------------------------------------

pub const DIAGTRACK_START_TYPE: &str = "telemetry.diagtrack_start_type";
pub const DELIVERY_OPTIMIZATION_DOWNLOAD_MODE: &str = "update.delivery_optimization_download_mode";
pub const SYSMAIN_START_TYPE: &str = "memory.sysmain_start_type";
pub const POWER_PLAN_HIGH_PERFORMANCE: &str = "power.active_plan_high_performance";

/// Windows 11's first build. MinWin validates against Windows 11 and declines
/// to apply changes on anything older rather than assuming they behave the
/// same way.
const WINDOWS_11_BUILD: u32 = 22000;

/// Renders a Delivery Optimization download mode in the terms Microsoft's
/// documentation uses.
fn describe_download_mode(value: Option<u32>) -> String {
    match value {
        None => "not configured (Windows uses its own default, LAN peering)".to_string(),
        Some(0) => "0 (HTTP only, no peer-to-peer)".to_string(),
        Some(1) => "1 (HTTP plus peering on the local network)".to_string(),
        Some(2) => "2 (HTTP plus peering across a private group)".to_string(),
        Some(3) => "3 (HTTP plus peering with devices on the internet)".to_string(),
        Some(99) => "99 (simple mode, no peering and no cloud service)".to_string(),
        Some(100) => "100 (bypass; Microsoft advises against this value)".to_string(),
        Some(other) => format!("{other} (not a documented download mode)"),
    }
}

/// Builds the registry of supported changes.
///
/// Order is meaningful: it is the order `apply` executes in. Lower-risk,
/// no-restart changes come first so that a failure part-way through leaves the
/// machine in the least surprising state.
pub fn all_changes() -> Vec<Box<dyn SystemChange>> {
    vec![
        // -------------------------------------------------------------------
        // Power plan. No elevation needed, instant effect, trivially undone.
        // -------------------------------------------------------------------
        Box::new(PowerPlanChange::new(
            ChangeMetadata {
                id: POWER_PLAN_HIGH_PERFORMANCE,
                name: "Active power plan",
                description: "Switch the active Windows power plan to High performance.",
                category: Category::Power,
                risk: RiskLevel::Low,
                requires_admin: false,
                reboot: RebootRequirement::NoReboot,
                mechanism: "PowerSetActiveScheme (powrprof), with GUID_MIN_POWER_SAVINGS",
                rationale: "The High performance plan keeps processor cores at higher minimum states \
                     instead of parking and unparking them, which removes the latency of ramping \
                     up when work arrives. It changes scheduling and power behaviour only; no \
                     feature is turned off.",
                tradeoffs: "Higher idle power draw, more heat and more fan noise. On a laptop this \
                     measurably shortens battery life. MinWin does not measure frame rates, so it \
                     makes no claim about game performance.",
                minimum_build: WINDOWS_11_BUILD,
            },
            // Resolved from the documented constant, not a copied literal.
            #[cfg(windows)]
            crate::sys::windows::well_known_power_plans::high_performance,
            #[cfg(not(windows))]
            || {
                crate::sys::model::PowerSchemeId::parse("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c")
                    .expect("the documented High performance GUID is a valid GUID")
            },
        )),
        // -------------------------------------------------------------------
        // Delivery Optimization peer-to-peer.
        //
        // Documented at:
        //   learn.microsoft.com/windows/deployment/do/waas-delivery-optimization-reference
        //   learn.microsoft.com/windows/client-management/mdm/policy-csp-deliveryoptimization
        //
        // Mode 0 is "HTTP only, no peering". Windows Update continues to work:
        // content is still downloaded over HTTP from Microsoft or from a
        // Connected Cache server. Only the peer-to-peer exchange stops.
        // -------------------------------------------------------------------
        Box::new(PolicyDwordChange::new(
            ChangeMetadata {
                id: DELIVERY_OPTIMIZATION_DOWNLOAD_MODE,
                name: "Delivery Optimization peer-to-peer",
                description: "Set the Delivery Optimization download mode to HTTP only, so this machine \
                     stops uploading update content to other devices.",
                category: Category::UpdateDelivery,
                risk: RiskLevel::Low,
                requires_admin: true,
                reboot: RebootRequirement::NoReboot,
                mechanism: r"Group Policy value HKLM\SOFTWARE\Policies\Microsoft\Windows\DeliveryOptimization\DODownloadMode",
                rationale: "By default Windows seeds update content to other devices on the local \
                     network, which costs background network and disk activity at times the user \
                     does not choose. Mode 0 keeps downloads working over HTTP and stops the \
                     peering.",
                tradeoffs: "Updates are fetched from Microsoft rather than from a nearby peer, so on a \
                     network with several Windows machines total internet bandwidth use may rise. \
                     Windows Update itself is untouched and continues to install updates normally.",
                minimum_build: WINDOWS_11_BUILD,
            },
            RegistryRoot::LocalMachine,
            r"SOFTWARE\Policies\Microsoft\Windows\DeliveryOptimization",
            "DODownloadMode",
            0,
            describe_download_mode,
        )),
        // -------------------------------------------------------------------
        // Connected User Experiences and Telemetry.
        //
        // Set to Manual, never Disabled, so Windows and any troubleshooter can
        // still start it on demand.
        //
        // Declined on managed devices: Microsoft's Defender for Endpoint
        // troubleshooting guidance expects this service to be set to automatic
        // start on onboarded machines. The EDR sensor has had no hard
        // dependency on it since Windows 10 1809, but MinWin is not willing to
        // take that chance on a device somebody else is responsible for.
        // -------------------------------------------------------------------
        Box::new(ServiceStartTypeChange::new(
            ChangeMetadata {
                id: DIAGTRACK_START_TYPE,
                name: "Connected User Experiences and Telemetry service",
                description: "Change the DiagTrack service from Automatic to Manual start, so Windows \
                     diagnostic data collection does not start on its own at boot.",
                category: Category::Telemetry,
                risk: RiskLevel::Low,
                requires_admin: true,
                reboot: RebootRequirement::RebootRecommended,
                mechanism: "ChangeServiceConfigW with dwStartType = SERVICE_DEMAND_START",
                rationale: "DiagTrack collects and uploads Windows diagnostic data. It runs continuously \
                     from boot by default and is not required for Windows to function, install \
                     updates, or for Microsoft Defender Antivirus to protect the machine.",
                tradeoffs: "Diagnostic data stops being sent, so Feedback Hub submissions and some \
                     Windows troubleshooters lose information Microsoft would otherwise use. \
                     Enterprise tooling that consumes Windows diagnostic data, such as Desktop \
                     Analytics, will no longer see this device; MinWin declines the change \
                     entirely on devices that look centrally managed.",
                minimum_build: WINDOWS_11_BUILD,
            },
            "DiagTrack",
            ServiceStartType::Manual,
            ManagementRule::DeclineIfManaged {
                because: "MinWin does not change diagnostic data collection on centrally managed \
                     devices, because management and security tooling may depend on it",
            },
        )),
        // -------------------------------------------------------------------
        // SysMain. Medium risk, and off by default in every shipped profile.
        // -------------------------------------------------------------------
        Box::new(ServiceStartTypeChange::new(
            ChangeMetadata {
                id: SYSMAIN_START_TYPE,
                name: "SysMain (prefetch) service",
                description: "Change the SysMain service from Automatic to Manual start, so Windows does \
                     not run its prefetching and superfetch analysis in the background.",
                category: Category::MemoryManagement,
                risk: RiskLevel::Medium,
                requires_admin: true,
                reboot: RebootRequirement::RebootRequired,
                mechanism: "ChangeServiceConfigW with dwStartType = SERVICE_DEMAND_START",
                rationale: "SysMain watches application usage and preloads data to speed up launches. \
                     The analysis itself costs continuous background disk and CPU activity, and \
                     it holds memory for cached data.",
                tradeoffs: "This is the riskiest change MinWin offers and the reason it ships disabled \
                     in both profiles. Microsoft does not recommend turning SysMain off. \
                     Application launches can become slower, and the effect depends heavily on \
                     the storage device: on a system with a slow drive or limited RAM it is \
                     likely to make things worse, not better. Enable it only if you intend to \
                     benchmark before and after and judge the result yourself.",
                minimum_build: WINDOWS_11_BUILD,
            },
            "SysMain",
            ServiceStartType::Manual,
            ManagementRule::Unrestricted,
        )),
    ]
}

/// A lookup over the registered changes.
pub struct ChangeRegistry {
    changes: Vec<Box<dyn SystemChange>>,
}

impl ChangeRegistry {
    pub fn load() -> Self {
        Self {
            changes: all_changes(),
        }
    }

    pub fn get(&self, id: &str) -> Option<&dyn SystemChange> {
        self.changes
            .iter()
            .find(|change| change.id() == id)
            .map(AsRef::as_ref)
    }

    /// Looks up a change, or fails naming the source that referenced it. This
    /// is the only way a profile's change id becomes executable, which is what
    /// confines profiles to the registered set.
    pub fn require(&self, id: &str, source_name: &str) -> Result<&dyn SystemChange> {
        self.get(id).ok_or_else(|| MinWinError::UnknownChange {
            id: id.to_string(),
            source_name: source_name.to_string(),
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn SystemChange> {
        self.changes.iter().map(AsRef::as_ref)
    }

    pub fn ids(&self) -> Vec<&'static str> {
        self.changes.iter().map(|change| change.id()).collect()
    }

    pub fn len(&self) -> usize {
        self.changes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

impl Default for ChangeRegistry {
    fn default() -> Self {
        Self::load()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::fake::FakeMachine;

    #[test]
    fn the_registry_contains_exactly_the_four_documented_changes() {
        let registry = ChangeRegistry::load();
        let mut ids = registry.ids();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![
                SYSMAIN_START_TYPE,
                POWER_PLAN_HIGH_PERFORMANCE,
                DIAGTRACK_START_TYPE,
                DELIVERY_OPTIMIZATION_DOWNLOAD_MODE,
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
        );
        assert_eq!(registry.len(), 4);
    }

    #[test]
    fn change_ids_are_unique() {
        let registry = ChangeRegistry::load();
        let unique: std::collections::BTreeSet<_> = registry.ids().into_iter().collect();
        assert_eq!(unique.len(), registry.len());
    }

    #[test]
    fn every_change_documents_a_mechanism_rationale_and_tradeoff() {
        // A change with an empty tradeoff field would be claiming it has no
        // downside, which MinWin does not permit.
        for change in ChangeRegistry::load().iter() {
            let metadata = change.metadata();
            assert!(!metadata.name.is_empty(), "{} has no name", metadata.id);
            assert!(
                !metadata.description.is_empty(),
                "{} has no description",
                metadata.id
            );
            assert!(
                metadata.mechanism.len() > 10,
                "{} does not name its Windows mechanism",
                metadata.id
            );
            assert!(
                metadata.rationale.len() > 40,
                "{} does not explain why it helps",
                metadata.id
            );
            assert!(
                metadata.tradeoffs.len() > 40,
                "{} does not state its tradeoff",
                metadata.id
            );
            assert!(
                metadata.minimum_build >= WINDOWS_11_BUILD,
                "{} would apply to pre-Windows-11 builds",
                metadata.id
            );
        }
    }

    #[test]
    fn no_change_touches_a_security_component() {
        // A crude but real guard: the words below must not appear in any
        // registered change's mechanism or description. If a future change
        // trips this, that is the review conversation happening on purpose.
        const FORBIDDEN: [&str; 12] = [
            "defender",
            "firewall",
            "windows update service",
            "wuauserv",
            "uac",
            "smartscreen",
            "memory integrity",
            "hvci",
            "credential guard",
            "lsass",
            "bitlocker",
            "tamper",
        ];
        for change in ChangeRegistry::load().iter() {
            let metadata = change.metadata();
            let haystack = format!(
                "{} {} {}",
                metadata.mechanism, metadata.description, metadata.id
            )
            .to_lowercase();
            for word in FORBIDDEN {
                assert!(
                    !haystack.contains(word),
                    "change {} mentions the security-relevant term {:?}",
                    metadata.id,
                    word
                );
            }
        }
    }

    #[test]
    fn only_manual_start_types_are_targeted_never_disabled() {
        // Verified behaviourally: after planning, no change targets Disabled.
        let machine = FakeMachine::windows_11().elevated();
        let facts = SystemFacts::gather(&machine).expect("facts");
        for change in ChangeRegistry::load().iter() {
            if !change
                .check_applicability(&machine, &facts)
                .expect("applicability")
                .is_applicable()
            {
                continue;
            }
            let plan = change.plan(&machine, &facts).expect("plan");
            let rendered = serde_json::to_string(&plan.target).expect("encode");
            assert!(
                !rendered.contains("disabled"),
                "change {} targets a Disabled state",
                change.id()
            );
        }
    }

    #[test]
    fn unknown_ids_are_rejected_with_the_referencing_source_named() {
        let registry = ChangeRegistry::load();
        assert!(registry.get("does.not.exist").is_none());
        // `dyn SystemChange` is not Debug, so unwrap the error by hand.
        let error = match registry.require("does.not.exist", "profiles/custom.toml") {
            Ok(_) => panic!("an unregistered change id must not resolve"),
            Err(error) => error,
        };
        let rendered = error.to_string();
        assert!(rendered.contains("does.not.exist"));
        assert!(rendered.contains("profiles/custom.toml"));
    }

    #[test]
    fn the_power_plan_change_is_the_only_one_not_needing_admin() {
        let registry = ChangeRegistry::load();
        let no_admin: Vec<&str> = registry
            .iter()
            .filter(|change| !change.metadata().requires_admin)
            .map(|change| change.id())
            .collect();
        assert_eq!(no_admin, vec![POWER_PLAN_HIGH_PERFORMANCE]);
    }

    #[test]
    fn sysmain_is_the_only_medium_risk_change() {
        let registry = ChangeRegistry::load();
        let medium: Vec<&str> = registry
            .iter()
            .filter(|change| change.metadata().risk == RiskLevel::Medium)
            .map(|change| change.id())
            .collect();
        assert_eq!(medium, vec![SYSMAIN_START_TYPE]);
    }

    #[test]
    fn download_modes_are_described_using_microsofts_own_vocabulary() {
        assert!(describe_download_mode(None).contains("not configured"));
        assert!(describe_download_mode(Some(0)).contains("HTTP only"));
        assert!(describe_download_mode(Some(1)).contains("local network"));
        assert!(describe_download_mode(Some(100)).contains("advises against"));
        assert!(describe_download_mode(Some(42)).contains("not a documented"));
    }
}
