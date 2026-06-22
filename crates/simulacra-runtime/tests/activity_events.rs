//! S019 — Activity Events: behavioral tests for types, traits, and sinks.
//!
//! Every test exercises real code paths (constructing values, calling methods,
//! serializing/deserializing). No source-scanning.

use std::sync::Arc;

use simulacra_runtime::{
    ActivitySink, ChannelActivitySink, ForwardingActivitySink, NoopActivitySink,
};
use simulacra_types::ActivityEvent;

// ---------------------------------------------------------------------------
// ActivityEvent type — construction and bounds
// ---------------------------------------------------------------------------

mod activity_event_type {
    use super::*;

    /// Verify every variant can be constructed with owned fields.
    #[test]
    fn all_eleven_variants_are_constructible() {
        let _token = ActivityEvent::Token {
            text: "hello".into(),
        };
        let _think_start = ActivityEvent::ThinkStart;
        let _think_delta = ActivityEvent::ThinkDelta { text: "hmm".into() };
        let _think_end = ActivityEvent::ThinkEnd {
            think_duration_ms: 100,
            think_tokens: 50,
        };
        let _tool_start = ActivityEvent::ToolStart {
            tool_call_id: "tc-1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({"cmd": "ls"}),
        };
        let _tool_output = ActivityEvent::ToolOutput {
            tool_call_id: "tc-1".into(),
            line: "file.txt".into(),
        };
        let _tool_finish = ActivityEvent::ToolFinish {
            tool_call_id: "tc-1".into(),
            name: "shell".into(),
            is_error: false,
            duration_ms: 42,
            exit_code: Some(0),
        };
        let _child_spawned = ActivityEvent::ChildSpawned {
            child_id: "c-1".into(),
            agent_type: "researcher".into(),
            task: "find bugs".into(),
        };
        let _child_activity = ActivityEvent::ChildActivity {
            child_id: "c-1".into(),
            agent_type: "researcher".into(),
            event: Box::new(ActivityEvent::TurnComplete),
        };
        let _child_finished = ActivityEvent::ChildFinished {
            child_id: "c-1".into(),
            agent_type: "researcher".into(),
            exit_reason: "done".into(),
            duration_ms: 1000,
            tool_uses: 5,
            token_count: 2000,
        };
        let _turn_complete = ActivityEvent::TurnComplete;
    }

    /// ActivityEvent must be Clone + Send + 'static (compile-time assertion).
    #[test]
    fn activity_event_is_clone_send_static() {
        fn assert_bounds<T: Clone + Send + 'static>() {}
        assert_bounds::<ActivityEvent>();
    }

    /// ActivityEvent must implement serde Serialize + Deserialize.
    #[test]
    fn activity_event_is_serde_serializable_and_deserializable() {
        fn assert_serde<T: serde::Serialize + serde::de::DeserializeOwned>() {}
        assert_serde::<ActivityEvent>();
    }

    /// JSON roundtrip for a simple variant preserves all fields.
    #[test]
    fn json_roundtrip_token() {
        let event = ActivityEvent::Token {
            text: "hello world".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let restored: ActivityEvent = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&restored).unwrap();
        assert_eq!(json, json2);
        assert!(json.contains("\"text\":\"hello world\""));
    }

