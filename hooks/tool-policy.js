// Tool policy — blocks dangerous shell commands
function invoke(phase, operation, context) {
    if (phase === "before" && operation === "tool_call") {
        var ctx = JSON.parse(context);

        if (ctx.tool === "shell_exec") {
            var cmd = (ctx.arguments && ctx.arguments.command) || "";

            // Block rm -rf
            if (/rm\s+(-[a-zA-Z]*r[a-zA-Z]*f|--recursive)\s/.test(cmd)) {
                return { deny: "rm -rf is blocked by organization policy" };
            }

            // Block curl to non-approved domains
            if (/curl\s/.test(cmd) && !/github\.com|api\.anthropic\.com/.test(cmd)) {
                return { deny: "curl to non-approved domain blocked by policy" };
            }
        }
    }
    return { continue: null };
}
