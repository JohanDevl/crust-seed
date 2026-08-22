//! Linking diagnostics.
//!
//! Ported from `problems/linking.ts`. Hardlinks and reflinks cannot cross
//! filesystems, so a `dataDir` with no `linkDir` on the same device can never
//! be linked from — a failure mode that otherwise only shows up as silent
//! non-matching.

use std::path::Path;

use super::paths::{PathIssue, PathProblemDescriptor, build_path_problem, diagnose_dir};
use crate::action::device_of;
use crate::config::RuntimeConfig;
use crate::problems::{Problem, ProblemSeverity};

pub async fn collect_data_linking_problems(config: &RuntimeConfig) -> Vec<Problem> {
    let mut descriptors: Vec<PathProblemDescriptor> = Vec::new();
    if config.link_dirs.is_empty() || config.data_dirs.is_empty() {
        return Vec::new();
    }

    // Verify each linkDir, keeping the ones that are usable along with the
    // device they live on.
    let mut valid_link_dirs: Vec<(String, u64)> = Vec::new();
    for (index, dir) in config.link_dirs.iter().enumerate() {
        match diagnose_dir(dir, &format!("linkDir{index}"), "linkDirs", true, true).await {
            Some(descriptor) => descriptors.push(descriptor),
            None => {
                if let Some(device) = device_of(Path::new(dir)).await {
                    valid_link_dirs.push((dir.clone(), device));
                }
            }
        }
    }
    if valid_link_dirs.is_empty() {
        return descriptors.iter().map(build_path_problem).collect();
    }

    for (index, data_dir) in config.data_dirs.iter().enumerate() {
        if let Some(descriptor) = diagnose_dir(
            data_dir,
            &format!("dataDir{index}"),
            "dataDirs",
            true,
            false,
        )
        .await
        {
            descriptors.push(descriptor);
            continue;
        }

        let data_device = device_of(Path::new(data_dir)).await;
        let matching = data_device
            .and_then(|device| valid_link_dirs.iter().find(|(_, d)| *d == device))
            .cloned();

        let Some((link_dir, _)) = matching else {
            descriptors.push(PathProblemDescriptor {
                category: "linkDirs",
                name: "linkDir".into(),
                path: valid_link_dirs
                    .iter()
                    .map(|(path, _)| path.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
                issue: PathIssue::CrossPlatformLinking,
                message: "No linkDir shares a filesystem with this dataDir, so linking will fail. Add a linkDir on the same device.".into(),
                severity: ProblemSeverity::Error,
            });
            continue;
        };

        match crate::action::test_linking(
            Path::new(data_dir),
            "healthCheckSrc.cross-seed",
            "healthCheckDest.cross-seed",
            config,
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => descriptors.push(PathProblemDescriptor {
                category: "linkDirs",
                name: "linkDir".into(),
                path: link_dir,
                issue: PathIssue::LinkingFailed,
                message: "Linking test failed. Check filesystem permissions and mounts.".into(),
                severity: ProblemSeverity::Warning,
            }),
            Err(_) => descriptors.push(PathProblemDescriptor {
                category: "linkDirs",
                name: "linkDir".into(),
                path: data_dir.clone(),
                issue: PathIssue::LinkingFailed,
                message: "Linking test threw an error. See logs for details.".into(),
                severity: ProblemSeverity::Error,
            }),
        }
    }

    descriptors.iter().map(build_path_problem).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_runtime_config;

    #[tokio::test]
    async fn nothing_is_reported_without_both_link_and_data_dirs() {
        let mut config = default_runtime_config();
        config.link_dirs = vec!["/links".into()];
        config.data_dirs = vec![];
        assert!(collect_data_linking_problems(&config).await.is_empty());

        config.link_dirs = vec![];
        config.data_dirs = vec!["/data".into()];
        assert!(collect_data_linking_problems(&config).await.is_empty());
    }

    #[tokio::test]
    async fn a_missing_link_dir_is_reported() {
        let mut config = default_runtime_config();
        config.link_dirs = vec!["/definitely/not/here".into()];
        config.data_dirs = vec!["/also/not/here".into()];

        let problems = collect_data_linking_problems(&config).await;
        assert!(problems.iter().any(|p| p.id.contains("linkDirs:missing")));
    }

    /// The happy path: a dataDir and a linkDir on the same filesystem.
    #[tokio::test]
    async fn same_device_dirs_pass() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let links = dir.path().join("links");
        tokio::fs::create_dir_all(&data).await.unwrap();
        tokio::fs::create_dir_all(&links).await.unwrap();
        tokio::fs::write(data.join("a.mkv"), b"x").await.unwrap();

        let mut config = default_runtime_config();
        config.link_dirs = vec![links.to_string_lossy().into_owned()];
        config.data_dirs = vec![data.to_string_lossy().into_owned()];
        config.link_type = crate::constants::LinkType::Hardlink;

        assert!(collect_data_linking_problems(&config).await.is_empty());
    }
}
