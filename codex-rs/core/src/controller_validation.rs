use codex_protocol::user_input::UserInput;

const BUILD_TEST_COMMANDS_HEADING: &str = "Build/test commands:";
const SUMMARY_CHECKLIST_HEADING: &str = "Summary checklist:";
const VALIDATION_NOTE: &str = "Validation is controller-managed.";
const SILENT_FINAL_INSTRUCTION: &str = "Controller-managed validation is pending. Make the required edits only. Do not output a final answer, summary, checklist, or validation report. When edits are complete, end the turn silently.";

const MAX_FAILURE_OUTPUT_CHARS: usize = 8192;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ControllerValidationState {
    pub(crate) commands: Vec<String>,
    pub(crate) attempt: u8,
}

impl ControllerValidationState {
    pub(crate) fn commands(&self) -> &[String] {
        &self.commands
    }

    pub(crate) fn attempt(&self) -> u8 {
        self.attempt
    }

    pub(crate) fn increment_attempt(&mut self) {
        self.attempt = self.attempt.saturating_add(1);
    }

    pub(crate) fn has_attempts_remaining(&self, max_attempts: u8) -> bool {
        self.attempt < max_attempts
    }

    pub(crate) fn failed_command_first(&mut self, failed_command: &str) {
        if let Some(pos) = self.commands.iter().position(|c| c == failed_command) {
            let cmd = self.commands.remove(pos);
            self.commands.insert(0, cmd);
        }
    }
}

pub(crate) fn validation_success_message() -> &'static str {
    "All checks passed."
}

pub(crate) fn compact_validation_failure_summary(
    command: &str,
    exit_code: i32,
    output: &str,
    max_lines: usize,
) -> String {
    let output_lines: Vec<&str> = output.lines().collect();
    let (mut capped_output, was_line_truncated) = if output_lines.len() > max_lines {
        let start = output_lines.len() - max_lines;
        (output_lines[start..].join("\n"), true)
    } else {
        (output.to_string(), false)
    };

    let was_char_truncated = if capped_output.len() > MAX_FAILURE_OUTPUT_CHARS {
        let mut start_index = capped_output.len() - MAX_FAILURE_OUTPUT_CHARS;
        // UTF-8 safe truncation: move to the next valid char boundary.
        while start_index < capped_output.len() && !capped_output.is_char_boundary(start_index) {
            start_index += 1;
        }
        capped_output = capped_output[start_index..].to_string();
        true
    } else {
        false
    };

    let prefix = if was_line_truncated || was_char_truncated {
        "...\n"
    } else {
        ""
    };

    format!(
        "Validation failed for command: `{}`\nExit code: {}\nOutput:\n{}{}",
        command, exit_code, prefix, capped_output
    )
}

pub(crate) fn build_validation_repair_prompt(failure_summary: &str) -> String {
    format!(
        "{}\n\nFix this validation failure only. Do not output a final summary. Validation remains controller-managed.",
        failure_summary
    )
}

pub(crate) struct ControllerValidationTransform {
    pub model_visible_text: String,
    pub commands: Vec<String>,
    pub validation_pending: bool,
}

pub(crate) fn transform_user_inputs_for_controller_validation(
    items: Vec<UserInput>,
) -> (Vec<UserInput>, Option<ControllerValidationState>) {
    let mut controller_validation = None;
    let transformed_items = items
        .into_iter()
        .map(|item| {
            if controller_validation.is_some() {
                return item;
            }
            let UserInput::Text {
                text,
                text_elements,
            } = item
            else {
                return item;
            };
            let transform = transform_packet_for_controller_validation(&text);
            if !transform.validation_pending {
                return UserInput::Text {
                    text,
                    text_elements,
                };
            }
            controller_validation = Some(ControllerValidationState {
                commands: transform.commands,
                attempt: 0,
            });
            UserInput::Text {
                text: transform.model_visible_text,
                // Byte ranges no longer line up after prompt rewrite.
                text_elements: Vec::new(),
            }
        })
        .collect();
    (transformed_items, controller_validation)
}

