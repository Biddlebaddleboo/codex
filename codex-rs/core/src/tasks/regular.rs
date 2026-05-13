use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::controller_validation::ControllerValidationRunResult;
use crate::controller_validation::ControllerValidationState;
use crate::controller_validation::build_validation_repair_prompt;
use crate::session::turn::run_turn;
use crate::session::turn_context::TurnContext;
use crate::session_startup_prewarm::SessionStartupPrewarmResolution;
use crate::state::TaskKind;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::user_input::UserInput;
use tracing::Instrument;
use tracing::trace_span;
use tracing::warn;

use super::SessionTask;
use super::SessionTaskContext;

#[derive(Default)]
pub(crate) struct RegularTask;

const MAX_CONTROLLER_VALIDATION_REPAIR_ATTEMPTS: u8 = 2;

enum ControllerValidationAction {
    Retry {
        validation: ControllerValidationState,
        failure_summary: String,
        repair_prompt: String,
    },
    Terminal {
        message: String,
    },
}

impl RegularTask {
    pub(crate) fn new() -> Self {
        Self
    }
}

fn next_controller_validation_action(
    mut validation: ControllerValidationState,
    run_result: ControllerValidationRunResult,
) -> ControllerValidationAction {
    match run_result {
        ControllerValidationRunResult::Passed { message } => {
            ControllerValidationAction::Terminal { message }
        }
        ControllerValidationRunResult::Failed {
            message,
            failed_command,
            ..
        } => {
            if validation.has_attempts_remaining(MAX_CONTROLLER_VALIDATION_REPAIR_ATTEMPTS) {
                validation.increment_attempt();
                validation.failed_command_first(&failed_command);
                return ControllerValidationAction::Retry {
                    validation,
                    failure_summary: message.clone(),
                    repair_prompt: build_validation_repair_prompt(&message),
                };
            }
            ControllerValidationAction::Terminal { message }
        }
    }
}

impl SessionTask for RegularTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.turn"
    }

    fn records_turn_token_usage_on_span(&self) -> bool {
        true
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> Option<String> {
        let sess = session.clone_session();
        let run_turn_span = trace_span!("run_turn");
        // Regular turns emit `TurnStarted` inline so first-turn lifecycle does
        // not wait on startup prewarm resolution.
        let event = EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: ctx.sub_id.clone(),
            started_at: ctx.turn_timing_state.started_at_unix_secs().await,
            model_context_window: ctx.model_context_window(),
            collaboration_mode_kind: ctx.collaboration_mode.mode,
        });
        sess.send_event(ctx.as_ref(), event).await;
        sess.set_server_reasoning_included(/*included*/ false).await;
        let prewarmed_client_session = match sess
            .consume_startup_prewarm_for_regular_turn(&cancellation_token)
            .await
        {
            SessionStartupPrewarmResolution::Cancelled => return None,
            SessionStartupPrewarmResolution::Unavailable { .. } => None,
            SessionStartupPrewarmResolution::Ready(prewarmed_client_session) => {
                Some(*prewarmed_client_session)
            }
        };
        let mut next_input = input;
        let mut prewarmed_client_session = prewarmed_client_session;
        loop {
            let last_agent_message = run_turn(
                Arc::clone(&sess),
                Arc::clone(&ctx),
                next_input,
                prewarmed_client_session.take(),
                cancellation_token.child_token(),
            )
            .instrument(run_turn_span.clone())
            .await;
            if let Some(validation) = sess.take_pending_controller_validation(&ctx.sub_id).await {
                // Controller now owns turn finalization; defer Stop/AfterAgent
                // until terminal controller-validation result is finalized.
                sess.set_controller_validation_active(&ctx.sub_id, true)
                    .await;
                let run_result = sess
                    .run_controller_validation_commands(&ctx, &validation)
                    .await;
                match next_controller_validation_action(validation, run_result) {
                    // Repair attempts are same-turn subphases. Do not use
                    // `start_task(...)` or emit extra TurnStarted/TurnComplete.
                    ControllerValidationAction::Retry {
                        validation,
                        failure_summary,
                        repair_prompt,
                    } => {
                        sess.set_pending_controller_validation(&ctx.sub_id, validation)
                            .await;
                        let repair_prompt_item = ResponseInputItem::Message {
                            role: "developer".to_string(),
                            content: vec![ContentItem::InputText {
                                text: repair_prompt,
                            }],
                            phase: None,
                        };
                        if let Err(items) =
                            sess.inject_response_items(vec![repair_prompt_item]).await
                        {
                            warn!(
                                turn_id = %ctx.sub_id,
                                count = items.len(),
                                "failed to inject controller validation repair prompt; finalize terminal failure"
                            );
                            let failure_message = format!(
                                "{failure_summary}\n\nRepair prompt handoff failed: queued retry input was rejected for active turn."
                            );
                            sess.set_terminal_controller_validation_result(
                                &ctx.sub_id,
                                failure_message,
                            )
                            .await;
                            return None;
                        }
                        // Wake next same-turn model pass from pending-input path
                        // in `run_turn(...)` without new turn lifecycle events.
                        if !sess.has_pending_input().await {
                            let failure_message = format!(
                                "{failure_summary}\n\nRepair prompt handoff failed: retry input was not pending for model consumption."
                            );
                            sess.set_terminal_controller_validation_result(
                                &ctx.sub_id,
                                failure_message,
                            )
                            .await;
                            return None;
                        }
                        next_input = Vec::new();
                        continue;
                    }
                    // Stop/AfterAgent run once at terminal controller result in
                    // `on_task_finished(...)`.
                    ControllerValidationAction::Terminal { message } => {
                        sess.set_terminal_controller_validation_result(&ctx.sub_id, message)
                            .await;
                        return None;
                    }
                }
            }
            if !sess.has_pending_input().await {
                return last_agent_message;
            }
            next_input = Vec::new();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn failed_validation_with_attempts_remaining_increments_attempt_and_moves_failed_command_first()
    {
        let state = ControllerValidationState {
            commands: vec!["c1".to_string(), "c2".to_string(), "c3".to_string()],
            attempt: 0,
        };
        let run_result = ControllerValidationRunResult::Failed {
            message: "fail".to_string(),
            failed_command: "c2".to_string(),
            exit_code: 7,
        };

        let action = next_controller_validation_action(state, run_result);
        let ControllerValidationAction::Retry {
            validation,
            failure_summary,
            repair_prompt,
        } = action
        else {
            panic!("expected retry");
        };
        assert_eq!(validation.attempt(), 1);
        assert_eq!(
            validation.commands(),
            &["c2".to_string(), "c1".to_string(), "c3".to_string()]
        );
        assert_eq!(failure_summary, "fail".to_string());
        assert_eq!(repair_prompt, build_validation_repair_prompt("fail"));
    }

    #[test]
    fn failed_validation_with_exhausted_attempts_is_terminal() {
        let state = ControllerValidationState {
            commands: vec!["c1".to_string()],
            attempt: 2,
        };
        let run_result = ControllerValidationRunResult::Failed {
            message: "final fail".to_string(),
            failed_command: "c1".to_string(),
            exit_code: 1,
        };

        let action = next_controller_validation_action(state, run_result);
        let ControllerValidationAction::Terminal { message } = action else {
            panic!("expected terminal");
        };
        assert_eq!(message, "final fail".to_string());
    }
}
