// PII scanner — blocks tool results containing SSN patterns
function invoke(phase, operation, context) {
    if (phase === "after" && operation === "tool_call") {
        var ctx = JSON.parse(context);
        var resultStr = JSON.stringify(ctx.result || "");

        // Check for SSN pattern (XXX-XX-XXXX)
        if (/\d{3}-\d{2}-\d{4}/.test(resultStr)) {
            return { kill: "PII detected: SSN pattern found in tool output" };
        }

        // Check for credit card patterns (16 digits)
        if (/\b\d{4}[\s-]?\d{4}[\s-]?\d{4}[\s-]?\d{4}\b/.test(resultStr)) {
            return { kill: "PII detected: credit card pattern found in tool output" };
        }
    }
    return { continue: null };
}