pub(crate) fn transform_packet_for_controller_validation(
    text: &str,
) -> ControllerValidationTransform {
    let Some(parsed) = parse_controller_validation_packet(text) else {
        return ControllerValidationTransform {
            model_visible_text: text.to_owned(),
            commands: Vec::new(),
            validation_pending: false,
        };
    };
    if parsed.commands.is_empty() {
        return ControllerValidationTransform {
            model_visible_text: text.to_owned(),
            commands: parsed.commands,
            validation_pending: false,
        };
    }
    let mut model_visible_lines = Vec::new();
    let line_count = text.split('\n').count();
    for (line_index, line) in text.split('\n').enumerate() {
        if line_index == parsed.build_start {
            model_visible_lines.push(BUILD_TEST_COMMANDS_HEADING.to_owned());
            model_visible_lines.push(VALIDATION_NOTE.to_owned());
            model_visible_lines.push(SILENT_FINAL_INSTRUCTION.to_owned());
            continue;
        }
        if line_index > parsed.build_start && line_index < parsed.build_end.unwrap_or(line_count) {
            continue;
        }
        if let Some(summary_start) = parsed.summary_start {
            let summary_end = parsed.summary_end.unwrap_or(line_count);
            if line_index >= summary_start && line_index < summary_end {
                continue;
            }
        }
        model_visible_lines.push(line.to_owned());
    }
    ControllerValidationTransform {
        model_visible_text: model_visible_lines.join("\n"),
        commands: parsed.commands,
        validation_pending: true,
    }
}

fn parse_controller_validation_packet(text: &str) -> Option<ControllerValidationPacket> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut build_start = None;
    let mut summary_start = None;
    let mut summary_end = None;
    let mut commands = Vec::new();
    let mut in_fence = false;
    for (line_index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if build_start.is_none() {
            if trimmed == BUILD_TEST_COMMANDS_HEADING {
                build_start = Some(line_index);
            }
            continue;
        }
        if summary_start.is_none() && trimmed == SUMMARY_CHECKLIST_HEADING {
            summary_start = Some(line_index);
            continue;
        }
        if summary_start.is_some() {
            if trimmed.is_empty() || is_checklist_line(trimmed) {
                continue;
            }
            if trimmed.starts_with("```") {
                continue;
            }
            summary_end = Some(line_index);
            break;
        }
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if in_fence {
            commands.push(trimmed.to_owned());
            continue;
        }
        if let Some(command) = trimmed.strip_prefix("- ") {
            let command = command.trim();
            if !command.is_empty() {
                commands.push(command.to_owned());
            }
            continue;
        }
        if let Some(command) = trimmed.strip_prefix("* ") {
            let command = command.trim();
            if !command.is_empty() {
                commands.push(command.to_owned());
            }
            continue;
        }
        commands.push(trimmed.to_owned());
    }
    build_start.map(|build_start| ControllerValidationPacket {
        build_start,
        build_end: summary_start.or(Some(lines.len())),
        summary_start,
        summary_end,
        commands,
    })
}

fn is_checklist_line(trimmed: &str) -> bool {
    trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
        || trimmed.starts_with("- [ ]")
        || trimmed.starts_with("- [x]")
        || trimmed.starts_with("- [X]")
}

struct ControllerValidationPacket {
    build_start: usize,
    build_end: Option<usize>,
    summary_start: Option<usize>,
    summary_end: Option<usize>,
    commands: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::user_input::UserInput;
    use pretty_assertions::assert_eq;

    #[test]
    fn missing_build_test_section_is_noop() {
        let text = "Hello\nSummary checklist:\n- done";
        let transform = transform_packet_for_controller_validation(text);
        assert_eq!(transform.model_visible_text, text);
        assert_eq!(transform.commands, Vec::<String>::new());
        assert!(!transform.validation_pending);
    }

    #[test]
    fn empty_build_test_section_is_noop() {
        let text = "Build/test commands:\n\nSummary checklist:\n- done";
        let transform = transform_packet_for_controller_validation(text);
        assert_eq!(transform.model_visible_text, text);
        assert_eq!(transform.commands, Vec::<String>::new());
        assert!(!transform.validation_pending);
    }

