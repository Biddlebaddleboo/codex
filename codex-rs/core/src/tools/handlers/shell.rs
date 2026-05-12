use codex_features::Feature;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::models::SandboxPermissions;
use codex_protocol::models::ShellCommandToolCallParams;
use codex_protocol::models::ShellToolCallParams;
use codex_protocol::protocol::AskForApproval;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::exec::ExecCapturePolicy;
use crate::exec::ExecExpiration;
use crate::exec::ExecParams;
use crate::exec_env::create_env;
use crate::exec_policy::ExecApprovalRequest;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::handlers::apply_granted_turn_permissions;
use crate::tools::handlers::apply_patch::intercept_apply_patch;
use crate::tools::handlers::implicit_granted_permissions;
use crate::tools::handlers::normalize_and_validate_additional_permissions;
use crate::tools::handlers::parse_arguments;
use crate::tools::hook_names::HookToolName;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::runtimes::shell::ShellRequest;
use crate::tools::runtimes::shell::ShellRuntime;
use crate::tools::runtimes::shell::ShellRuntimeBackend;
use crate::tools::sandboxing::ToolCtx;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::ExecCommandSource;

mod container_exec;
mod local_shell;
mod shell_command;
mod shell_handler;

pub use container_exec::ContainerExecHandler;
pub use local_shell::LocalShellHandler;
pub use shell_command::ShellCommandHandler;
pub use shell_handler::ShellHandler;

/// Timeout for controller validation commands. Build/test commands can
/// legitimately take longer than the default shell tool timeout (10 s).
/// A five-minute ceiling keeps validation bounded without timing out
/// reasonable builds.
const CONTROLLER_VALIDATION_TIMEOUT_MS: u64 = 5 * 60 * 1000; // 5 minutes

fn shell_function_payload_command(payload: &ToolPayload) -> Option<String> {
    let ToolPayload::Function { arguments } = payload else {
        return None;
    };

    parse_arguments::<ShellToolCallParams>(arguments)
        .ok()
        .map(|params| codex_shell_command::parse_command::shlex_join(&params.command))
}

fn local_shell_payload_command(payload: &ToolPayload) -> Option<String> {
    let ToolPayload::LocalShell { params } = payload else {
        return None;
    };

    Some(codex_shell_command::parse_command::shlex_join(
        &params.command,
    ))
}

fn shell_command_payload_command(payload: &ToolPayload) -> Option<String> {
    let ToolPayload::Function { arguments } = payload else {
        return None;
    };

    parse_arguments::<ShellCommandToolCallParams>(arguments)
        .ok()
        .map(|params| params.command)
}

struct RunExecLikeArgs {
    tool_name: String,
    exec_params: ExecParams,
    hook_command: String,
    additional_permissions: Option<AdditionalPermissionProfile>,
    prefix_rule: Option<Vec<String>>,
    session: Arc<crate::session::session::Session>,
    turn: Arc<TurnContext>,
    tracker: crate::tools::context::SharedTurnDiffTracker,
    call_id: String,
    freeform: bool,
    shell_runtime_backend: ShellRuntimeBackend,
}

fn shell_function_pre_tool_use_payload(invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
    shell_function_payload_command(&invocation.payload).map(|command| PreToolUsePayload {
        tool_name: HookToolName::bash(),
        tool_input: serde_json::json!({ "command": command }),
    })
}

fn shell_function_post_tool_use_payload(
    invocation: &ToolInvocation,
    result: &FunctionToolOutput,
) -> Option<PostToolUsePayload> {
    let tool_response = result.post_tool_use_response(&invocation.call_id, &invocation.payload)?;
    let command = shell_function_payload_command(&invocation.payload)?;
    Some(PostToolUsePayload {
        tool_name: HookToolName::bash(),
        tool_use_id: invocation.call_id.clone(),
        tool_input: serde_json::json!({ "command": command }),
        tool_response,
    })
}

