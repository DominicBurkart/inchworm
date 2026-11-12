use crate::model::FailureClass;
use crate::nanna_client::{NannaFailure, TaskOutcome};

const KIND_TABLE: &[(&str, FailureClass)] = &[
    ("max_iterations", FailureClass::MaxIterations),
    ("state_error", FailureClass::AgentFailed),
    ("task_check_failed", FailureClass::TestsFailed),
    (
        "protected_path_violation",
        FailureClass::ProtectedPathViolation,
    ),
    ("scope_error", FailureClass::AgentFailed),
    ("identity_unavailable", FailureClass::SpawnRefused),
    ("model_unavailable", FailureClass::ModelError),
    ("container_setup_failed", FailureClass::ContainerBuildFailed),
    ("workspace_setup_failed", FailureClass::ContainerBuildFailed),
    (
        "offline_dependencies_failed",
        FailureClass::ContainerBuildFailed,
    ),
    ("impact_error", FailureClass::AgentFailed),
    ("queue_error", FailureClass::AgentFailed),
    ("restore_refused", FailureClass::SpawnRefused),
    ("spawn_refused", FailureClass::SpawnRefused),
    ("invalid_request", FailureClass::SpawnRefused),
    ("cancelled", FailureClass::Cancelled),
    ("verification_failed", FailureClass::TestsFailed),
    ("verifier_checks_invalid", FailureClass::SpawnRefused),
    (
        "verifier_requires_container",
        FailureClass::ContainerBuildFailed,
    ),
    ("verifier_exec_failed", FailureClass::AgentFailed),
];

const ERROR_TYPE_TABLE: &[(&str, FailureClass)] = &[
    ("MaxIterationsExceeded", FailureClass::MaxIterations),
    ("StateError", FailureClass::AgentFailed),
    ("TaskCheckFailed", FailureClass::TestsFailed),
    (
        "ProtectedPathViolation",
        FailureClass::ProtectedPathViolation,
    ),
    ("ScopeError", FailureClass::AgentFailed),
    ("IdentityUnavailable", FailureClass::SpawnRefused),
    ("NoProvider", FailureClass::ModelError),
    ("ContainerSetupFailed", FailureClass::ContainerBuildFailed),
    ("SidecarSetupFailed", FailureClass::ContainerBuildFailed),
    (
        "WorkspaceCreationFailed",
        FailureClass::ContainerBuildFailed,
    ),
    (
        "OfflineDependenciesFailed",
        FailureClass::ContainerBuildFailed,
    ),
    ("ImpactError", FailureClass::AgentFailed),
    ("QueuePersistFailed", FailureClass::AgentFailed),
    ("RestoreNoIdentity", FailureClass::SpawnRefused),
    ("RestoreOverridesInvalid", FailureClass::SpawnRefused),
    ("RestoreRefused", FailureClass::SpawnRefused),
    ("RestoreAuditFailed", FailureClass::SpawnRefused),
    ("RestoreAuditDeferred", FailureClass::SpawnRefused),
    ("VerificationFailed", FailureClass::TestsFailed),
    ("VerifierChecksInvalid", FailureClass::SpawnRefused),
    (
        "VerifierRequiresContainer",
        FailureClass::ContainerBuildFailed,
    ),
    ("VerifierExecFailed", FailureClass::AgentFailed),
];

const TEXT_TABLE: &[(&[&str], FailureClass)] = &[
    (
        &[
            "tests failed",
            "test failed",
            "verification failed",
            "check failed",
        ],
        FailureClass::TestsFailed,
    ),
    (&["spawn refused"], FailureClass::SpawnRefused),
    (
        &["protected path", "protected-path"],
        FailureClass::ProtectedPathViolation,
    ),
    (
        &["maximum iterations", "max iterations", "max_iterations"],
        FailureClass::MaxIterations,
    ),
    (&["timed out", "timeout"], FailureClass::Timeout),
    (&["cancelled", "canceled"], FailureClass::Cancelled),
    (
        &[
            "container image",
            "container build",
            "container setup",
            "container runtime",
            "image build",
            "image pull",
            "cargo vendor",
            "offline dependenc",
            "nix build",
        ],
        FailureClass::ContainerBuildFailed,
    ),
    (
        &[
            "ollama",
            "model provider",
            "no provider",
            "model unavailable",
            "connection refused",
        ],
        FailureClass::ModelError,
    ),
    (
        &["patch does not apply", "patch failed", "apply patch"],
        FailureClass::PatchApplyFailed,
    ),
];