    #[test]
    fn bullet_commands_are_extracted_in_order() {
        let text = "Build/test commands:\n- cargo check -p codex-core\n- cargo test -p codex-core\nSummary checklist:\n- done";
        let transform = transform_packet_for_controller_validation(text);
        assert_eq!(
            transform.commands,
            vec![
                "cargo check -p codex-core".to_owned(),
                "cargo test -p codex-core".to_owned(),
            ]
        );
        assert!(transform.validation_pending);
        assert!(
            transform
                .model_visible_text
                .contains("Validation is controller-managed.")
        );
        assert!(
            transform
                .model_visible_text
                .contains("Controller-managed validation is pending.")
        );
        assert!(
            !transform
                .model_visible_text
                .contains("cargo check -p codex-core")
        );
        assert!(
            !transform
                .model_visible_text
                .contains("cargo test -p codex-core")
        );
        assert!(!transform.model_visible_text.contains("Summary checklist:"));
        assert!(!transform.model_visible_text.contains("- done"));
    }

    #[test]
    fn fenced_commands_are_extracted_in_order() {
        let text = "Build/test commands:\n```sh\ncargo check -p codex-core\ncargo test -p codex-core\n```\nSummary checklist:\n- done";
        let transform = transform_packet_for_controller_validation(text);
        assert_eq!(
            transform.commands,
            vec![
                "cargo check -p codex-core".to_owned(),
                "cargo test -p codex-core".to_owned(),
            ]
        );
        assert!(transform.validation_pending);
    }

    #[test]
    fn section_stops_before_summary_checklist() {
        let text = "Build/test commands:\n- cargo check -p codex-core\nSummary checklist:\n- make sure\n- verify output";
        let transform = transform_packet_for_controller_validation(text);
        assert_eq!(
            transform.commands,
            vec!["cargo check -p codex-core".to_owned()]
        );
        assert!(!transform.model_visible_text.contains("make sure"));
        assert!(!transform.model_visible_text.contains("verify output"));
    }

    #[test]
    fn transformed_prompt_hides_command_list_and_checklist_items() {
        let text = "Before\nBuild/test commands:\n- cargo check -p codex-core\n- cargo test -p codex-core\nSummary checklist:\n- item one\n- item two\nAfter";
        let transform = transform_packet_for_controller_validation(text);
        assert!(transform.model_visible_text.contains("Before"));
        assert!(transform.model_visible_text.contains("After"));
        assert!(
            transform
                .model_visible_text
                .contains("Validation is controller-managed.")
        );
        assert!(
            transform
                .model_visible_text
                .contains("Controller-managed validation is pending.")
        );
        assert!(
            !transform
                .model_visible_text
                .contains("cargo check -p codex-core")
        );
        assert!(
            !transform
                .model_visible_text
                .contains("cargo test -p codex-core")
        );
        assert!(!transform.model_visible_text.contains("Summary checklist:"));
        assert!(!transform.model_visible_text.contains("item one"));
        assert!(!transform.model_visible_text.contains("item two"));
    }

    #[test]
    fn transforms_only_first_text_item_with_valid_commands() {
        let items = vec![
            UserInput::Text {
                text: "Before\nBuild/test commands:\n- cargo check -p codex-core\nSummary checklist:\n- done"
                    .to_string(),
                text_elements: vec![codex_protocol::user_input::TextElement::new(
                    codex_protocol::user_input::ByteRange { start: 0, end: 6 },
                    Some("Before".to_string()),
                )],
            },
            UserInput::Image {
                image_url: "data:image/png;base64,abc".to_string(),
            },
            UserInput::Text {
                text: "Build/test commands:\n- cargo test -p codex-core\nSummary checklist:\n- done"
                    .to_string(),
                text_elements: Vec::new(),
            },
        ];

        let (transformed_items, controller_validation) =
            transform_user_inputs_for_controller_validation(items);

        assert_eq!(
            transformed_items,
            vec![
                UserInput::Text {
                    text: "Before\nBuild/test commands:\nValidation is controller-managed.\nController-managed validation is pending. Make the required edits only. Do not output a final answer, summary, checklist, or validation report. When edits are complete, end the turn silently."
                        .to_string(),
                    text_elements: Vec::new(),
                },
                UserInput::Image {
                    image_url: "data:image/png;base64,abc".to_string(),
                },
                UserInput::Text {
                    text: "Build/test commands:\n- cargo test -p codex-core\nSummary checklist:\n- done"
                        .to_string(),
                    text_elements: Vec::new(),
                },
            ]
        );
        assert_eq!(
            controller_validation,
            Some(ControllerValidationState {
                commands: vec!["cargo check -p codex-core".to_string()],
                attempt: 0,
            })
        );
    }