    /// JSON roundtrip for ToolStart preserves name, call_id, and arguments.
    #[test]
    fn json_roundtrip_tool_start() {
        let event = ActivityEvent::ToolStart {
            tool_call_id: "tc-42".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"cmd": "echo hi", "timeout": 30}),
        };
        let json = serde_json::to_string(&event).unwrap();
        let restored: ActivityEvent = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&restored).unwrap();
        assert_eq!(json, json2);
    }

    /// JSON roundtrip for ToolFinish preserves is_error, duration, and exit_code.
    #[test]
    fn json_roundtrip_tool_finish() {
        let event = ActivityEvent::ToolFinish {
            tool_call_id: "tc-99".into(),
            name: "read_file".into(),
            is_error: true,
            duration_ms: 123,
            exit_code: Some(1),
        };
        let json = serde_json::to_string(&event).unwrap();
        let restored: ActivityEvent = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&restored).unwrap();
        assert_eq!(json, json2);
        assert!(json.contains("\"is_error\":true"));
        assert!(json.contains("\"exit_code\":1"));
    }

    /// JSON roundtrip for ToolFinish with exit_code = None.
    #[test]
    fn json_roundtrip_tool_finish_no_exit_code() {
        let event = ActivityEvent::ToolFinish {
            tool_call_id: "tc-100".into(),
            name: "mcp_call".into(),
            is_error: false,
            duration_ms: 50,
            exit_code: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let restored: ActivityEvent = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&restored).unwrap();
        assert_eq!(json, json2);
        assert!(json.contains("\"exit_code\":null"));
    }

    /// JSON roundtrip for ThinkEnd preserves duration and token count.
    #[test]
    fn json_roundtrip_think_end() {
        let event = ActivityEvent::ThinkEnd {
            think_duration_ms: 8000,
            think_tokens: 3200,
        };
        let json = serde_json::to_string(&event).unwrap();
        let restored: ActivityEvent = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&restored).unwrap();
        assert_eq!(json, json2);
    }

    /// JSON roundtrip for ChildFinished preserves all aggregated stats.
    #[test]
    fn json_roundtrip_child_finished() {
        let event = ActivityEvent::ChildFinished {
            child_id: "c-7".into(),
            agent_type: "coder".into(),
            exit_reason: "complete".into(),
            duration_ms: 45000,
            tool_uses: 21,
            token_count: 83500,
        };
        let json = serde_json::to_string(&event).unwrap();
        let restored: ActivityEvent = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&restored).unwrap();
        assert_eq!(json, json2);
    }

    /// ChildActivity with Box<ActivityEvent> supports recursive nesting.
    /// A doubly-nested ChildActivity (grandchild) survives JSON roundtrip.
    #[test]
    fn json_roundtrip_doubly_nested_child_activity() {
        let inner = ActivityEvent::ToolStart {
            tool_call_id: "tc-1".into(),
            name: "shell_exec".into(),
            arguments: serde_json::json!({"cmd": "ls"}),
        };
        let wrapped = ActivityEvent::ChildActivity {
            child_id: "child-1".into(),
            agent_type: "researcher".into(),
            event: Box::new(ActivityEvent::ChildActivity {
                child_id: "grandchild-1".into(),
                agent_type: "coder".into(),
                event: Box::new(inner),
            }),
        };
        let json = serde_json::to_string(&wrapped).unwrap();
        let restored: ActivityEvent = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&restored).unwrap();
        assert_eq!(json, json2);
        // Verify nesting structure is present in the JSON
        assert!(json.contains("\"child_id\":\"grandchild-1\""));
        assert!(json.contains("\"child_id\":\"child-1\""));
    }

    /// TurnComplete (unit variant) roundtrips.
    #[test]
    fn json_roundtrip_turn_complete() {
        let event = ActivityEvent::TurnComplete;
        let json = serde_json::to_string(&event).unwrap();
        let restored: ActivityEvent = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&restored).unwrap();
        assert_eq!(json, json2);
    }

    /// ThinkStart (unit variant) roundtrips.
    #[test]
    fn json_roundtrip_think_start() {
        let event = ActivityEvent::ThinkStart;
        let json = serde_json::to_string(&event).unwrap();
        let restored: ActivityEvent = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&restored).unwrap();
        assert_eq!(json, json2);
    }

    /// Clone produces an independent copy that serializes identically.
    #[test]
    fn clone_produces_identical_serialization() {
        let event = ActivityEvent::ChildActivity {
            child_id: "c-1".into(),
            agent_type: "explorer".into(),
            event: Box::new(ActivityEvent::ToolOutput {
                tool_call_id: "tc-5".into(),
                line: "output line".into(),
            }),
        };
        let cloned = event.clone();
        let json_orig = serde_json::to_string(&event).unwrap();
        let json_clone = serde_json::to_string(&cloned).unwrap();
        assert_eq!(json_orig, json_clone);
    }
}

// ---------------------------------------------------------------------------
// ActivitySink trait — object safety, NoopActivitySink, ChannelActivitySink
// ---------------------------------------------------------------------------

mod activity_sink_trait {
    use super::*;

    /// ActivitySink must be object-safe: Box<dyn ActivitySink> compiles.
    #[test]
    fn activity_sink_is_object_safe() {
        let sink: Box<dyn ActivitySink> = Box::new(NoopActivitySink);
        sink.emit(ActivityEvent::TurnComplete);
    }

    /// ActivitySink must work behind Arc<dyn ActivitySink>.
    #[test]
    fn activity_sink_works_behind_arc() {
        let sink: Arc<dyn ActivitySink> = Arc::new(NoopActivitySink);
        sink.emit(ActivityEvent::ThinkStart);
    }