fn lookup(table: &[(&str, FailureClass)], key: &str) -> Option<FailureClass> {
    table
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, class)| *class)
}

fn from_text(text: &str) -> Option<FailureClass> {
    let lowered = text.to_ascii_lowercase();
    TEXT_TABLE
        .iter()
        .find(|(needles, _)| needles.iter().any(|needle| lowered.contains(needle)))
        .map(|(_, class)| *class)
}

/// ```
/// use inchworm::classify::classify;
/// use inchworm::model::FailureClass;
/// use inchworm::nanna_client::NannaFailure;
///
/// let structured = NannaFailure {
///     failure_kind: Some("max_iterations".into()),
///     ..Default::default()
/// };
/// assert_eq!(classify(&structured), FailureClass::MaxIterations);
///
/// let legacy = NannaFailure {
///     error_type: Some("OfflineDependenciesFailed".into()),
///     ..Default::default()
/// };
/// assert_eq!(classify(&legacy), FailureClass::ContainerBuildFailed);
///
/// let opaque = NannaFailure {
///     message: "something nobody has seen before".into(),
///     ..Default::default()
/// };
/// assert_eq!(classify(&opaque), FailureClass::AgentFailed);
/// ```
pub fn classify(failure: &NannaFailure) -> FailureClass {
    failure
        .failure_kind
        .as_deref()
        .and_then(|kind| lookup(KIND_TABLE, kind))
        .or_else(|| {
            failure
                .error_type
                .as_deref()
                .and_then(|label| lookup(ERROR_TYPE_TABLE, label))
        })
        .or_else(|| from_text(&failure.message))
        .unwrap_or(FailureClass::AgentFailed)
}

/// ```
/// use inchworm::classify::classify_outcome;
/// use inchworm::model::FailureClass;
/// use inchworm::nanna_client::TaskOutcome;
///
/// assert_eq!(classify_outcome(&TaskOutcome::TimedOut), Some(FailureClass::Timeout));
/// assert_eq!(classify_outcome(&TaskOutcome::Cancelled), Some(FailureClass::Cancelled));
/// ```
pub fn classify_outcome(outcome: &TaskOutcome) -> Option<FailureClass> {
    match outcome {
        TaskOutcome::Completed(_) => None,
        TaskOutcome::Failed(failure) => Some(classify(failure)),
        TaskOutcome::Cancelled => Some(FailureClass::Cancelled),
        TaskOutcome::TimedOut => Some(FailureClass::Timeout),
        TaskOutcome::Refused(_) => Some(FailureClass::SpawnRefused),
    }
}

