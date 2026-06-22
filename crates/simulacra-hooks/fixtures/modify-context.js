function invoke(phase, operation, context) {
    if (phase === "after" && operation === "tool_call") {
        var ctx = JSON.parse(context);
        if (ctx.result && /\d{3}-\d{2}-\d{4}/.test(JSON.stringify(ctx.result))) {
            ctx.result = JSON.stringify(ctx.result).replace(/\d{3}-\d{2}-\d{4}/g, "***-**-****");
            ctx.result = JSON.parse(ctx.result);
            return { continue: JSON.stringify(ctx) };
        }
    }
    return { continue: null };
}
