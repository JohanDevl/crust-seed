//! Webhook notifications.
//!
//! Ported from `pushNotifier.ts`. Each configured webhook receives a POST with
//! a `{title, body, ...}` JSON body. An entry may also carry its own headers
//! and an extra payload object, and `{placeholder}` template variables inside
//! those are substituted so one webhook config can drive Discord, Apprise,
//! Notifiarr and friends without crust-seed knowing about any of them.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::config::{RuntimeConfig, WebhookEntry};
use crate::constants::{ActionResult, Decision, InjectionResult, PROGRAM_NAME};
use crate::decide::ResultAssessment;
use crate::http::client;
use crate::logger::Label;
use crate::searchee::Searchee;
use crate::utils::format_as_list;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WebhookResult {
    pub url: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Replaces `{name}` placeholders throughout a JSON value.
///
/// Recurses into arrays and objects so a nested Discord embed can use the same
/// variables as the top-level body. An unknown placeholder is left verbatim,
/// matching the original's `vars[varName] ?? match`.
pub fn substitute_template_value(value: &Value, vars: &BTreeMap<String, String>) -> Value {
    match value {
        Value::String(text) => Value::String(substitute_in_string(text, vars)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| substitute_template_value(item, vars))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), substitute_template_value(item, vars)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn substitute_in_string(text: &str, vars: &BTreeMap<String, String>) -> String {
    static PLACEHOLDER: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\{(\w+)\}").unwrap());
    PLACEHOLDER
        .replace_all(text, |caps: &regex::Captures<'_>| {
            vars.get(&caps[1])
                .cloned()
                .unwrap_or_else(|| caps[0].to_string())
        })
        .into_owned()
}

/// Merges user headers over the defaults **case-insensitively**, so an override
/// of `content-type` replaces `Content-Type` instead of both being sent.
pub fn merge_headers(
    defaults: &BTreeMap<String, String>,
    overrides: Option<&BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    let mut merged = defaults.clone();
    let Some(overrides) = overrides else {
        return merged;
    };
    for (key, value) in overrides {
        let clashing: Vec<String> = merged
            .keys()
            .filter(|existing| {
                existing.as_str() != key.as_str() && existing.to_lowercase() == key.to_lowercase()
            })
            .cloned()
            .collect();
        for existing in clashing {
            merged.remove(&existing);
        }
        merged.insert(key.clone(), value.clone());
    }
    merged
}

fn default_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("Content-Type".to_string(), "application/json".to_string()),
        ("User-Agent".to_string(), crate::constants::user_agent()),
    ])
}

#[derive(Debug, Clone, Default)]
pub struct PushNotification {
    pub title: Option<String>,
    pub body: String,
    pub template_vars: BTreeMap<String, String>,
    pub extra: Map<String, Value>,
}

pub struct PushNotifier {
    entries: Vec<WebhookEntry>,
}

impl PushNotifier {
    pub fn new(entries: Vec<WebhookEntry>) -> Self {
        PushNotifier { entries }
    }

    pub async fn notify(&self, notification: PushNotification) -> Vec<WebhookResult> {
        let title = notification
            .title
            .clone()
            .unwrap_or_else(|| PROGRAM_NAME.to_string());

        futures::future::join_all(self.entries.iter().map(|entry| {
            let title = title.clone();
            let notification = notification.clone();
            async move { self.send(entry, &title, &notification).await }
        }))
        .await
    }