pub fn raw_error(outcome: &TaskOutcome) -> String {
    match outcome {
        TaskOutcome::Completed(_) => String::new(),
        TaskOutcome::Failed(failure) | TaskOutcome::Refused(failure) => {
            let mut parts = Vec::new();
            if let Some(kind) = &failure.failure_kind {
                parts.push(format!("failure_kind={kind}"));
            }
            if let Some(error_type) = &failure.error_type {
                parts.push(format!("error_type={error_type}"));
            }
            parts.push(failure.message.clone());
            parts.join(": ")
        }
        TaskOutcome::Cancelled => "task was cancelled".to_string(),
        TaskOutcome::TimedOut => "attempt timed out and the task was cancelled".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nanna_client::fake::{Scenario, connect};
    use proptest::prelude::*;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::time::Duration;

    fn failure(kind: Option<&str>, error_type: Option<&str>, message: &str) -> NannaFailure {
        NannaFailure {
            failure_kind: kind.map(str::to_string),
            error_type: error_type.map(str::to_string),
            message: message.to_string(),
        }
    }

    #[test]
    fn structured_kinds_map_per_table() {
        let cases = [
            ("max_iterations", FailureClass::MaxIterations),
            ("task_check_failed", FailureClass::TestsFailed),
            (
                "protected_path_violation",
                FailureClass::ProtectedPathViolation,
            ),
            ("model_unavailable", FailureClass::ModelError),
            ("container_setup_failed", FailureClass::ContainerBuildFailed),
            ("workspace_setup_failed", FailureClass::ContainerBuildFailed),
            (
                "offline_dependencies_failed",
                FailureClass::ContainerBuildFailed,
            ),
            ("verification_failed", FailureClass::TestsFailed),
            (
                "verifier_requires_container",
                FailureClass::ContainerBuildFailed,
            ),
            ("verifier_exec_failed", FailureClass::AgentFailed),
            ("verifier_checks_invalid", FailureClass::SpawnRefused),
            ("spawn_refused", FailureClass::SpawnRefused),
            ("invalid_request", FailureClass::SpawnRefused),
            ("identity_unavailable", FailureClass::SpawnRefused),
            ("restore_refused", FailureClass::SpawnRefused),
            ("cancelled", FailureClass::Cancelled),
            ("state_error", FailureClass::AgentFailed),
            ("scope_error", FailureClass::AgentFailed),
            ("impact_error", FailureClass::AgentFailed),
            ("queue_error", FailureClass::AgentFailed),
        ];
        for (kind, class) in cases {
            assert_eq!(classify(&failure(Some(kind), None, "")), class, "{kind}");
        }
    }

    #[test]
    fn error_type_labels_cover_offline_deps_and_verifier_kinds() {
        let cases = [
            (
                "OfflineDependenciesFailed",
                FailureClass::ContainerBuildFailed,
            ),
            ("VerificationFailed", FailureClass::TestsFailed),
            ("VerifierChecksInvalid", FailureClass::SpawnRefused),
            (
                "VerifierRequiresContainer",
                FailureClass::ContainerBuildFailed,
            ),
            ("VerifierExecFailed", FailureClass::AgentFailed),
            ("MaxIterationsExceeded", FailureClass::MaxIterations),
            ("NoProvider", FailureClass::ModelError),
            ("SidecarSetupFailed", FailureClass::ContainerBuildFailed),
            ("RestoreAuditDeferred", FailureClass::SpawnRefused),
        ];
        for (label, class) in cases {
            assert_eq!(classify(&failure(None, Some(label), "")), class, "{label}");
        }
    }

    #[test]
    fn structured_kind_wins_over_error_type_and_text() {
        let f = failure(
            Some("max_iterations"),
            Some("NoProvider"),
            "container exploded",
        );
        assert_eq!(classify(&f), FailureClass::MaxIterations);
    }

    #[test]
    fn unclassified_kind_falls_back_to_error_type_then_text() {
        let f = failure(Some("unclassified"), Some("OfflineDependenciesFailed"), "x");
        assert_eq!(classify(&f), FailureClass::ContainerBuildFailed);
        let f = failure(Some("unclassified"), Some("SomethingNew"), "Ollama said no");
        assert_eq!(classify(&f), FailureClass::ModelError);
    }

    #[test]
    fn a_failing_test_whose_name_contains_another_keyword_is_still_a_test_failure() {
        for text in [
            "tests failed: test_connection_timeout panicked",
            "2 tests failed: protected_path_rules, max_iterations_cap",
            "verification failed: check lint timed out",
            "check failed: cancelled_tasks_are_reported",
            "tests failed: the container image builder",
        ] {
            assert_eq!(
                classify(&failure(None, None, text)),
                FailureClass::TestsFailed,
                "{text}"
            );
        }
    }

    #[test]
    fn unrelated_text_with_a_bare_container_image_or_vendor_word_is_not_a_build_failure() {
        for text in [
            "failed to decode image attachment",
            "container_id missing from the log line",
            "the vendored crate foo panicked",
            "wrote a vendor-neutral summary",
        ] {
            assert_eq!(
                classify(&failure(None, None, text)),
                FailureClass::AgentFailed,
                "{text}"
            );
        }
        for text in [
            "could not build the container image",
            "cannot start container runtime",
            "podman: container setup failed",
            "image pull failed: no route",
            "cargo vendor failed: no Cargo.lock",
            "offline dependencies could not be prepared",
            "nix build exited 1",
        ] {
            assert_eq!(
                classify(&failure(None, None, text)),
                FailureClass::ContainerBuildFailed,
                "{text}"
            );
        }
    }

    #[test]
    fn text_fallback_recognises_common_messages() {
        let cases = [
            (
                "spawn refused: block: unknown_identity",
                FailureClass::SpawnRefused,
            ),
            (
                "Maximum iterations (100) exceeded",
                FailureClass::MaxIterations,
            ),
            ("operation timed out", FailureClass::Timeout),
            ("task cancelled", FailureClass::Cancelled),
            (
                "could not build the container image",
                FailureClass::ContainerBuildFailed,
            ),
            ("cannot reach Ollama", FailureClass::ModelError),
            (
                "patch does not apply cleanly",
                FailureClass::PatchApplyFailed,
            ),
            ("3 tests failed", FailureClass::TestsFailed),
            (
                "write to protected path .github/ci.yml",
                FailureClass::ProtectedPathViolation,
            ),
            ("nothing recognisable", FailureClass::AgentFailed),
            ("", FailureClass::AgentFailed),
        ];
        for (text, class) in cases {
            assert_eq!(classify(&failure(None, None, text)), class, "{text}");
        }
    }

    #[test]
    fn tables_only_use_known_distinct_names() {
        let kinds: HashSet<_> = KIND_TABLE.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds.len(), KIND_TABLE.len());
        let labels: HashSet<_> = ERROR_TYPE_TABLE.iter().map(|(k, _)| *k).collect();
        assert_eq!(labels.len(), ERROR_TYPE_TABLE.len());
        for (kind, _) in KIND_TABLE {
            assert!(kind.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        }
    }

    #[test]
    fn outcomes_map_to_classes() {
        use crate::nanna_client::TaskDone;
        let done = TaskDone {
            result_summary: String::new(),
            changes_patch: None,
            format_patch: None,
            files_modified: vec![],
            denial_count: 0,
            iterations: 0,
            model_used: String::new(),
            raw: serde_json::Value::Null,
        };
        assert_eq!(classify_outcome(&TaskOutcome::Completed(done)), None);
        assert_eq!(
            classify_outcome(&TaskOutcome::Cancelled),
            Some(FailureClass::Cancelled)
        );
        assert_eq!(
            classify_outcome(&TaskOutcome::TimedOut),
            Some(FailureClass::Timeout)
        );
        let refused = TaskOutcome::Refused(failure(Some("invalid_request"), None, "m"));
        assert_eq!(classify_outcome(&refused), Some(FailureClass::SpawnRefused));
        let failed = TaskOutcome::Failed(failure(Some("max_iterations"), None, "m"));
        assert_eq!(classify_outcome(&failed), Some(FailureClass::MaxIterations));
    }

    #[test]
    fn raw_error_always_keeps_the_original_text() {
        let f = TaskOutcome::Failed(failure(Some("k"), Some("T"), "boom"));
        assert_eq!(raw_error(&f), "failure_kind=k: error_type=T: boom");
        let bare = TaskOutcome::Refused(failure(None, None, "nope"));
        assert_eq!(raw_error(&bare), "nope");
        assert!(raw_error(&TaskOutcome::Cancelled).contains("cancelled"));
        assert!(raw_error(&TaskOutcome::TimedOut).contains("timed out"));
        assert_eq!(
            raw_error(&TaskOutcome::Completed(crate::nanna_client::TaskDone {
                result_summary: String::new(),
                changes_patch: None,
                format_patch: None,
                files_modified: vec![],
                denial_count: 0,
                iterations: 0,
                model_used: String::new(),
                raw: serde_json::Value::Null,
            })),
            ""
        );
    }

    #[test]
    fn wire_fixtures_classify_end_to_end() {
        let cases = [
            ("result_failed_legacy", FailureClass::MaxIterations),
            ("result_failed_structured", FailureClass::MaxIterations),
            (
                "result_failed_offline_deps",
                FailureClass::ContainerBuildFailed,
            ),
        ];
        for (name, class) in cases {
            let (mut client, _fake) = connect(vec![Scenario::failed(name)]);
            client.initialize().unwrap();
            let request = crate::nanna_client::AssignRequest {
                description: "d".into(),
                repo_path: PathBuf::from("/r"),
                identity: "i".into(),
                branch: "main".into(),
            };
            let outcome = client.run_task(&request, Duration::from_secs(10)).unwrap();
            assert_eq!(classify_outcome(&outcome), Some(class), "{name}");
        }
    }

    proptest! {
        #[test]
        fn classification_is_total(
            kind in proptest::option::of(".{0,40}"),
            error_type in proptest::option::of(".{0,40}"),
            message in ".{0,200}",
        ) {
            let class = classify(&NannaFailure {
                failure_kind: kind,
                error_type,
                message,
            });
            prop_assert!(FailureClass::ALL.contains(&class));
        }

        #[test]
        fn known_kinds_ignore_the_message(idx in 0..KIND_TABLE.len(), message in ".{0,100}") {
            let (kind, class) = KIND_TABLE[idx];
            let f = failure(Some(kind), None, &message);
            prop_assert_eq!(classify(&f), class);
        }
    }
}
