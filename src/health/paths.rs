//! Directory permission diagnostics.
//!
//! Ported from `problems/path.ts`. Most "cross-seed isn't doing anything"
//! reports are a container missing a volume mount, so each configured directory
//! is probed for existence and the access it actually needs.

use std::path::Path;

use serde::Serialize;

use crate::config::RuntimeConfig;
use crate::problems::{Problem, ProblemSeverity};
use crate::utils::{DirVerificationFailure, R_OK, W_OK, verify_dir};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PathIssue {
    Missing,
    NotDirectory,
    Unreadable,
    Unwritable,
    CrossPlatformLinking,
    LinkingFailed,
}

impl PathIssue {
    pub fn as_str(self) -> &'static str {
        match self {
            PathIssue::Missing => "missing",
            PathIssue::NotDirectory => "not-directory",
            PathIssue::Unreadable => "unreadable",
            PathIssue::Unwritable => "unwritable",
            PathIssue::CrossPlatformLinking => "cross-platform-linking",
            PathIssue::LinkingFailed => "linking-failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PathProblemDescriptor {
    pub category: &'static str,
    pub name: String,
    pub path: String,
    pub issue: PathIssue,
    pub message: String,
    pub severity: ProblemSeverity,
}

pub fn build_path_problem(descriptor: &PathProblemDescriptor) -> Problem {
    Problem {
        id: format!(
            "path:{}:{}:{}",
            descriptor.category,
            descriptor.issue.as_str(),
            descriptor.path
        ),
        severity: descriptor.severity,
        summary: format!("{}: {}", descriptor.name, descriptor.message),
        details: Some(format!("Path: {}", descriptor.path)),
        metadata: serde_json::json!({
            "category": descriptor.category,
            "path": descriptor.path,
            "issue": descriptor.issue.as_str(),
        })
        .as_object()
        .cloned(),
    }
}

/// Verifies one directory, returning a descriptor when it is unusable.
pub async fn diagnose_dir(
    path: &str,
    name: &str,
    category: &'static str,
    read: bool,
    write: bool,
) -> Option<PathProblemDescriptor> {
    let mode = if read { R_OK } else { 0 } | if write { W_OK } else { 0 };
    match verify_dir(Path::new(path), name, mode).await {
        Ok(_) => None,
        Err(reason) => {
            let (issue, message) = match reason {
                DirVerificationFailure::Missing => {
                    (PathIssue::Missing, "Directory does not exist.")
                }
                DirVerificationFailure::NotDirectory => {
                    (PathIssue::NotDirectory, "Path is not a directory.")
                }
                DirVerificationFailure::Unreadable => (
                    PathIssue::Unreadable,
                    "crust-seed cannot read from this directory.",
                ),
                DirVerificationFailure::Unwritable => (
                    PathIssue::Unwritable,
                    "crust-seed cannot write to this directory.",
                ),
            };
            Some(PathProblemDescriptor {
                category,
                name: name.to_string(),
                path: path.to_string(),
                issue,
                message: message.to_string(),
                severity: ProblemSeverity::Error,
            })
        }
    }
}

pub async fn collect_path_problems(config: &RuntimeConfig) -> Vec<Problem> {
    let mut descriptors = Vec::new();

    if let Some(torrent_dir) = &config.torrent_dir
        && let Some(descriptor) =
            diagnose_dir(torrent_dir, "torrentDir", "torrentDir", true, false).await
    {
        descriptors.push(descriptor);
    }
    if !config.output_dir.is_empty()
        && let Some(descriptor) =
            diagnose_dir(&config.output_dir, "outputDir", "outputDir", true, true).await
    {
        descriptors.push(descriptor);
    }
    if let Some(inject_dir) = &config.inject_dir
        && !inject_dir.is_empty()
        && let Some(descriptor) =
            diagnose_dir(inject_dir, "injectDir", "injectDir", true, true).await
    {
        descriptors.push(descriptor);
    }

    descriptors.iter().map(build_path_problem).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_runtime_config;

    #[tokio::test]
    async fn a_missing_directory_is_an_error() {
        let descriptor = diagnose_dir("/definitely/not/here", "outputDir", "outputDir", true, true)
            .await
            .unwrap();
        assert_eq!(descriptor.issue, PathIssue::Missing);
        assert_eq!(descriptor.severity, ProblemSeverity::Error);
    }

    #[tokio::test]
    async fn a_file_where_a_directory_belongs_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        tokio::fs::write(&file, b"x").await.unwrap();

        let descriptor = diagnose_dir(
            &file.to_string_lossy(),
            "outputDir",
            "outputDir",
            true,
            false,
        )
        .await
        .unwrap();
        assert_eq!(descriptor.issue, PathIssue::NotDirectory);
    }

    #[tokio::test]
    async fn a_usable_directory_produces_no_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            diagnose_dir(
                &dir.path().to_string_lossy(),
                "outputDir",
                "outputDir",
                true,
                true
            )
            .await
            .is_none()
        );
    }

    #[test]
    fn problem_ids_encode_the_category_issue_and_path() {
        let problem = build_path_problem(&PathProblemDescriptor {
            category: "outputDir",
            name: "outputDir".into(),
            path: "/out".into(),
            issue: PathIssue::Missing,
            message: "Directory does not exist.".into(),
            severity: ProblemSeverity::Error,
        });
        assert_eq!(problem.id, "path:outputDir:missing:/out");
        assert_eq!(problem.summary, "outputDir: Directory does not exist.");
    }

    #[tokio::test]
    async fn unconfigured_directories_are_skipped() {
        let mut config = default_runtime_config();
        config.torrent_dir = None;
        config.inject_dir = None;
        config.output_dir = String::new();
        assert!(collect_path_problems(&config).await.is_empty());
    }
}
