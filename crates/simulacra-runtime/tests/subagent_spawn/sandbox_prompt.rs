#[test]
fn default_system_prompt_describes_current_sandbox_affordances() {
    assert!(DEFAULT_SYSTEM_PROMPT.contains("fresh JS global/context"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("simulacra:path"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("simulacra:crypto"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("Cwd and env vars persist"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("node -"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("python -"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("/proc/mailbox/<filename>"));
    assert!(!DEFAULT_SYSTEM_PROMPT.contains("persistent QuickJS context"));
    assert!(!DEFAULT_SYSTEM_PROMPT.contains("No `cd`"));
}