    async fn send(
        &self,
        entry: &WebhookEntry,
        title: &str,
        notification: &PushNotification,
    ) -> WebhookResult {
        let url = entry.url().to_string();

        let mut payload = Map::new();
        payload.insert("title".into(), json!(title));
        payload.insert("body".into(), json!(notification.body));
        for (key, value) in &notification.extra {
            payload.insert(key.clone(), value.clone());
        }

        let mut headers = default_headers();
        if let WebhookEntry::Object(object) = entry {
            headers = merge_headers(&default_headers(), object.headers.as_ref());
            // The user's payload wins over crust-seed's own fields, so a
            // Discord webhook can replace `body` with `content`.
            if let Some(extra) = &object.payload {
                for (key, value) in extra {
                    payload.insert(key.clone(), value.clone());
                }
            }
            if !notification.template_vars.is_empty() {
                payload = match substitute_template_value(
                    &Value::Object(payload),
                    &notification.template_vars,
                ) {
                    Value::Object(map) => map,
                    _ => Map::new(),
                };
                headers = headers
                    .into_iter()
                    .map(|(key, value)| {
                        (
                            key,
                            substitute_in_string(&value, &notification.template_vars),
                        )
                    })
                    .collect();
            }
        }

        let mut request = client()
            .post(&url)
            .timeout(std::time::Duration::from_secs(300))
            .body(Value::Object(payload).to_string());
        for (key, value) in headers {
            request = request.header(key, value);
        }

        match request.send().await {
            Ok(response) if response.status().is_success() => WebhookResult {
                url,
                ok: true,
                error: None,
            },
            Ok(response) => {
                let status = response.status();
                let error = format!(
                    "{} {}",
                    status.as_u16(),
                    status.canonical_reason().unwrap_or("")
                );
                tracing::error!("{url} rejected push notification: {error}");
                WebhookResult {
                    url,
                    ok: false,
                    error: Some(error),
                }
            }
            Err(e) => {
                tracing::error!("{url} failed to send push notification: {e}");
                WebhookResult {
                    url,
                    ok: false,
                    error: Some(e.to_string()),
                }
            }
        }
    }
}

/// Builds the notification for a batch of results against one searchee.
///
/// Successes and failures are reported separately, because a run can produce
/// both and users route them to different places.
pub async fn send_results_notification(
    searchee: &Searchee,
    results: &[(ResultAssessment, String, ActionResult)],
    config: &RuntimeConfig,
) {
    let notifier = PushNotifier::new(config.notification_webhook_urls.clone());
    let source = searchee
        .label
        .map(|l| l.as_str().to_string())
        .unwrap_or_default();
    let searchee_source = searchee.source().as_str();

    let searchee_json = json!({
        "category": searchee.category,
        "tags": searchee.tags,
        "trackers": searchee.trackers,
        "length": searchee.length,
        "clientHost": searchee.client_host,
        "infoHash": searchee.info_hash,
        "path": searchee.path,
        "source": searchee_source,
    });

    let successes: Vec<&(ResultAssessment, String, ActionResult)> = results
        .iter()
        .filter(|(_, _, action)| {
            matches!(
                action,
                ActionResult::Saved | ActionResult::Injection(InjectionResult::Success)
            )
        })
        .collect();

    if !successes.is_empty() {
        let injected = successes
            .iter()
            .any(|(_, _, action)| *action == ActionResult::Injection(InjectionResult::Success));
        // A saved-only result is never seeding, so it is reported as paused.
        let paused = if injected {
            successes.iter().any(|(assessment, _, _)| {
                assessment.metafile.as_ref().is_some_and(|meta| {
                    crate::clients::estimate_paused_status(
                        meta,
                        searchee,
                        assessment.decision,
                        config,
                    )
                })
            })
        } else {
            true
        };
        let performed_action = if injected {
            format!("Injected{}", if paused { " (paused)" } else { "" })
        } else {
            "Saved".to_string()
        };
        notifier
            .notify(build_notification(
                &successes,
                &source,
                searchee_source,
                &performed_action,
                paused,
                if injected {
                    InjectionResult::Success.as_str()
                } else {
                    "SAVED"
                },
                &searchee_json,
            ))
            .await;
    }

    let failures: Vec<&(ResultAssessment, String, ActionResult)> = results
        .iter()
        .filter(|(_, _, action)| *action == ActionResult::Injection(InjectionResult::Failure))
        .collect();

    if !failures.is_empty() {
        notifier
            .notify(build_notification(
                &failures,
                &source,
                searchee_source,
                "Failed to inject",
                false,
                InjectionResult::Failure.as_str(),
                &searchee_json,
            ))
            .await;
    }
}

