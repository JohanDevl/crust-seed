//! Health "problems" surfaced on the web UI's health page.
//!
//! Ported from `problems.ts`. Each provider is fallible and independent: the
//! original ran them through `Promise.allSettled` and converted a rejection
//! into a problem of its own, so one broken provider never hides the others.
//! [`collect_problems`] does the same with `join_all` over boxed futures.

use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProblemSeverity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Problem {
    pub id: String,
    pub severity: ProblemSeverity,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

impl Problem {
    pub fn new(
        id: impl Into<String>,
        severity: ProblemSeverity,
        summary: impl Into<String>,
        details: impl Into<String>,
    ) -> Self {
        Problem {
            id: id.into(),
            severity,
            summary: summary.into(),
            details: Some(details.into()),
            metadata: None,
        }
    }

    pub fn with_metadata(mut self, metadata: Map<String, Value>) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// A named provider plus its future, so a failure can be attributed.
pub type ProblemFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Problem>, String>> + Send + 'a>>;

/// Runs every provider and folds failures into synthetic problems.
pub async fn collect_from(providers: Vec<(&'static str, ProblemFuture<'_>)>) -> Vec<Problem> {
    let (names, futures): (Vec<_>, Vec<_>) = providers.into_iter().unzip();
    let results = join_all(futures).await;

    let mut problems = Vec::new();
    for (name, result) in names.into_iter().zip(results) {
        match result {
            Ok(found) => problems.extend(found),
            Err(message) => problems.push(Problem::new(
                format!("problem-provider-error:{name}"),
                ProblemSeverity::Error,
                "Problem provider failed to collect problems.",
                message,
            )),
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_failing_provider_becomes_a_problem_instead_of_hiding_the_others() {
        let problems = collect_from(vec![
            (
                "ok",
                Box::pin(async {
                    Ok(vec![Problem::new(
                        "x",
                        ProblemSeverity::Info,
                        "fine",
                        "all good",
                    )])
                }) as ProblemFuture<'_>,
            ),
            (
                "broken",
                Box::pin(async { Err("kaboom".to_string()) }) as ProblemFuture<'_>,
            ),
        ])
        .await;

        assert_eq!(problems.len(), 2);
        assert_eq!(problems[0].id, "x");
        assert_eq!(problems[1].id, "problem-provider-error:broken");
        assert_eq!(problems[1].severity, ProblemSeverity::Error);
        assert_eq!(problems[1].details.as_deref(), Some("kaboom"));
    }
}
