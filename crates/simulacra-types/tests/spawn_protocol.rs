use serde_json::{Value, json};
use simulacra_types::{ActivityEvent, JOURNAL_SCHEMA_VERSION, JournalEntry};

#[test]
fn s060_a33_child_activity_protocol_uses_only_placement_vocabulary() {
    let cases = [
        json!({
            "type": "ChildSpawned",
            "child_id": "child-0123456789abcdef0123456789abcdef",
            "placement": "workspace",
            "task": "  preserve task bytes \n"
        }),
        json!({
            "type": "ChildActivity",
            "child_id": "child-0123456789abcdef0123456789abcdef",
            "placement": "workspace",
            "event": { "type": "TurnComplete" }
        }),
        json!({
            "type": "ChildFinished",
            "child_id": "child-0123456789abcdef0123456789abcdef",
            "placement": "workspace",
            "exit_reason": "complete",
            "duration_ms": 17,
            "tool_uses": 2,
            "token_count": 41
        }),
    ];

    for expected in cases {
        let event: ActivityEvent = serde_json::from_value(expected.clone())
            .expect("S060 child activity shape should deserialize");
        let encoded = serde_json::to_value(event).expect("activity event should serialize");
        assert_eq!(encoded, expected);
        assert!(encoded.get("agent_type").is_none());
        assert!(encoded.get("child_agent_type").is_none());
    }
}

#[test]
fn s060_a34_journal_schema_version_is_exactly_three() {
    assert_eq!(JOURNAL_SCHEMA_VERSION, 3);
}

#[test]
fn s060_a34_v3_rejects_the_old_sub_agent_spawned_payload_instead_of_inferring_it() {
    let old_v2_payload = json!({
        "schema_version": 3,
        "agent_id": "parent-agent",
        "timestamp_ms": 7,
        "entry": {
            "type": "SubAgentSpawned",
            "child_id": "child-0123456789abcdef0123456789abcdef",
            "agent_type": "coder",
            "system_prompt": "legacy role prompt"
        }
    });

    let error = serde_json::from_value::<JournalEntry>(old_v2_payload)
        .expect_err("a v3 envelope must not decode the v2 SubAgentSpawned shape");
    let message = error.to_string();
    assert!(
        message.contains("placement")
            || message.contains("backend")
            || message.contains("task")
            || message.contains("unknown field"),
        "malformed v3 payload should fail for its old spawn fields: {message}"
    );
}

#[test]
fn s060_a15_a35_v3_sub_agent_spawned_roundtrips_exact_effective_values() {
    let instructions = "  use the implementation skill\nkeep exact whitespace  ";
    let task = "  inspect only the requested slice \n";
    let expected = json!({
        "schema_version": 3,
        "agent_id": "parent-agent",
        "timestamp_ms": 11,
        "entry": {
            "type": "SubAgentSpawned",
            "child_id": "child-0123456789abcdef0123456789abcdef",
            "placement": "workspace",
            "backend": "acp",
            "task": task,
            "instructions": instructions
        }
    });

    let entry: JournalEntry = serde_json::from_value(expected.clone())
        .expect("the exact S060 SubAgentSpawned shape should deserialize");
    let encoded = serde_json::to_value(entry).expect("journal entry should serialize");
    assert_eq!(encoded, expected);

    let fields = encoded["entry"]
        .as_object()
        .expect("SubAgentSpawned payload should be an object")
        .keys()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        fields,
        [
            "backend",
            "child_id",
            "instructions",
            "placement",
            "task",
            "type",
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(encoded["entry"]["instructions"], Value::from(instructions));
    assert_eq!(encoded["entry"]["task"], Value::from(task));
}

#[test]
fn s060_a34_v3_rejects_hybrid_sub_agent_spawned_payload_with_legacy_fields() {
    let hybrid = json!({
        "schema_version": 3,
        "agent_id": "parent-agent",
        "timestamp_ms": 12,
        "entry": {
            "type": "SubAgentSpawned",
            "child_id": "child-0123456789abcdef0123456789abcdef",
            "placement": "workspace",
            "backend": "acp",
            "task": "bounded task",
            "instructions": "shape independently",
            "agent_type": "coder",
            "system_prompt": "legacy role prompt"
        }
    });

    let error = serde_json::from_value::<JournalEntry>(hybrid)
        .expect_err("legacy fields must not be ignored when all required v3 fields are present");
    let message = error.to_string();
    assert!(
        message.contains("agent_type") || message.contains("system_prompt"),
        "hybrid payload rejection should identify a legacy field: {message}"
    );
}
