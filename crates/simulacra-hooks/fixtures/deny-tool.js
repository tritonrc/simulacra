function invoke(phase, operation, context) {
    if (phase === "before" && operation === "tool_call") {
        var ctx = JSON.parse(context);
        if (ctx.tool === "dangerous_tool") {
            return { deny: "dangerous_tool is not allowed" };
        }
    }
    return { continue: null };
}
