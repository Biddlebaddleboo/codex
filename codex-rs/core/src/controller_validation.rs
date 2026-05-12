use codex_protocol::user_input::UserInput;

const BUILD_TEST_COMMANDS_HEADING: &str = "Build/test commands:";
const SUMMARY_CHECKLIST_HEADING: &str = "Summary checklist:";
const VALIDATION_NOTE: &str = "Validation is controller-managed.";
const SILENT_FINAL_INSTRUCTION: &str = "Controller-managed validation is pending. Make the required edits only. Do not output a final answer, summary, checklist, or validation report. When edits are complete, end the turn silently.";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ControllerValidationState {
    pub(crate) commands: Vec<String>,
    pub(crate) attempt: u8,
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
    let Some(parsed) = parse_controller_validation_packet(text)
    else {
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
        if line_index > parsed.build_start
            && line_index < parsed.build_end.unwrap_or(line_count)
        {
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
    use pretty_assertions::assert_eq;
    use codex_protocol::user_input::UserInput;

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
                    text: "Before\nBuild/test commands:\nValidation is controller-managed.\nController-managed validation is pending.\nSummary checklist:\n- done"
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
}
