//! Parsing and validating profiles.

use std::path::Path;

use crate::changes::ChangeRegistry;
use crate::core::error::{MinWinError, Result};
use crate::profiles::model::{CURRENT_SCHEMA_VERSION, Profile, ProfileFile};

/// The profiles MinWin ships. Embedded from the same files that live in the
/// repository, so there is one source of truth.
const MINIMAL_TOML: &str = include_str!("../../profiles/minimal.toml");
const GAMING_TOML: &str = include_str!("../../profiles/gaming.toml");

pub const BUILT_IN_PROFILE_IDS: [&str; 2] = ["minimal", "gaming"];

/// Loads one of the built-in profiles by id.
pub fn load_builtin(id: &str, registry: &ChangeRegistry) -> Result<Profile> {
    let (text, source_name) = match id {
        "minimal" => (MINIMAL_TOML, "built-in profile 'minimal'"),
        "gaming" => (GAMING_TOML, "built-in profile 'gaming'"),
        other => return Err(MinWinError::UnknownProfile(other.to_string())),
    };
    let profile = parse(text, source_name, registry)?;

    // A built-in profile whose declared id does not match the name it is
    // loaded by is a packaging mistake, and would make `status` report a
    // profile the user never asked for.
    if profile.id != id {
        return Err(MinWinError::invalid_profile(
            source_name,
            format!("it declares id {:?} but is loaded as {:?}", profile.id, id),
        ));
    }
    Ok(profile)
}

/// Loads a profile from an explicit path, for authoring and testing.
///
/// The path comes from the user on the command line, so it is used as given
/// and never joined to an id — there is no place for a traversal sequence to
/// act, because MinWin never constructs a path from profile content.
pub fn load_from_file(path: &Path, registry: &ChangeRegistry) -> Result<Profile> {
    let source_name = path.display().to_string();
    let text = std::fs::read_to_string(path)
        .map_err(|e| MinWinError::io(format!("read profile {source_name}"), e))?;

    // A profile is a short document. Refusing an enormous one keeps a hostile
    // file from being an easy way to exhaust memory.
    const MAX_BYTES: usize = 64 * 1024;
    if text.len() > MAX_BYTES {
        return Err(MinWinError::invalid_profile(
            &source_name,
            format!("the file is larger than the {MAX_BYTES} byte limit for a profile"),
        ));
    }

    parse(&text, &source_name, registry)
}

/// Parses and validates profile text.
///
/// Validation is total: every rule that could otherwise surprise a user at
/// apply time is enforced here, so a profile that loads is a profile that can
/// be planned.
pub fn parse(text: &str, source_name: &str, registry: &ChangeRegistry) -> Result<Profile> {
    let file: ProfileFile = toml::from_str(text)
        .map_err(|error| MinWinError::invalid_profile(source_name, error.message().to_string()))?;

    if file.schema_version != CURRENT_SCHEMA_VERSION {
        return Err(MinWinError::invalid_profile(
            source_name,
            format!(
                "it declares schema_version {} but this build of MinWin understands version {}",
                file.schema_version, CURRENT_SCHEMA_VERSION
            ),
        ));
    }

    validate_id(&file.profile.id, source_name)?;

    if file.profile.name.trim().is_empty() {
        return Err(MinWinError::invalid_profile(
            source_name,
            "the profile name is empty",
        ));
    }
    if file.profile.description.trim().is_empty() {
        return Err(MinWinError::invalid_profile(
            source_name,
            "the profile description is empty",
        ));
    }
    if file.changes.is_empty() {
        return Err(MinWinError::invalid_profile(
            source_name,
            "it selects no changes, so applying it would do nothing",
        ));
    }

    let mut seen = std::collections::BTreeSet::new();
    for selection in &file.changes {
        // The whole point: a profile may only name a change MinWin implements.
        registry.require(&selection.id, source_name)?;

        if !seen.insert(selection.id.as_str()) {
            return Err(MinWinError::invalid_profile(
                source_name,
                format!(
                    "it lists change {:?} more than once, so its intended state is ambiguous",
                    selection.id
                ),
            ));
        }
    }

    Ok(Profile {
        id: file.profile.id,
        name: file.profile.name,
        description: file.profile.description,
        notes: file.profile.notes,
        changes: file.changes,
        source_name: source_name.to_string(),
    })
}