fn build_notification(
    results: &[&(ResultAssessment, String, ActionResult)],
    source: &str,
    searchee_source: &str,
    performed_action: &str,
    paused: bool,
    result_code: &str,
    searchee_json: &Value,
) -> PushNotification {
    let name = results
        .first()
        .and_then(|(assessment, _, _)| assessment.metafile.as_ref())
        .map(|meta| meta.name.clone())
        .unwrap_or_default();
    let num_trackers = results.len();
    let info_hashes: Vec<String> = results
        .iter()
        .filter_map(|(assessment, _, _)| assessment.metafile.as_ref().map(|m| m.info_hash.clone()))
        .collect();
    let trackers: Vec<String> = results
        .iter()
        .map(|(_, tracker, _)| tracker.clone())
        .collect();
    let decisions: Vec<String> = results
        .iter()
        .map(|(assessment, _, _)| assessment.decision.as_str().to_string())
        .collect();
    let trackers_list = format_as_list(&trackers, true, false);
    let decisions_list = format_as_list(&decisions, true, false);

    let body = format!(
        "{source}: {performed_action} {name} on {num_trackers} tracker{} by {decisions_list} from {searchee_source}: {trackers_list}",
        if num_trackers != 1 { "s" } else { "" }
    );

    let template_vars = BTreeMap::from([
        ("source".to_string(), source.to_string()),
        ("performedAction".to_string(), performed_action.to_string()),
        ("name".to_string(), name.clone()),
        ("numTrackers".to_string(), num_trackers.to_string()),
        ("trackersListStr".to_string(), trackers_list.clone()),
        ("searcheeSource".to_string(), searchee_source.to_string()),
        ("decisions".to_string(), decisions_list),
        ("trackers".to_string(), trackers.join(", ")),
        ("result".to_string(), result_code.to_string()),
        ("paused".to_string(), paused.to_string()),
        ("infoHashes".to_string(), info_hashes.join(", ")),
    ]);

    let mut extra = Map::new();
    extra.insert("event".into(), json!("RESULTS"));
    extra.insert("name".into(), json!(name));
    extra.insert("infoHashes".into(), json!(info_hashes));
    extra.insert("trackers".into(), json!(trackers));
    extra.insert("source".into(), json!(source));
    extra.insert("result".into(), json!(result_code));
    extra.insert("paused".into(), json!(paused));
    extra.insert(
        "decisions".into(),
        json!(
            results
                .iter()
                .map(|(a, _, _)| a.decision.as_str())
                .collect::<Vec<_>>()
        ),
    );
    extra.insert("searchee".into(), searchee_json.clone());

    PushNotification {
        title: None,
        body,
        template_vars,
        extra,
    }
}

