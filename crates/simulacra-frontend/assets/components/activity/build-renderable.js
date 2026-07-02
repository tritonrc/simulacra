export function buildRenderable(events) {
  const toolBuckets = new Map(); // tool_call_id -> aggregated tool node
  const pendingByIndex = new Map(); // provider index -> streamed pending tool node
  const out = [];

  for (const ev of events) {
    const type = ev.event ?? ev.type;
    if (type === 'tool.call_delta') {
      const index = Number.isFinite(ev.index) ? ev.index : 0;
      const id = ev.tool_call_id || null;
      let node = (id && toolBuckets.get(id)) || pendingByIndex.get(index);
      if (!node) {
        node = {
          kind: 'tool',
          toolCallId: id,
          toolName: ev.tool_name || 'tool call',
          arguments: '',
          pendingArguments: true,
          outputLines: [],
          durationMs: null,
          isError: false,
          finished: false,
        };
        pendingByIndex.set(index, node);
        out.push({ kind: 'tool', node, key: 'tool-delta:' + (id || index) });
      }
      if (id) {
        node.toolCallId = id;
        toolBuckets.set(id, node);
      }
      if (ev.tool_name) node.toolName = ev.tool_name;
      node.arguments += ev.arguments_delta || '';
    } else if (type === 'tool.called') {
      let node = toolBuckets.get(ev.tool_call_id);
      if (!node && pendingByIndex.size === 1) {
        node = pendingByIndex.values().next().value;
      }
      if (!node) {
        node = {
          kind: 'tool',
          outputLines: [],
          durationMs: null,
          isError: false,
          finished: false,
        };
        out.push({ kind: 'tool', node, key: 'tool:' + ev.tool_call_id });
      }
      Object.assign(node, {
        kind: 'tool',
        toolCallId: ev.tool_call_id,
        toolName: ev.tool_name,
        arguments: ev.arguments,
        pendingArguments: false,
        finished: false,
      });
      toolBuckets.set(ev.tool_call_id, node);
      for (const [index, pendingNode] of pendingByIndex.entries()) {
        if (pendingNode === node) pendingByIndex.delete(index);
      }
    } else if (type === 'tool.output') {
      const node = toolBuckets.get(ev.tool_call_id);
      if (node) node.outputLines.push(ev.line);
    } else if (type === 'tool.result') {
      const node = toolBuckets.get(ev.tool_call_id);
      if (node) {
        node.durationMs = ev.duration_ms;
        node.isError = !!ev.is_error;
        node.finished = true;
      }
    } else if (type === 'agent.message') {
      out.push({ kind: 'message', payload: ev, key: 'msg:' + ev.seq });
    } else if (type === 'agent.thinking') {
      out.push({ kind: 'thinking', payload: ev, key: 'think:' + ev.seq });
    } else if (type === 'agent.child_spawned' || type === 'agent.child_finished') {
      out.push({ kind: 'child', payload: ev, key: 'child:' + ev.seq });
    }
  }
  return out;
}
