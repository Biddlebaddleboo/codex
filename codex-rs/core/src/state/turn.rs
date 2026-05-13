//! Turn-scoped state and active turn metadata scaffolding.

use codex_sandboxing::policy_transforms::merge_permission_profiles;
use indexmap::IndexMap;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::request_permissions::RequestPermissionProfile;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_rmcp_client::ElicitationResponse;
use codex_utils_absolute_path::AbsolutePathBuf;
use rmcp::model::RequestId;
use tokio::sync::oneshot;

use crate::controller_validation::ControllerValidationState;
use crate::session::turn_context::TurnContext;
use crate::tasks::AnySessionTask;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::TokenUsage;

/// Metadata about the currently running turn.
pub(crate) struct ActiveTurn {
    pub(crate) tasks: IndexMap<String, RunningTask>,
    pub(crate) turn_state: Arc<Mutex<TurnState>>,
}

/// Whether mailbox deliveries should still be folded into the current turn.
///
/// State machine:
/// - A turn starts in `CurrentTurn`, so queued child mail can join the next
///   model request for that turn.
/// - After user-visible terminal output is recorded, we switch to `NextTurn`
///   to leave late child mail queued instead of extending an already shown
///   answer.
/// - If the same task later gets explicit same-turn work again (a steered user
///   prompt or a tool call after an untagged preamble), we reopen `CurrentTurn`
///   so that pending child mail is drained into that follow-up request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum MailboxDeliveryPhase {
    /// Incoming mailbox messages can still be consumed by the current turn.
    #[default]
    CurrentTurn,
    /// The current turn already emitted visible final answer text; mailbox
    /// messages should remain queued for a later turn.
    NextTurn,
}