    /// NoopActivitySink discards events without panicking.
    #[test]
    fn noop_activity_sink_discards_events() {
        let sink = NoopActivitySink;
        // Emit every variant — none should panic.
        sink.emit(ActivityEvent::Token { text: "hi".into() });
        sink.emit(ActivityEvent::ThinkStart);
        sink.emit(ActivityEvent::ThinkDelta {
            text: "thinking".into(),
        });
        sink.emit(ActivityEvent::ThinkEnd {
            think_duration_ms: 10,
            think_tokens: 5,
        });
        sink.emit(ActivityEvent::ToolStart {
            tool_call_id: "tc".into(),
            name: "t".into(),
            arguments: serde_json::Value::Null,
        });
        sink.emit(ActivityEvent::ToolOutput {
            tool_call_id: "tc".into(),
            line: "x".into(),
        });
        sink.emit(ActivityEvent::ToolFinish {
            tool_call_id: "tc".into(),
            name: "t".into(),
            is_error: false,
            duration_ms: 1,
            exit_code: None,
        });
        sink.emit(ActivityEvent::TurnComplete);
    }

    /// ChannelActivitySink delivers events to the receiver.
    #[test]
    fn channel_activity_sink_delivers_events() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ChannelActivitySink::new(tx);

        sink.emit(ActivityEvent::Token {
            text: "hello".into(),
        });
        sink.emit(ActivityEvent::TurnComplete);

        // Should receive both events in order.
        let e1 = rx.try_recv().expect("should receive first event");
        let e2 = rx.try_recv().expect("should receive second event");

        // Verify via serialization since ActivityEvent doesn't derive PartialEq.
        let j1 = serde_json::to_string(&e1).unwrap();
        let j2 = serde_json::to_string(&e2).unwrap();
        assert!(j1.contains("\"text\":\"hello\""));
        assert!(j2.contains("\"type\":\"TurnComplete\""));
    }

    /// ChannelActivitySink does not panic when the receiver is dropped.
    #[test]
    fn channel_activity_sink_silent_on_dropped_receiver() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ChannelActivitySink::new(tx);
        drop(rx);
        // Should not panic — send failure is silently ignored.
        sink.emit(ActivityEvent::TurnComplete);
    }

    /// ChannelActivitySink preserves event ordering.
    #[test]
    fn channel_activity_sink_preserves_ordering() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ChannelActivitySink::new(tx);

        sink.emit(ActivityEvent::ToolStart {
            tool_call_id: "tc-1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({}),
        });
        sink.emit(ActivityEvent::ToolOutput {
            tool_call_id: "tc-1".into(),
            line: "line 1".into(),
        });
        sink.emit(ActivityEvent::ToolFinish {
            tool_call_id: "tc-1".into(),
            name: "bash".into(),
            is_error: false,
            duration_ms: 100,
            exit_code: Some(0),
        });

        let j1 = serde_json::to_string(&rx.try_recv().unwrap()).unwrap();
        let j2 = serde_json::to_string(&rx.try_recv().unwrap()).unwrap();
        let j3 = serde_json::to_string(&rx.try_recv().unwrap()).unwrap();

        assert!(j1.contains("\"type\":\"ToolStart\""));
        assert!(j2.contains("\"type\":\"ToolOutput\""));
        assert!(j3.contains("\"type\":\"ToolFinish\""));
    }

    /// ForwardingActivitySink wraps events in ChildActivity.
    #[test]
    fn forwarding_activity_sink_wraps_in_child_activity() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let parent_sink: Arc<dyn ActivitySink> = Arc::new(ChannelActivitySink::new(tx));

        let forwarding =
            ForwardingActivitySink::new("child-42".into(), "researcher".into(), parent_sink);

        forwarding.emit(ActivityEvent::Token {
            text: "from child".into(),
        });

        let received = rx.try_recv().expect("should receive wrapped event");
        let json = serde_json::to_string(&received).unwrap();
        assert!(json.contains("\"type\":\"ChildActivity\""));
        assert!(json.contains("\"child_id\":\"child-42\""));
        assert!(json.contains("\"agent_type\":\"researcher\""));
        assert!(json.contains("\"from child\""));
    }

    /// ForwardingActivitySink nests correctly for grandchildren
    /// (double wrapping via two forwarding sinks).
    #[test]
    fn forwarding_sink_double_nesting_for_grandchild() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let root_sink: Arc<dyn ActivitySink> = Arc::new(ChannelActivitySink::new(tx));

        let child_sink: Arc<dyn ActivitySink> = Arc::new(ForwardingActivitySink::new(
            "child-1".into(),
            "researcher".into(),
            root_sink,
        ));

        let grandchild_sink =
            ForwardingActivitySink::new("grandchild-1".into(), "coder".into(), child_sink);

        grandchild_sink.emit(ActivityEvent::TurnComplete);

        let received = rx.try_recv().expect("should receive doubly-wrapped event");
        let json = serde_json::to_string(&received).unwrap();

        // Outer wrapping from child
        assert!(json.contains("\"child_id\":\"child-1\""));
        // Inner wrapping from grandchild
        assert!(json.contains("\"child_id\":\"grandchild-1\""));
        // The innermost event
        assert!(json.contains("\"type\":\"TurnComplete\""));
    }
}
