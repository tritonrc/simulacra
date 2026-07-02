import { test } from 'node:test';
import assert from 'node:assert/strict';

const { buildRenderable } = await import('./activity/build-renderable.js');

test('buildRenderable merges tool.call_delta events into a pending tool block', () => {
  const renderable = buildRenderable([
    {
      event: 'tool.call_delta',
      seq: 1,
      index: 0,
      tool_call_id: 'call-1',
      tool_name: 'file_read',
      arguments_delta: '{"path"',
    },
    {
      event: 'tool.call_delta',
      seq: 2,
      index: 0,
      tool_call_id: 'call-1',
      tool_name: 'file_read',
      arguments_delta: ':"/workspace/a.md"}',
    },
  ]);

  assert.equal(renderable.length, 1);
  assert.equal(renderable[0].kind, 'tool');
  assert.equal(renderable[0].node.toolCallId, 'call-1');
  assert.equal(renderable[0].node.toolName, 'file_read');
  assert.equal(renderable[0].node.arguments, '{"path":"/workspace/a.md"}');
  assert.equal(renderable[0].node.pendingArguments, true);
});

test('buildRenderable replaces streamed argument text with final tool.called arguments', () => {
  const renderable = buildRenderable([
    {
      event: 'tool.call_delta',
      seq: 1,
      index: 0,
      tool_call_id: 'call-1',
      tool_name: 'file_read',
      arguments_delta: '{"path"',
    },
    {
      event: 'tool.called',
      seq: 2,
      tool_call_id: 'call-1',
      tool_name: 'file_read',
      arguments: { path: '/workspace/final.md' },
    },
  ]);

  assert.equal(renderable.length, 1);
  assert.deepEqual(renderable[0].node.arguments, { path: '/workspace/final.md' });
  assert.equal(renderable[0].node.pendingArguments, false);
  assert.equal(renderable[0].node.finished, false);
});

test('buildRenderable does not reuse a finalized pending index for later streamed calls', () => {
  const renderable = buildRenderable([
    {
      event: 'tool.call_delta',
      seq: 1,
      index: 0,
      tool_call_id: 'call-1',
      tool_name: 'file_read',
      arguments_delta: '{"path":"/workspace/one.md"}',
    },
    {
      event: 'tool.called',
      seq: 2,
      tool_call_id: 'call-1',
      tool_name: 'file_read',
      arguments: { path: '/workspace/one.md' },
    },
    {
      event: 'tool.result',
      seq: 3,
      tool_call_id: 'call-1',
      tool_name: 'file_read',
      duration_ms: 10,
      is_error: false,
    },
    {
      event: 'tool.call_delta',
      seq: 4,
      index: 0,
      tool_call_id: 'call-2',
      tool_name: 'file_read',
      arguments_delta: '{"path":"/workspace/two.md"}',
    },
    {
      event: 'tool.called',
      seq: 5,
      tool_call_id: 'call-2',
      tool_name: 'file_read',
      arguments: { path: '/workspace/two.md' },
    },
  ]);

  assert.equal(renderable.length, 2);
  assert.equal(renderable[0].node.toolCallId, 'call-1');
  assert.deepEqual(renderable[0].node.arguments, { path: '/workspace/one.md' });
  assert.equal(renderable[0].node.finished, true);
  assert.equal(renderable[1].node.toolCallId, 'call-2');
  assert.deepEqual(renderable[1].node.arguments, { path: '/workspace/two.md' });
  assert.equal(renderable[1].node.pendingArguments, false);
});
