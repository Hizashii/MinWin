//! The profile schema.

use serde::{Deserialize, Serialize};

/// The on-disk schema version. Bumped only when the TOML shape changes in a
/// way an older binary could misread.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// A profile exactly as written in TOML.
///
/// `deny_unknown_fields` is deliberate: a profile with a misspelled key, or one
/// written for a newer MinWin, must fail rather than be silently applied with
/// the unrecognised part ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileFile {
    pub schema_version: u32,
    pub profile: ProfileHeader,
    #[serde(default, rename = "changes")]
    pub changes: Vec<ProfileChangeSelection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileHeader {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Free text shown by `apply`, used by the shipped profiles to state how
    /// they differ from each other.
    #[serde(default)]
    pub notes: Option<String>,
}

/// One change selection. Two fields, by design — see the module docs on why
/// there is nothing else here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileChangeSelection {
    pub id: String,
    /// `false` means the change is *offered* by this profile but not applied.
    /// This is how MinWin ships a medium-risk change without turning it on.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Optional note explaining a selection, shown next to the change.
    #[serde(default)]
    pub note: Option<String>,
}

fn default_enabled() -> bool {
    true
}

/// A validated profile. Constructing one of these is only possible through
/// [`crate::profiles::loader`], which is what guarantees every id in it is
/// registered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub description: String,
    pub notes: Option<String>,
    pub changes: Vec<ProfileChangeSelection>,
    /// Where this profile came from, for error messages and for the record in
    /// the state database.
    pub source_name: String,
}

impl Profile {
    /// The change ids this profile will actually apply, in profile order.
    pub fn enabled_change_ids(&self) -> Vec<&str> {
        self.changes
            .iter()
            .filter(|selection| selection.enabled)
            .map(|selection| selection.id.as_str())
            .collect()
    }

    /// Changes the profile mentions but leaves switched off.
    pub fn offered_change_ids(&self) -> Vec<&str> {
        self.changes
            .iter()
            .filter(|selection| !selection.enabled)
            .map(|selection| selection.id.as_str())
            .collect()
    }

    pub fn selection(&self, id: &str) -> Option<&ProfileChangeSelection> {
        self.changes.iter().find(|selection| selection.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> Profile {
        Profile {
            id: "minimal".into(),
            name: "Minimal".into(),
            description: "d".into(),
            notes: None,
            changes: vec![
                ProfileChangeSelection {
                    id: "a".into(),
                    enabled: true,
                    note: None,
                },
                ProfileChangeSelection {
                    id: "b".into(),
                    enabled: false,
                    note: Some("opt in".into()),
                },
                ProfileChangeSelection {
                    id: "c".into(),
                    enabled: true,
                    note: None,
                },
            ],
            source_name: "test".into(),
        }
    }

    #[test]
    fn enabled_and_offered_changes_are_separated_and_keep_profile_order() {
        assert_eq!(profile().enabled_change_ids(), vec!["a", "c"]);
        assert_eq!(profile().offered_change_ids(), vec!["b"]);
    }

    #[test]
    fn selections_can_be_looked_up_with_their_notes() {
        let profile = profile();
        assert_eq!(
            profile.selection("b").and_then(|s| s.note.as_deref()),
            Some("opt in")
        );
        assert!(profile.selection("missing").is_none());
    }

    #[test]
    fn enabled_defaults_to_true_when_omitted() {
        let parsed: ProfileChangeSelection =
            toml::from_str(r#"id = "x""#).expect("minimal selection should parse");
        assert!(parsed.enabled);
    }
}
