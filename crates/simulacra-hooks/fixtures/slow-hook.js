function invoke(phase, operation, context) {
    var start = Date.now();
    while (Date.now() - start < 5000) {} // busy wait
    return { continue: null };
}
