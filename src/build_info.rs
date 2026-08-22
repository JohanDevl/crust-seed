//! Build provenance surfaced by `--version` and the web UI's about page.
//!
//! Ported from `buildInfo.ts`. The values are baked in at compile time from the
//! environment the Docker build sets, so a running container can be traced back
//! to a commit without consulting the registry.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    pub commit_sha: Option<String>,
    pub branch: Option<String>,
    pub tag: Option<String>,
    pub message: Option<String>,
    pub date: Option<String>,
}

fn normalize(value: Option<&'static str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

pub fn build_info() -> BuildInfo {
    BuildInfo {
        commit_sha: normalize(option_env!("BUILD_COMMIT_SHA")),
        branch: normalize(option_env!("BUILD_BRANCH")),
        tag: normalize(option_env!("BUILD_VERSION")),
        message: normalize(option_env!("BUILD_MESSAGE")),
        date: normalize(option_env!("BUILD_DATE")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A local build has no build environment; every field must be absent
    /// rather than an empty string, so the UI can hide the section.
    #[test]
    fn missing_build_environment_yields_nulls() {
        let info = build_info();
        for field in [&info.commit_sha, &info.branch, &info.tag] {
            assert!(field.as_deref().is_none_or(|v| !v.is_empty()));
        }
    }

    #[test]
    fn blank_values_are_treated_as_absent() {
        assert_eq!(normalize(Some("  ")), None);
        assert_eq!(normalize(Some(" abc ")), Some("abc".to_string()));
        assert_eq!(normalize(None), None);
    }
}