async fn run_exec_like(args: RunExecLikeArgs) -> Result<FunctionToolOutput, FunctionCallError> {
    let RunExecLikeArgs {
        tool_name,
        exec_params,
        hook_command,
        additional_permissions,
        prefix_rule,
        session,
        turn,
        tracker,
        call_id,
        freeform,
        shell_runtime_backend,
    } = args;

    let mut exec_params = exec_params;
    let Some(turn_environment) = turn.environments.primary() else {
        return Err(FunctionCallError::RespondToModel(
            "shell is unavailable in this session".to_string(),
        ));
    };
    let fs = turn_environment.environment.get_filesystem();

    let dependency_env = session.dependency_env().await;
    if !dependency_env.is_empty() {
        exec_params.env.extend(dependency_env.clone());
    }

    let mut explicit_env_overrides = turn.shell_environment_policy.r#set.clone();
    for key in dependency_env.keys() {
        if let Some(value) = exec_params.env.get(key) {
            explicit_env_overrides.insert(key.clone(), value.clone());
        }
    }

    let exec_permission_approvals_enabled =
        session.features().enabled(Feature::ExecPermissionApprovals);
    let requested_additional_permissions = additional_permissions.clone();
    let effective_additional_permissions = apply_granted_turn_permissions(
        session.as_ref(),
        turn.cwd.as_path(),
        exec_params.sandbox_permissions,
        additional_permissions,
    )
    .await;
    let additional_permissions_allowed = exec_permission_approvals_enabled
        || (session.features().enabled(Feature::RequestPermissionsTool)
            && effective_additional_permissions.permissions_preapproved);
    let normalized_additional_permissions = implicit_granted_permissions(
        exec_params.sandbox_permissions,
        requested_additional_permissions.as_ref(),
        &effective_additional_permissions,
    )
    .map_or_else(
        || {
            normalize_and_validate_additional_permissions(
                additional_permissions_allowed,
                turn.approval_policy.value(),
                effective_additional_permissions.sandbox_permissions,
                effective_additional_permissions.additional_permissions,
                effective_additional_permissions.permissions_preapproved,
                &exec_params.cwd,
            )
        },
        |permissions| Ok(Some(permissions)),
    )
    .map_err(FunctionCallError::RespondToModel)?;

    // Approval policy guard for explicit escalation in non-OnRequest modes.
    // Sticky turn permissions have already been approved, so they should
    // continue through the normal exec approval flow for the command.
    if effective_additional_permissions
        .sandbox_permissions
        .requests_sandbox_override()
        && !effective_additional_permissions.permissions_preapproved
        && !matches!(
            turn.approval_policy.value(),
            codex_protocol::protocol::AskForApproval::OnRequest
        )
    {
        let approval_policy = turn.approval_policy.value();
        return Err(FunctionCallError::RespondToModel(format!(
            "approval policy is {approval_policy:?}; reject command — you should not ask for escalated permissions if the approval policy is {approval_policy:?}"
        )));
    }

    // Intercept apply_patch if present.
    if let Some(output) = intercept_apply_patch(
        &exec_params.command,
        &exec_params.cwd,
        fs.as_ref(),
        session.clone(),
        turn.clone(),
        Some(&tracker),
        &call_id,
        tool_name.as_str(),
    )
    .await?
    {
        return Ok(output);
    }

    let source = ExecCommandSource::Agent;
    let emitter = ToolEmitter::shell(
        exec_params.command.clone(),
        exec_params.cwd.clone(),
        source,
        freeform,
    );
    let event_ctx = ToolEventCtx::new(
        session.as_ref(),
        turn.as_ref(),
        &call_id,
        /*turn_diff_tracker*/ None,
    );
    emitter.begin(event_ctx).await;

    let file_system_sandbox_policy = turn.file_system_sandbox_policy();
    let exec_approval_requirement = session
        .services
        .exec_policy
        .create_exec_approval_requirement_for_command(ExecApprovalRequest {
            command: &exec_params.command,
            approval_policy: turn.approval_policy.value(),
            permission_profile: turn.permission_profile(),
            file_system_sandbox_policy: &file_system_sandbox_policy,
            sandbox_cwd: turn.cwd.as_path(),
            sandbox_permissions: if effective_additional_permissions.permissions_preapproved {
                codex_protocol::models::SandboxPermissions::UseDefault
            } else {
                effective_additional_permissions.sandbox_permissions
            },
            prefix_rule,
        })
        .await;

    let req = ShellRequest {
        command: exec_params.command.clone(),
        hook_command,
        cwd: exec_params.cwd.clone(),
        timeout_ms: exec_params.expiration.timeout_ms(),
        env: exec_params.env.clone(),
        explicit_env_overrides,
        network: exec_params.network.clone(),
        sandbox_permissions: effective_additional_permissions.sandbox_permissions,
        additional_permissions: normalized_additional_permissions,
        #[cfg(unix)]
        additional_permissions_preapproved: effective_additional_permissions
            .permissions_preapproved,
        justification: exec_params.justification.clone(),
        exec_approval_requirement,
    };
    let mut orchestrator = ToolOrchestrator::new();
    let mut runtime = {
        use ShellRuntimeBackend::*;
        match shell_runtime_backend {
            Generic => ShellRuntime::new(),
            backend @ (ShellCommandClassic | ShellCommandZshFork) => {
                ShellRuntime::for_shell_command(backend)
            }
        }
    };
    let tool_ctx = ToolCtx {
        session: session.clone(),
        turn: turn.clone(),
        call_id: call_id.clone(),
        tool_name,
    };
    let out = orchestrator
        .run(
            &mut runtime,
            &req,
            &tool_ctx,
            &turn,
            turn.approval_policy.value(),
        )
        .await
        .map(|result| result.output);
    let event_ctx = ToolEventCtx::new(
        session.as_ref(),
        turn.as_ref(),
        &call_id,
        /*turn_diff_tracker*/ None,
    );
    let post_tool_use_response = out
        .as_ref()
        .ok()
        .map(|output| crate::tools::format_exec_output_str(output, turn.truncation_policy))
        .map(JsonValue::String);
    let content = emitter
        .finish(event_ctx, out, /*applied_patch_delta*/ None)
        .await?;
    Ok(FunctionToolOutput {
        body: vec![
            codex_protocol::models::FunctionCallOutputContentItem::InputText { text: content },
        ],
        success: Some(true),
        post_tool_use_response,
    })
}