impl Default for ActiveTurn {
    fn default() -> Self {
        Self {
            tasks: IndexMap::new(),
            turn_state: Arc::new(Mutex::new(TurnState::default())),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TaskKind {
    Regular,
    Review,
    Compact,
}

pub(crate) struct RunningTask {
    pub(crate) done: Arc<Notify>,
    pub(crate) kind: TaskKind,
    pub(crate) task: Arc<dyn AnySessionTask>,
    pub(crate) cancellation_token: CancellationToken,
    pub(crate) handle: AbortOnDropHandle<()>,
    pub(crate) turn_context: Arc<TurnContext>,
    // Timer recorded when the task drops to capture the full turn duration.
    pub(crate) _timer: Option<codex_otel::Timer>,
}

pub(crate) struct RemovedTask {
    pub(crate) records_turn_token_usage_on_span: bool,
    pub(crate) active_turn_is_empty: bool,
}

impl ActiveTurn {
    pub(crate) fn add_task(&mut self, task: RunningTask) {
        let sub_id = task.turn_context.sub_id.clone();
        self.tasks.insert(sub_id, task);
    }

    pub(crate) fn remove_task(&mut self, sub_id: &str) -> Option<RemovedTask> {
        let task = self.tasks.swap_remove(sub_id)?;
        let records_turn_token_usage_on_span = task.task.records_turn_token_usage_on_span();
        task.handle.detach();
        Some(RemovedTask {
            records_turn_token_usage_on_span,
            active_turn_is_empty: self.tasks.is_empty(),
        })
    }

    pub(crate) fn drain_tasks(&mut self) -> Vec<RunningTask> {
        self.tasks.drain(..).map(|(_, task)| task).collect()
    }
}

/// Mutable state for a single turn.
#[derive(Default)]
pub(crate) struct TurnState {
    pending_approvals: HashMap<String, oneshot::Sender<ReviewDecision>>,
    pending_request_permissions: HashMap<String, PendingRequestPermissions>,
    pending_user_input: HashMap<String, oneshot::Sender<RequestUserInputResponse>>,
    pending_elicitations: HashMap<(String, RequestId), oneshot::Sender<ElicitationResponse>>,
    pending_dynamic_tools: HashMap<String, oneshot::Sender<DynamicToolResponse>>,
    pending_input: Vec<ResponseInputItem>,
    pending_controller_validation: Option<ControllerValidationState>,
    terminal_controller_validation_result: Option<String>,
    controller_validation_active: bool,
    mailbox_delivery_phase: MailboxDeliveryPhase,
    granted_permissions: Option<AdditionalPermissionProfile>,
    strict_auto_review_enabled: bool,
    pub(crate) tool_calls: u64,
    pub(crate) has_memory_citation: bool,
    pub(crate) token_usage_at_turn_start: TokenUsage,
}

pub(crate) struct PendingRequestPermissions {
    pub(crate) tx_response: oneshot::Sender<RequestPermissionsResponse>,
    pub(crate) requested_permissions: RequestPermissionProfile,
    pub(crate) cwd: AbsolutePathBuf,
}

impl TurnState {
    pub(crate) fn insert_pending_approval(
        &mut self,
        key: String,
        tx: oneshot::Sender<ReviewDecision>,
    ) -> Option<oneshot::Sender<ReviewDecision>> {
        self.pending_approvals.insert(key, tx)
    }

    pub(crate) fn remove_pending_approval(
        &mut self,
        key: &str,
    ) -> Option<oneshot::Sender<ReviewDecision>> {
        self.pending_approvals.remove(key)
    }

    pub(crate) fn clear_pending(&mut self) {
        self.pending_approvals.clear();
        self.pending_request_permissions.clear();
        self.pending_user_input.clear();
        self.pending_elicitations.clear();
        self.pending_dynamic_tools.clear();
        self.pending_input.clear();
        self.pending_controller_validation = None;
        self.terminal_controller_validation_result = None;
        self.controller_validation_active = false;
    }

    pub(crate) fn insert_pending_request_permissions(
        &mut self,
        key: String,
        pending_request_permissions: PendingRequestPermissions,
    ) -> Option<PendingRequestPermissions> {
        self.pending_request_permissions
            .insert(key, pending_request_permissions)
    }

    pub(crate) fn remove_pending_request_permissions(
        &mut self,
        key: &str,
    ) -> Option<PendingRequestPermissions> {
        self.pending_request_permissions.remove(key)
    }

    pub(crate) fn insert_pending_user_input(
        &mut self,
        key: String,
        tx: oneshot::Sender<RequestUserInputResponse>,
    ) -> Option<oneshot::Sender<RequestUserInputResponse>> {
        self.pending_user_input.insert(key, tx)
    }

    pub(crate) fn remove_pending_user_input(
        &mut self,
        key: &str,
    ) -> Option<oneshot::Sender<RequestUserInputResponse>> {
        self.pending_user_input.remove(key)
    }

    pub(crate) fn insert_pending_elicitation(
        &mut self,
        server_name: String,
        request_id: RequestId,
        tx: oneshot::Sender<ElicitationResponse>,
    ) -> Option<oneshot::Sender<ElicitationResponse>> {
        self.pending_elicitations
            .insert((server_name, request_id), tx)
    }

    pub(crate) fn remove_pending_elicitation(
        &mut self,
        server_name: &str,
        request_id: &RequestId,
    ) -> Option<oneshot::Sender<ElicitationResponse>> {
        self.pending_elicitations
            .remove(&(server_name.to_string(), request_id.clone()))
    }

    pub(crate) fn insert_pending_dynamic_tool(
        &mut self,
        key: String,
        tx: oneshot::Sender<DynamicToolResponse>,
    ) -> Option<oneshot::Sender<DynamicToolResponse>> {
        self.pending_dynamic_tools.insert(key, tx)
    }

    pub(crate) fn remove_pending_dynamic_tool(
        &mut self,
        key: &str,
    ) -> Option<oneshot::Sender<DynamicToolResponse>> {
        self.pending_dynamic_tools.remove(key)
    }

    pub(crate) fn push_pending_input(&mut self, input: ResponseInputItem) {
        self.pending_input.push(input);
    }

    pub(crate) fn prepend_pending_input(&mut self, mut input: Vec<ResponseInputItem>) {
        if input.is_empty() {
            return;
        }

        input.append(&mut self.pending_input);
        self.pending_input = input;
    }

    pub(crate) fn take_pending_input(&mut self) -> Vec<ResponseInputItem> {
        if self.pending_input.is_empty() {
            Vec::with_capacity(0)
        } else {
            let mut ret = Vec::new();
            std::mem::swap(&mut ret, &mut self.pending_input);
            ret
        }
    }

    pub(crate) fn has_pending_input(&self) -> bool {
        !self.pending_input.is_empty()
    }

    pub(crate) fn accept_mailbox_delivery_for_current_turn(&mut self) {
        self.set_mailbox_delivery_phase(MailboxDeliveryPhase::CurrentTurn);
    }

    pub(crate) fn accepts_mailbox_delivery_for_current_turn(&self) -> bool {
        self.mailbox_delivery_phase == MailboxDeliveryPhase::CurrentTurn
    }

    pub(crate) fn set_mailbox_delivery_phase(&mut self, phase: MailboxDeliveryPhase) {
        self.mailbox_delivery_phase = phase;
    }

    pub(crate) fn record_granted_permissions(&mut self, permissions: AdditionalPermissionProfile) {
        self.granted_permissions =
            merge_permission_profiles(self.granted_permissions.as_ref(), Some(&permissions));
    }

    pub(crate) fn granted_permissions(&self) -> Option<AdditionalPermissionProfile> {
        self.granted_permissions.clone()
    }

    pub(crate) fn enable_strict_auto_review(&mut self) {
        self.strict_auto_review_enabled = true;
    }

    pub(crate) fn strict_auto_review_enabled(&self) -> bool {
        self.strict_auto_review_enabled
    }

    pub(crate) fn set_pending_controller_validation(
        &mut self,
        controller_validation: ControllerValidationState,
    ) {
        self.pending_controller_validation = Some(controller_validation);
        self.controller_validation_active = true;
    }

    pub(crate) fn take_pending_controller_validation(
        &mut self,
    ) -> Option<ControllerValidationState> {
        self.pending_controller_validation.take()
    }

    pub(crate) fn has_pending_controller_validation(&self) -> bool {
        self.pending_controller_validation.is_some()
    }

    pub(crate) fn set_terminal_controller_validation_result(&mut self, result: String) {
        self.terminal_controller_validation_result = Some(result);
    }

    pub(crate) fn take_terminal_controller_validation_result(&mut self) -> Option<String> {
        self.terminal_controller_validation_result.take()
    }

    pub(crate) fn has_terminal_controller_validation_result(&self) -> bool {
        self.terminal_controller_validation_result.is_some()
    }

    pub(crate) fn set_controller_validation_active(&mut self, active: bool) {
        self.controller_validation_active = active;
    }

    pub(crate) fn is_controller_validation_active(&self) -> bool {
        self.controller_validation_active
    }

    pub(crate) fn controller_validation_owns_turn_finalization(&self) -> bool {
        self.pending_controller_validation.is_some()
            || self.controller_validation_active
            || self.terminal_controller_validation_result.is_some()
    }
}

impl ActiveTurn {
    /// Clear any pending approvals and input buffered for the current turn.
    pub(crate) async fn clear_pending(&self) {
        let mut ts = self.turn_state.lock().await;
        ts.clear_pending();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller_validation::ControllerValidationState;
    use pretty_assertions::assert_eq;

    #[test]
    fn controller_validation_state_round_trips() {
        let mut turn_state = TurnState::default();
        let state = ControllerValidationState {
            commands: vec!["cargo check -p codex-core".to_string()],
            attempt: 0,
        };

        turn_state.set_pending_controller_validation(state.clone());
        assert!(turn_state.is_controller_validation_active());
        assert_eq!(turn_state.take_pending_controller_validation(), Some(state));
        assert_eq!(turn_state.take_pending_controller_validation(), None);
    }

    #[test]
    fn terminal_controller_validation_result_round_trips() {
        let mut turn_state = TurnState::default();
        assert!(!turn_state.has_terminal_controller_validation_result());

        turn_state.set_terminal_controller_validation_result("All checks passed.".to_string());
        assert!(turn_state.has_terminal_controller_validation_result());
        assert_eq!(
            turn_state.take_terminal_controller_validation_result(),
            Some("All checks passed.".to_string())
        );
        assert_eq!(
            turn_state.take_terminal_controller_validation_result(),
            None
        );
    }

    #[test]
    fn controller_validation_active_state_round_trips() {
        let mut turn_state = TurnState::default();
        assert!(!turn_state.is_controller_validation_active());
        turn_state.set_controller_validation_active(true);
        assert!(turn_state.is_controller_validation_active());
        turn_state.set_controller_validation_active(false);
        assert!(!turn_state.is_controller_validation_active());
    }

    #[test]
    fn controller_validation_owns_turn_finalization_when_any_signal_is_set() {
        let mut turn_state = TurnState::default();
        assert!(!turn_state.controller_validation_owns_turn_finalization());

        turn_state.set_controller_validation_active(true);
        assert!(turn_state.controller_validation_owns_turn_finalization());

        turn_state.set_controller_validation_active(false);
        turn_state.set_pending_controller_validation(ControllerValidationState {
            commands: vec!["cargo test -p codex-core".to_string()],
            attempt: 0,
        });
        assert!(turn_state.controller_validation_owns_turn_finalization());

        turn_state.take_pending_controller_validation();
        turn_state.set_terminal_controller_validation_result("done".to_string());
        assert!(turn_state.controller_validation_owns_turn_finalization());
    }
}