/// The `test-notification` command and the settings page's test button.
pub async fn send_test_notification(entries: Vec<WebhookEntry>) -> Vec<WebhookResult> {
    let notifier = PushNotifier::new(entries);
    let results = notifier
        .notify(PushNotification {
            title: None,
            body: "Test notification from crust-seed".to_string(),
            template_vars: BTreeMap::from([
                ("source".to_string(), "TestClient".to_string()),
                ("performedAction".to_string(), "Injected".to_string()),
                (
                    "name".to_string(),
                    "Test.Torrent.2024.1080p.BluRay.x264".to_string(),
                ),
                ("numTrackers".to_string(), "1".to_string()),
                ("trackersListStr".to_string(), "ExampleTracker".to_string()),
                ("searcheeSource".to_string(), "torrentClient".to_string()),
                (
                    "decisions".to_string(),
                    Decision::Match.as_str().to_string(),
                ),
                ("trackers".to_string(), "ExampleTracker".to_string()),
                (
                    "result".to_string(),
                    InjectionResult::Success.as_str().to_string(),
                ),
                ("paused".to_string(), "false".to_string()),
                ("infoHashes".to_string(), "abc123def456".to_string()),
            ]),
            extra: Map::from_iter([("event".to_string(), json!("TEST"))]),
        })
        .await;
    tracing::info!(label = Label::Webhook.as_str(), "Sent test notification");
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WebhookObject;

    #[test]
    fn template_vars_substitute_recursively() {
        let vars = BTreeMap::from([
            ("name".to_string(), "Some.Release".to_string()),
            ("source".to_string(), "search".to_string()),
        ]);
        let payload = json!({
            "content": "{source} found {name}",
            "embeds": [{ "title": "{name}", "fields": [{ "value": "{unknown}" }] }],
            "count": 3
        });
        let substituted = substitute_template_value(&payload, &vars);
        assert_eq!(substituted["content"], json!("search found Some.Release"));
        assert_eq!(substituted["embeds"][0]["title"], json!("Some.Release"));
        // Unknown placeholders are left alone.
        assert_eq!(
            substituted["embeds"][0]["fields"][0]["value"],
            json!("{unknown}")
        );
        assert_eq!(substituted["count"], json!(3));
    }

    /// A user override must replace the default header, not be sent alongside
    /// it under different capitalisation.
    #[test]
    fn header_overrides_are_case_insensitive() {
        let defaults = default_headers();
        let overrides = BTreeMap::from([
            ("content-type".to_string(), "text/plain".to_string()),
            ("X-Token".to_string(), "abc".to_string()),
        ]);
        let merged = merge_headers(&defaults, Some(&overrides));

        assert_eq!(
            merged.get("content-type").map(String::as_str),
            Some("text/plain")
        );
        assert!(!merged.contains_key("Content-Type"));
        assert_eq!(merged.get("X-Token").map(String::as_str), Some("abc"));
        assert!(merged.contains_key("User-Agent"));
    }

    #[test]
    fn webhook_entries_without_overrides_keep_the_defaults() {
        let merged = merge_headers(&default_headers(), None);
        assert_eq!(
            merged.get("Content-Type").map(String::as_str),
            Some("application/json")
        );
    }

    #[test]
    fn notification_bodies_pluralise_and_list_trackers() {
        let assessment = ResultAssessment {
            decision: Decision::Match,
            metafile: None,
            meta_cached: false,
        };
        let results = [
            (
                assessment.clone(),
                "TrackerB".to_string(),
                ActionResult::Injection(InjectionResult::Success),
            ),
            (
                assessment,
                "TrackerA".to_string(),
                ActionResult::Injection(InjectionResult::Success),
            ),
        ];
        let refs: Vec<&(ResultAssessment, String, ActionResult)> = results.iter().collect();
        let notification = build_notification(
            &refs,
            "search",
            "torrentClient",
            "Injected",
            false,
            "INJECTED",
            &json!({}),
        );

        assert!(notification.body.contains("on 2 trackers"));
        // Trackers are sorted for a stable message.
        assert!(notification.body.ends_with("TrackerA and TrackerB"));
        assert_eq!(
            notification
                .template_vars
                .get("numTrackers")
                .map(String::as_str),
            Some("2")
        );
        assert_eq!(notification.extra["event"], json!("RESULTS"));
    }

    #[test]
    fn object_entries_expose_their_url_like_string_entries() {
        let string_entry = WebhookEntry::Url("https://hook.example/a".into());
        let object_entry = WebhookEntry::Object(WebhookObject {
            url: "https://hook.example/b".into(),
            payload: None,
            headers: None,
        });
        assert_eq!(string_entry.url(), "https://hook.example/a");
        assert_eq!(object_entry.url(), "https://hook.example/b");
    }
}