/// Profile ids appear in output and are stored in the database, so they are
/// restricted to a conservative character set. They are never used to build a
/// file path, but keeping them boring removes a whole class of question.
fn validate_id(id: &str, source_name: &str) -> Result<()> {
    if id.is_empty() || id.len() > 32 {
        return Err(MinWinError::invalid_profile(
            source_name,
            format!("the profile id {id:?} must be between 1 and 32 characters"),
        ));
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(MinWinError::invalid_profile(
            source_name,
            format!(
                "the profile id {id:?} may only contain lowercase letters, digits, '-' and '_'"
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{
        DELIVERY_OPTIMIZATION_DOWNLOAD_MODE, DIAGTRACK_START_TYPE, POWER_PLAN_HIGH_PERFORMANCE,
        SYSMAIN_START_TYPE,
    };

    fn registry() -> ChangeRegistry {
        ChangeRegistry::load()
    }

    fn valid_toml(body: &str) -> String {
        format!(
            r#"schema_version = 1

[profile]
id = "test"
name = "Test"
description = "A test profile."

{body}
"#
        )
    }

    // -- the shipped profiles ------------------------------------------------

    #[test]
    fn the_shipped_minimal_profile_loads_and_is_conservative() {
        let profile = load_builtin("minimal", &registry()).expect("minimal should load");
        assert_eq!(profile.id, "minimal");

        let enabled = profile.enabled_change_ids();
        assert!(enabled.contains(&DIAGTRACK_START_TYPE));
        assert!(enabled.contains(&DELIVERY_OPTIMIZATION_DOWNLOAD_MODE));

        // The medium-risk change must be present but switched off.
        assert!(
            profile.offered_change_ids().contains(&SYSMAIN_START_TYPE),
            "SysMain must ship disabled in the minimal profile"
        );
        assert!(!enabled.contains(&SYSMAIN_START_TYPE));
    }

    #[test]
    fn the_shipped_gaming_profile_loads_and_adds_the_power_plan() {
        let profile = load_builtin("gaming", &registry()).expect("gaming should load");
        assert_eq!(profile.id, "gaming");

        let enabled = profile.enabled_change_ids();
        assert!(
            enabled.contains(&POWER_PLAN_HIGH_PERFORMANCE),
            "the power plan is what distinguishes gaming from minimal"
        );
        assert!(enabled.contains(&DIAGTRACK_START_TYPE));
        assert!(enabled.contains(&DELIVERY_OPTIMIZATION_DOWNLOAD_MODE));
    }

    #[test]
    fn the_gaming_profile_does_not_enable_sysmain_because_prefetch_helps_game_loading() {
        let profile = load_builtin("gaming", &registry()).expect("gaming");
        assert!(!profile.enabled_change_ids().contains(&SYSMAIN_START_TYPE));
    }

    #[test]
    fn both_shipped_profiles_document_how_they_differ() {
        for id in BUILT_IN_PROFILE_IDS {
            let profile = load_builtin(id, &registry()).expect(id);
            let notes = profile.notes.unwrap_or_default();
            assert!(
                notes.len() > 40,
                "profile {id} should explain itself in its notes"
            );
        }
    }

    #[test]
    fn an_unknown_profile_name_is_rejected() {
        let error = load_builtin("turbo", &registry()).expect_err("should fail");
        assert!(matches!(error, MinWinError::UnknownProfile(_)));
        assert!(error.to_string().contains("minimal, gaming"));
    }

    // -- validation ----------------------------------------------------------

    #[test]
    fn a_profile_naming_an_unregistered_change_is_rejected_by_name() {
        let text = valid_toml(
            r#"[[changes]]
id = "registry.disable_defender"
enabled = true"#,
        );
        let error = parse(&text, "test.toml", &registry()).expect_err("should fail");
        let rendered = error.to_string();
        assert!(rendered.contains("registry.disable_defender"));
        assert!(rendered.contains("test.toml"));
    }

    #[test]
    fn a_profile_cannot_express_a_command_or_a_registry_path() {
        // `deny_unknown_fields` is what makes this true, so it is tested.
        for body in [
            r#"[[changes]]
id = "telemetry.diagtrack_start_type"
command = "cmd.exe /c del /f /s C:\\""#,
            r#"[[changes]]
id = "telemetry.diagtrack_start_type"
registry_key = "HKLM\\SOFTWARE\\Microsoft\\Windows Defender""#,
            r#"[[changes]]
id = "telemetry.diagtrack_start_type"
service = "WinDefend""#,
        ] {
            let text = valid_toml(body);
            assert!(
                parse(&text, "hostile.toml", &registry()).is_err(),
                "a profile carrying an action field must be rejected: {body}"
            );
        }
    }

    #[test]
    fn an_unknown_key_in_the_profile_header_is_rejected() {
        let text = r#"schema_version = 1

[profile]
id = "test"
name = "Test"
description = "d"
run_before = "payload.exe"

[[changes]]
id = "telemetry.diagtrack_start_type"
"#;
        assert!(parse(text, "test.toml", &registry()).is_err());
    }

    #[test]
    fn a_wrong_schema_version_is_rejected_rather_than_guessed_at() {
        let text = valid_toml(
            r#"[[changes]]
id = "telemetry.diagtrack_start_type""#,
        )
        .replace("schema_version = 1", "schema_version = 2");
        let error = parse(&text, "test.toml", &registry()).expect_err("should fail");
        assert!(error.to_string().contains("schema_version 2"));
    }

    #[test]
    fn a_duplicate_change_id_is_rejected_as_ambiguous() {
        let text = valid_toml(
            r#"[[changes]]
id = "telemetry.diagtrack_start_type"
enabled = true

[[changes]]
id = "telemetry.diagtrack_start_type"
enabled = false"#,
        );
        let error = parse(&text, "test.toml", &registry()).expect_err("should fail");
        assert!(error.to_string().contains("more than once"));
    }

    #[test]
    fn a_profile_with_no_changes_is_rejected() {
        let error = parse(&valid_toml(""), "test.toml", &registry()).expect_err("should fail");
        assert!(error.to_string().contains("selects no changes"));
    }

    #[test]
    fn empty_names_and_descriptions_are_rejected() {
        for (field, replacement) in [
            (r#"name = "Test""#, r#"name = "  ""#),
            (r#"description = "A test profile.""#, r#"description = """#),
        ] {
            let text = valid_toml(
                r#"[[changes]]
id = "telemetry.diagtrack_start_type""#,
            )
            .replace(field, replacement);
            assert!(parse(&text, "test.toml", &registry()).is_err());
        }
    }

    #[test]
    fn hostile_profile_ids_are_rejected() {
        for bad in [
            r#"../../windows/system32"#,
            r#"Minimal"#,
            r#"with space"#,
            r#""#,
            r#"a-very-long-profile-id-that-goes-well-past-the-limit"#,
        ] {
            let text = valid_toml(
                r#"[[changes]]
id = "telemetry.diagtrack_start_type""#,
            )
            .replace(r#"id = "test""#, &format!(r#"id = "{bad}""#));
            assert!(
                parse(&text, "test.toml", &registry()).is_err(),
                "profile id {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn malformed_toml_reports_a_parse_message_not_a_panic() {
        let error =
            parse("this is not = = toml", "broken.toml", &registry()).expect_err("should fail");
        assert!(error.to_string().contains("broken.toml"));
    }

    #[test]
    fn a_profile_can_be_loaded_from_an_explicit_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("custom.toml");
        std::fs::write(
            &path,
            valid_toml(
                r#"[[changes]]
id = "power.active_plan_high_performance""#,
            ),
        )
        .expect("write");

        let profile = load_from_file(&path, &registry()).expect("load");
        assert_eq!(profile.id, "test");
        assert_eq!(
            profile.enabled_change_ids(),
            vec![POWER_PLAN_HIGH_PERFORMANCE]
        );
        assert!(profile.source_name.contains("custom.toml"));
    }

    #[test]
    fn an_oversized_profile_file_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("huge.toml");
        let padding = "# ".repeat(40 * 1024);
        std::fs::write(
            &path,
            format!(
                "{padding}\n{}",
                valid_toml(
                    r#"[[changes]]
id = "telemetry.diagtrack_start_type""#
                )
            ),
        )
        .expect("write");

        let error = load_from_file(&path, &registry()).expect_err("should fail");
        assert!(error.to_string().contains("byte limit"));
    }

    #[test]
    fn a_missing_profile_file_reports_the_path() {
        let error =
            load_from_file(Path::new("does-not-exist.toml"), &registry()).expect_err("should fail");
        assert!(error.to_string().contains("does-not-exist.toml"));
    }
}