    #[test]
    fn no_valid_section_leaves_inputs_unchanged() {
        let items = vec![
            UserInput::Text {
                text: "Hello".to_string(),
                text_elements: Vec::new(),
            },
            UserInput::Skill {
                name: "skill".to_string(),
                path: std::path::PathBuf::from("/tmp/SKILL.md"),
            },
        ];

        let (transformed_items, controller_validation) =
            transform_user_inputs_for_controller_validation(items.clone());

        assert_eq!(transformed_items, items);
        assert_eq!(controller_validation, None);
    }

    #[test]
    fn silent_final_instruction_is_only_present_when_pending() {
        let pending = transform_packet_for_controller_validation(
            "Build/test commands:\n- cargo check -p codex-core\nSummary checklist:\n- done",
        );
        assert!(pending.validation_pending);
        assert!(
            pending
                .model_visible_text
                .contains("Controller-managed validation is pending.")
        );

        let noop = transform_packet_for_controller_validation("Hello");
        assert!(!noop.validation_pending);
        assert!(
            !noop
                .model_visible_text
                .contains("Controller-managed validation is pending.")
        );
    }

    #[test]
    fn state_helpers() {
        let mut state = ControllerValidationState {
            commands: vec!["c1".to_string(), "c2".to_string()],
            attempt: 0,
        };

        assert_eq!(state.commands(), &["c1", "c2"]);
        assert_eq!(state.attempt(), 0);
        assert!(state.has_attempts_remaining(2));

        state.increment_attempt();
        assert_eq!(state.attempt(), 1);
        assert!(state.has_attempts_remaining(2));

        state.increment_attempt();
        assert_eq!(state.attempt(), 2);
        assert!(!state.has_attempts_remaining(2));

        // Test saturating increment
        let mut max_state = ControllerValidationState {
            commands: vec![],
            attempt: u8::MAX,
        };
        max_state.increment_attempt();
        assert_eq!(max_state.attempt(), u8::MAX);
    }

    #[test]
    fn failed_command_first() {
        let mut state = ControllerValidationState {
            commands: vec!["c1".to_string(), "c2".to_string(), "c3".to_string()],
            attempt: 0,
        };

        state.failed_command_first("c2");
        assert_eq!(state.commands(), &["c2", "c1", "c3"]);

        state.failed_command_first("nonexistent");
        assert_eq!(state.commands(), &["c2", "c1", "c3"]);
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(validation_success_message(), "All checks passed.");

        let summary = compact_validation_failure_summary("cmd", 1, "out", 10);
        assert!(summary.contains("`cmd`"));
        assert!(summary.contains("Exit code: 1"));
        assert!(summary.contains("out"));

        let capped_lines = compact_validation_failure_summary("cmd", 1, "l1\nl2\nl3", 2);
        assert!(capped_lines.contains("...\nl2\nl3"));
        assert!(!capped_lines.contains("l1\nl2\nl3"));

        // Test huge single line capping
        let huge_line = "a".repeat(MAX_FAILURE_OUTPUT_CHARS + 100);
        let capped_huge = compact_validation_failure_summary("cmd", 1, &huge_line, 10);
        assert!(capped_huge.contains("...\n"));
        // It should contain the last characters of the huge line.
        let output_start = capped_huge.find("Output:\n").unwrap() + 8;
        let output_content = &capped_huge[output_start..];
        assert!(output_content.starts_with("...\n"));
        assert_eq!(output_content.len() - 4, MAX_FAILURE_OUTPUT_CHARS);

        // Test UTF-8 safe truncation
        let multi_byte = "🚀".repeat(MAX_FAILURE_OUTPUT_CHARS);
        let capped_utf8 = compact_validation_failure_summary("cmd", 1, &multi_byte, 10);
        assert!(capped_utf8.contains("...\n"));
        // Ensure no panic and output is valid UTF-8.
        let output_start_utf8 = capped_utf8.find("Output:\n").unwrap() + 8;
        let output_content_utf8 = &capped_utf8[output_start_utf8..];
        assert!(output_content_utf8.starts_with("...\n"));

        let repair = build_validation_repair_prompt("some failure");
        assert!(repair.contains("some failure"));
        assert!(repair.contains("Fix this validation failure only"));
        assert!(repair.contains("Do not output a final summary"));
    }
}
