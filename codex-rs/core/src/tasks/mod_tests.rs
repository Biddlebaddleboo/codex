use super::controller_validation_command_completion_message;
use super::controller_validation_command_start_message;
use super::emit_turn_memory_metric;
use super::emit_turn_network_proxy_metric;
use crate::controller_validation::ControllerValidationRunResult;
use crate::controller_validation::ControllerValidationState;
use crate::session::tests::make_session_and_context_with_rx;
use codex_otel::MetricsClient;
use codex_otel::MetricsConfig;
use codex_otel::SessionTelemetry;
use codex_otel::TURN_MEMORY_METRIC;
use codex_otel::TURN_NETWORK_PROXY_METRIC;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::WarningEvent;
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::data::AggregatedMetrics;
use opentelemetry_sdk::metrics::data::Metric;
use opentelemetry_sdk::metrics::data::MetricData;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use tokio::time::timeout;

fn test_session_telemetry() -> SessionTelemetry {
    let exporter = InMemoryMetricExporter::default();
    let metrics = MetricsClient::new(
        MetricsConfig::in_memory("test", "codex-core", env!("CARGO_PKG_VERSION"), exporter)
            .with_runtime_reader(),
    )
    .expect("in-memory metrics client");
    SessionTelemetry::new(
        ThreadId::new(),
        "gpt-5.4",
        "gpt-5.4",
        /*account_id*/ None,
        /*account_email*/ None,
        /*auth_mode*/ None,
        "test_originator".to_string(),
        /*log_user_prompts*/ false,
        "tty".to_string(),
        SessionSource::Cli,
    )
    .with_metrics_without_metadata_tags(metrics)
}

fn find_metric<'a>(resource_metrics: &'a ResourceMetrics, name: &str) -> &'a Metric {
    for scope_metrics in resource_metrics.scope_metrics() {
        for metric in scope_metrics.metrics() {
            if metric.name() == name {
                return metric;
            }
        }
    }
    panic!("metric {name} missing");
}

fn attributes_to_map<'a>(
    attributes: impl Iterator<Item = &'a KeyValue>,
) -> BTreeMap<String, String> {
    attributes
        .map(|kv| (kv.key.as_str().to_string(), kv.value.as_str().to_string()))
        .collect()
}

fn metric_point(resource_metrics: &ResourceMetrics, name: &str) -> (BTreeMap<String, String>, u64) {
    let metric = find_metric(resource_metrics, name);
    match metric.data() {
        AggregatedMetrics::U64(data) => match data {
            MetricData::Sum(sum) => {
                let points: Vec<_> = sum.data_points().collect();
                assert_eq!(points.len(), 1);
                let point = points[0];
                (attributes_to_map(point.attributes()), point.value())
            }
            _ => panic!("unexpected counter aggregation"),
        },
        _ => panic!("unexpected counter data type"),
    }
}

#[test]
fn emit_turn_network_proxy_metric_records_active_turn() {
    let session_telemetry = test_session_telemetry();

    emit_turn_network_proxy_metric(
        &session_telemetry,
        /*network_proxy_active*/ true,
        ("tmp_mem_enabled", "true"),
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_NETWORK_PROXY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("active".to_string(), "true".to_string()),
            ("tmp_mem_enabled".to_string(), "true".to_string()),
        ])
    );
}

#[test]
fn emit_turn_network_proxy_metric_records_inactive_turn() {
    let session_telemetry = test_session_telemetry();

    emit_turn_network_proxy_metric(
        &session_telemetry,
        /*network_proxy_active*/ false,
        ("tmp_mem_enabled", "false"),
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_NETWORK_PROXY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("active".to_string(), "false".to_string()),
            ("tmp_mem_enabled".to_string(), "false".to_string()),
        ])
    );
}

#[test]
fn emit_turn_memory_metric_records_read_allowed_with_citations() {
    let session_telemetry = test_session_telemetry();

    emit_turn_memory_metric(
        &session_telemetry,
        /*feature_enabled*/ true,
        /*config_enabled*/ true,
        /*has_citations*/ true,
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_MEMORY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("config_use_memories".to_string(), "true".to_string()),
            ("feature_enabled".to_string(), "true".to_string()),
            ("has_citations".to_string(), "true".to_string()),
            ("read_allowed".to_string(), "true".to_string()),
        ])
    );
}

#[test]
fn emit_turn_memory_metric_records_config_disabled_without_citations() {
    let session_telemetry = test_session_telemetry();

    emit_turn_memory_metric(
        &session_telemetry,
        /*feature_enabled*/ true,
        /*config_enabled*/ false,
        /*has_citations*/ false,
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_MEMORY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("config_use_memories".to_string(), "false".to_string()),
            ("feature_enabled".to_string(), "true".to_string()),
            ("has_citations".to_string(), "false".to_string()),
            ("read_allowed".to_string(), "false".to_string()),
        ])
    );
}

#[test]
fn controller_validation_status_messages_include_command_and_exit_code() {
    let command = "cargo test -p codex-core";
    assert_eq!(
        controller_validation_command_start_message(command),
        "Running build/test command: cargo test -p codex-core".to_string()
    );
    assert_eq!(
        controller_validation_command_completion_message(command, 0),
        "Build/test command passed: cargo test -p codex-core (exit code 0)".to_string()
    );
    assert_eq!(
        controller_validation_command_completion_message(command, 7),
        "Build/test command failed: cargo test -p codex-core (exit code 7)".to_string()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controller_validation_commands_emit_visible_status_item_events() {
    let (session, turn_context, rx) = make_session_and_context_with_rx().await;
    let command = "true";
    let validation = ControllerValidationState {
        commands: vec![command.to_string()],
        attempt: 0,
    };

    while rx.try_recv().is_ok() {}

    let result = session
        .run_controller_validation_commands(&turn_context, &validation)
        .await;
    assert_eq!(
        result,
        ControllerValidationRunResult::Passed {
            message: "All checks passed.".to_string()
        }
    );

    let expected_start = controller_validation_command_start_message(command);
    let expected_completion = controller_validation_command_completion_message(command, 0);
    let mut saw_start_item = false;
    let mut saw_completion_item = false;
    let mut saw_status_warning = false;

    let _ = timeout(std::time::Duration::from_secs(5), async {
        while !saw_start_item || !saw_completion_item {
            let event = rx.recv().await.expect("channel open");
            match event.msg {
                EventMsg::ItemCompleted(ItemCompletedEvent {
                    item: TurnItem::AgentMessage(agent_message),
                    ..
                }) => {
                    let text = agent_message
                        .content
                        .iter()
                        .map(|content| match content {
                            AgentMessageContent::Text { text } => text.as_str(),
                        })
                        .collect::<String>();
                    if text == expected_start {
                        saw_start_item = true;
                    }
                    if text == expected_completion {
                        saw_completion_item = true;
                    }
                }
                EventMsg::Warning(WarningEvent { message }) => {
                    if message == expected_start || message == expected_completion {
                        saw_status_warning = true;
                    }
                }
                _ => {}
            }
        }
    })
    .await
    .expect("expected status item events");

    assert!(saw_start_item);
    assert!(saw_completion_item);
    assert!(!saw_status_warning);

    let history = session.clone_history().await;
    assert!(!history.raw_items().iter().any(|item| {
        matches!(
            item,
            ResponseItem::Message { role, content, .. }
                if role == "assistant"
                    && content.iter().any(|part| matches!(
                        part,
                        codex_protocol::models::ContentItem::OutputText { text }
                            if text == &expected_start || text == &expected_completion
                    ))
        )
    }));
}