/// Run a single controller-validation shell command through the safe
/// `ToolOrchestrator + ShellRuntime` path.
///
/// This reuses the same permission, sandbox, and approval machinery as
/// model-initiated shell tool calls, but intentionally skips
/// `apply_patch` interception because controller validation runs
/// build/test commands — not patch applications.
pub(crate) async fn run_controller_validation_shell_command(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    command: &str,
) -> Result<ExecToolCallOutput, FunctionCallError> {
    // Derive shell argv from the command string as a shell snippet.
    // `derive_exec_args` wraps the snippet in `$SHELL -lc <snippet>`
    // without whitespace splitting, preserving pipes, redirects, etc.
    let shell = session.user_shell();
    let use_login_shell = true;
    let shell_argv = shell.derive_exec_args(command, use_login_shell);

    // Ensure a primary environment is available for shell execution.
    let Some(_turn_environment) = turn.environments.primary() else {
        return Err(FunctionCallError::RespondToModel(
            "controller validation: shell is unavailable in this session".to_string(),
        ));
    };

    let mut exec_params = ExecParams {
        command: shell_argv.clone(),
        cwd: turn.cwd.clone(),
        expiration: ExecExpiration::Timeout(Duration::from_millis(
            CONTROLLER_VALIDATION_TIMEOUT_MS,
        )),
        capture_policy: ExecCapturePolicy::ShellTool,
        env: create_env(
            &turn.shell_environment_policy,
            Some(session.conversation_id),
        ),
        network: turn.network.clone(),
        sandbox_permissions: SandboxPermissions::UseDefault,
        windows_sandbox_level: turn.windows_sandbox_level,
        windows_sandbox_private_desktop: turn
            .config
            .permissions
            .windows_sandbox_private_desktop,
        justification: None,
        arg0: None,
    };

    // Apply dependency env (same pattern as run_exec_like).
    let dependency_env = session.dependency_env().await;
    if !dependency_env.is_empty() {
        exec_params.env.extend(dependency_env.clone());
    }

    // Compute explicit env overrides (same pattern as run_exec_like).
    let mut explicit_env_overrides = turn.shell_environment_policy.r#set.clone();
    for key in dependency_env.keys() {
        if let Some(value) = exec_params.env.get(key) {
            explicit_env_overrides.insert(key.clone(), value.clone());
        }
    }

    // Apply granted turn permissions.
    // Controller validation has no model-requested additional permissions.
    let exec_permission_approvals_enabled =
        session.features().enabled(Feature::ExecPermissionApprovals);
    let effective_additional_permissions = apply_granted_turn_permissions(
        session.as_ref(),
        turn.cwd.as_path(),
        exec_params.sandbox_permissions,
        /*additional_permissions*/ None,
    )
    .await;
    let additional_permissions_allowed = exec_permission_approvals_enabled
        || (session.features().enabled(Feature::RequestPermissionsTool)
            && effective_additional_permissions.permissions_preapproved);
    let normalized_additional_permissions = implicit_granted_permissions(
        exec_params.sandbox_permissions,
        /*additional_permissions*/ None,
        &effective_additional_permissions,
    )
    .map_or_else(
        || {
            normalize_and_validate_additional_permissions(
                additional_permissions_allowed,
                turn.approval_policy.value(),
                effective_additional_permissions.sandbox_permissions,
                effective_additional_permissions.additional_permissions,
                effective_additional_permissions.permissions_preapproved,
                &exec_params.cwd,
            )
        },
        |permissions| Ok(Some(permissions)),
    )
    .map_err(FunctionCallError::RespondToModel)?;

    // Approval policy guard for explicit escalation in non-OnRequest modes
    // (same guard as run_exec_like).
    if effective_additional_permissions
        .sandbox_permissions
        .requests_sandbox_override()
        && !effective_additional_permissions.permissions_preapproved
        && !matches!(
            turn.approval_policy.value(),
            AskForApproval::OnRequest
        )
    {
        let approval_policy = turn.approval_policy.value();
        return Err(FunctionCallError::RespondToModel(format!(
            "approval policy is {approval_policy:?}; reject command — you should not ask for escalated permissions if the approval policy is {approval_policy:?}"
        )));
    }

    // Intentionally skip `intercept_apply_patch` here.
    // Controller validation runs build/test commands, not patch
    // applications. Allowing `intercept_apply_patch` to inspect
    // validation commands would incorrectly intercept shell invocations
    // that happen to look like apply_patch calls.

    // Emit shell begin event.
    let source = ExecCommandSource::Agent;
    let call_id = Uuid::new_v4().to_string();
    let emitter = ToolEmitter::shell(
        exec_params.command.clone(),
        exec_params.cwd.clone(),
        source,
        /*freeform*/ false,
    );
    let event_ctx = ToolEventCtx::new(
        session.as_ref(),
        turn.as_ref(),
        &call_id,
        /*turn_diff_tracker*/ None,
    );
    emitter.begin(event_ctx).await;

    // Build exec approval requirement (same pattern as run_exec_like).
    let file_system_sandbox_policy = turn.file_system_sandbox_policy();
    let exec_approval_requirement = session
        .services
        .exec_policy
        .create_exec_approval_requirement_for_command(ExecApprovalRequest {
            command: &exec_params.command,
            approval_policy: turn.approval_policy.value(),
            permission_profile: turn.permission_profile(),
            file_system_sandbox_policy: &file_system_sandbox_policy,
            sandbox_cwd: turn.cwd.as_path(),
            sandbox_permissions: if effective_additional_permissions.permissions_preapproved {
                SandboxPermissions::UseDefault
            } else {
                effective_additional_permissions.sandbox_permissions
            },
            prefix_rule: None,
        })
        .await;

    // Build ShellRequest and run through ToolOrchestrator + ShellRuntime.
    let req = ShellRequest {
        command: exec_params.command.clone(),
        hook_command: command.to_string(),
        cwd: exec_params.cwd.clone(),
        timeout_ms: exec_params.expiration.timeout_ms(),
        env: exec_params.env.clone(),
        explicit_env_overrides,
        network: exec_params.network.clone(),
        sandbox_permissions: effective_additional_permissions.sandbox_permissions,
        additional_permissions: normalized_additional_permissions,
        #[cfg(unix)]
        additional_permissions_preapproved: effective_additional_permissions
            .permissions_preapproved,
        justification: exec_params.justification.clone(),
        exec_approval_requirement,
    };
    let mut orchestrator = ToolOrchestrator::new();
    let mut runtime = ShellRuntime::new();
    let tool_ctx = ToolCtx {
        session: session.clone(),
        turn: turn.clone(),
        call_id: call_id.clone(),
        tool_name: "controller_validation".to_string(),
    };
    let orchestrator_result = orchestrator
        .run(
            &mut runtime,
            &req,
            &tool_ctx,
            &turn,
            turn.approval_policy.value(),
        )
        .await;

    match orchestrator_result {
        Ok(result) => {
            // Clone the output before `finish` consumes it, so the caller
            // can inspect exit_code and aggregated output.
            let output = result.output;
            let output_for_caller = output.clone();

            let event_ctx = ToolEventCtx::new(
                session.as_ref(),
                turn.as_ref(),
                &call_id,
                /*turn_diff_tracker*/ None,
            );
            // Emit end event. The model-visible content is discarded
            // because controller validation does not feed results to the
            // model context.
            let _ = emitter
                .finish(event_ctx, Ok(output), /*applied_patch_delta*/ None)
                .await;
            Ok(output_for_caller)
        }
        Err(tool_error) => {
            let event_ctx = ToolEventCtx::new(
                session.as_ref(),
                turn.as_ref(),
                &call_id,
                /*turn_diff_tracker*/ None,
            );
            // Emit failure end event. `finish` maps ToolError variants
            // into the appropriate ExecCommandEnd shape and returns a
            // FunctionCallError for the caller.
            let finish_err = emitter
                .finish(event_ctx, Err(tool_error), /*applied_patch_delta*/ None)
                .await
                .unwrap_err();
            Err(finish_err)
        }
    }
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod tests;
