use serde_json::json;
use simulacra_cli::activity_blocks::ActivityBlockRenderer;
use simulacra_types::ActivityEvent;

mod activity_block_rendering {
    use super::*;

    #[test]
    fn tool_start_opens_a_block_with_spinner_tool_name_and_argument_summary() {
        let mut renderer = ActivityBlockRenderer::new();
        let event = ActivityEvent::ToolStart {
            tool_call_id: "tc-1".into(),
            name: "Bash".into(),
            arguments: json!({"command": "cargo test"}),
        };
        let lines = renderer.process_event(&event);
        assert!(!lines.is_empty(), "ToolStart should produce output lines");
        let header = &lines[0];
        assert!(
            header.contains("Bash"),
            "Header should contain tool name 'Bash', got: {header}"
        );
        assert!(
            header.contains("cargo test"),
            "Header should contain argument summary, got: {header}"
        );
        // The header should start with a spinner character (braille unicode)
        let first_char = header.chars().next().unwrap();
        assert!(
            ('\u{2800}'..='\u{28FF}').contains(&first_char),
            "Header should start with a braille spinner char, got: {first_char}"
        );
    }

    #[test]
    fn tool_output_lines_are_rendered_in_a_last_three_lines_tail_window() {
        let mut renderer = ActivityBlockRenderer::new();
        renderer.process_event(&ActivityEvent::ToolStart {
            tool_call_id: "tc-1".into(),
            name: "Bash".into(),
            arguments: json!({}),
        });

        // Send 5 output lines; only the last 3 should appear in the tail window
        for i in 1..=5 {
            renderer.process_event(&ActivityEvent::ToolOutput {
                tool_call_id: "tc-1".into(),
                line: format!("line {i}"),
            });
        }

        let lines = renderer.process_event(&ActivityEvent::ToolOutput {
            tool_call_id: "tc-1".into(),
            line: "line 6".into(),
        });

        // The tail window should contain only the last 3 lines
        let tail_content = lines.join("\n");
        assert!(
            tail_content.contains("line 4")
                || tail_content.contains("line 5")
                || tail_content.contains("line 6"),
            "Tail window should contain recent lines, got: {tail_content}"
        );
        assert!(
            !tail_content.contains("line 1"),
            "Tail window should not contain old lines that scrolled out, got: {tail_content}"
        );
    }

    #[test]
    fn output_beyond_the_tail_window_shows_and_updates_a_hidden_line_counter() {
        let mut renderer = ActivityBlockRenderer::new();
        renderer.process_event(&ActivityEvent::ToolStart {
            tool_call_id: "tc-1".into(),
            name: "Bash".into(),
            arguments: json!({}),
        });

        // Send 5 lines to exceed the 3-line tail window
        for i in 1..=5 {
            renderer.process_event(&ActivityEvent::ToolOutput {
                tool_call_id: "tc-1".into(),
                line: format!("line {i}"),
            });
        }

        let lines = renderer.process_event(&ActivityEvent::ToolOutput {
            tool_call_id: "tc-1".into(),
            line: "line 6".into(),
        });

        let output = lines.join("\n");
        assert!(
            output.contains("... +") && output.contains("lines"),
            "Should show hidden line counter when output exceeds tail window, got: {output}"
        );
    }

    #[test]
    fn tool_finish_transitions_the_block_to_a_static_completed_summary() {
        let mut renderer = ActivityBlockRenderer::new();
        renderer.process_event(&ActivityEvent::ToolStart {
            tool_call_id: "tc-1".into(),
            name: "Bash".into(),
            arguments: json!({"command": "cargo test"}),
        });

        let lines = renderer.process_event(&ActivityEvent::ToolFinish {
            tool_call_id: "tc-1".into(),
            name: "Bash".into(),
            is_error: false,
            duration_ms: 1234,
            exit_code: Some(0),
        });

        let output = lines.join("\n");
        assert!(
            output.contains("Done"),
            "Successful tool finish should show 'Done', got: {output}"
        );
        assert!(
            output.contains("1234"),
            "Tool finish should show duration, got: {output}"
        );
        // Success indicator: filled dot
        assert!(
            output.contains('●'),
            "Successful tool should show filled dot indicator, got: {output}"
        );
    }

    #[test]
    fn tool_finish_error_shows_exit_code_and_error_summary() {
        let mut renderer = ActivityBlockRenderer::new();
        renderer.process_event(&ActivityEvent::ToolStart {
            tool_call_id: "tc-1".into(),
            name: "Bash".into(),
            arguments: json!({}),
        });

        let lines = renderer.process_event(&ActivityEvent::ToolFinish {
            tool_call_id: "tc-1".into(),
            name: "Bash".into(),
            is_error: true,
            duration_ms: 500,
            exit_code: Some(1),
        });

        let output = lines.join("\n");
        assert!(
            output.contains("Error"),
            "Error tool finish should show 'Error', got: {output}"
        );
        assert!(
            output.contains("exit code 1"),
            "Error tool finish should show exit code, got: {output}"
        );
    }

    #[test]
    fn child_spawned_opens_a_child_activity_block_with_agent_type_and_task_summary() {
        let mut renderer = ActivityBlockRenderer::new();
        let lines = renderer.process_event(&ActivityEvent::ChildSpawned {
            child_id: "child-1".into(),
            agent_type: "Explore".into(),
            task: "Research streaming architecture".into(),
        });

        assert!(!lines.is_empty(), "ChildSpawned should produce output");
        let header = &lines[0];
        assert!(
            header.contains("Explore"),
            "Header should contain agent_type, got: {header}"
        );
        assert!(
            header.contains("Research streaming architecture"),
            "Header should contain task summary, got: {header}"
        );
    }

    #[test]
    fn child_activity_events_render_recursively_inside_the_child_block() {
        let mut renderer = ActivityBlockRenderer::new();
        renderer.process_event(&ActivityEvent::ChildSpawned {
            child_id: "child-1".into(),
            agent_type: "Explore".into(),
            task: "some task".into(),
        });

        // A ChildActivity wrapping a ToolStart
        let lines = renderer.process_event(&ActivityEvent::ChildActivity {
            child_id: "child-1".into(),
            agent_type: "Explore".into(),
            event: Box::new(ActivityEvent::ToolStart {
                tool_call_id: "tc-child-1".into(),
                name: "Read".into(),
                arguments: json!({"path": "/foo"}),
            }),
        });

        assert!(
            !lines.is_empty(),
            "ChildActivity should produce rendered output from inner event"
        );
        let output = lines.join("\n");
        assert!(
            output.contains("Read"),
            "Inner ToolStart should render tool name, got: {output}"
        );
    }

    #[test]
    fn child_finished_transitions_the_child_block_to_a_stats_summary() {
        let mut renderer = ActivityBlockRenderer::new();
        renderer.process_event(&ActivityEvent::ChildSpawned {
            child_id: "child-1".into(),
            agent_type: "Explore".into(),
            task: "some task".into(),
        });

        let lines = renderer.process_event(&ActivityEvent::ChildFinished {
            child_id: "child-1".into(),
            agent_type: "Explore".into(),
            exit_reason: "complete".into(),
            duration_ms: 46000,
            tool_uses: 21,
            token_count: 83500,
        });

        let output = lines.join("\n");
        assert!(
            output.contains("21 tool uses"),
            "ChildFinished should show tool uses, got: {output}"
        );
        assert!(
            output.contains("83500 tokens"),
            "ChildFinished should show token count, got: {output}"
        );
        assert!(
            output.contains("46000"),
            "ChildFinished should show duration, got: {output}"
        );
    }

    #[test]
    fn think_start_opens_a_single_line_thinking_block() {
        let mut renderer = ActivityBlockRenderer::new();
        let lines = renderer.process_event(&ActivityEvent::ThinkStart);

        assert_eq!(lines.len(), 1, "ThinkStart should produce exactly one line");
        assert!(
            lines[0].contains("Thinking..."),
            "ThinkStart line should contain 'Thinking...', got: {}",
            lines[0]
        );
        assert!(
            lines[0].starts_with('+'),
            "ThinkStart should start with thinking indicator '+', got: {}",
            lines[0]
        );
    }

    #[test]
    fn think_end_finalizes_the_thinking_line_to_a_static_summary() {
        let mut renderer = ActivityBlockRenderer::new();
        renderer.process_event(&ActivityEvent::ThinkStart);

        let lines = renderer.process_event(&ActivityEvent::ThinkEnd {
            think_duration_ms: 8000,
            think_tokens: 3200,
        });

        assert!(!lines.is_empty(), "ThinkEnd should produce output");
        let output = lines.join("\n");
        assert!(
            output.contains("thought for"),
            "ThinkEnd should show 'thought for', got: {output}"
        );
        assert!(
            output.contains("3200"),
            "ThinkEnd should show token count, got: {output}"
        );
        assert!(
            output.contains("8000"),
            "ThinkEnd should show think duration, got: {output}"
        );
    }

    #[test]
    fn think_delta_payloads_are_not_rendered_to_the_terminal() {
        let mut renderer = ActivityBlockRenderer::new();
        renderer.process_event(&ActivityEvent::ThinkStart);

        let lines = renderer.process_event(&ActivityEvent::ThinkDelta {
            text: "Let me analyze this problem step by step...".into(),
        });

        assert!(
            lines.is_empty(),
            "ThinkDelta should produce zero rendered lines, got: {lines:?}"
        );
    }

    #[test]
    fn in_progress_activity_blocks_keep_animating_a_spinner() {
        let mut renderer = ActivityBlockRenderer::new();

        // Open two tool blocks and verify spinner chars differ (animation cycles)
        let lines1 = renderer.process_event(&ActivityEvent::ToolStart {
            tool_call_id: "tc-1".into(),
            name: "Bash".into(),
            arguments: json!({}),
        });
        let lines2 = renderer.process_event(&ActivityEvent::ToolStart {
            tool_call_id: "tc-2".into(),
            name: "Read".into(),
            arguments: json!({}),
        });

        let char1 = lines1[0].chars().next().unwrap();
        let char2 = lines2[0].chars().next().unwrap();
        // Both should be braille spinners
        assert!(('\u{2800}'..='\u{28FF}').contains(&char1));
        assert!(('\u{2800}'..='\u{28FF}').contains(&char2));
        // They should be different frames (spinner advances)
        assert_ne!(
            char1, char2,
            "Spinner should animate between consecutive blocks"
        );
    }

    #[test]
    fn erroring_tool_blocks_render_an_x_indicator_instead_of_a_success_dot() {
        let mut renderer = ActivityBlockRenderer::new();
        renderer.process_event(&ActivityEvent::ToolStart {
            tool_call_id: "tc-err".into(),
            name: "Bash".into(),
            arguments: json!({}),
        });

        let lines = renderer.process_event(&ActivityEvent::ToolFinish {
            tool_call_id: "tc-err".into(),
            name: "Bash".into(),
            is_error: true,
            duration_ms: 100,
            exit_code: Some(127),
        });

        let output = lines.join("\n");
        assert!(
            output.contains('✗'),
            "Error tool should show X (✗) indicator, got: {output}"
        );
        assert!(
            !output.contains('●'),
            "Error tool should NOT show success dot, got: {output}"
        );
    }
}
