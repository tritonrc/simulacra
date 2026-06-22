import { test, beforeEach } from 'node:test';
import assert from 'node:assert/strict';

let mockResponse, lastCall;
globalThis.fetch = async (url, opts) => {
  lastCall = { url, opts };
  return { ok: true, status: 200, headers: { get: () => 'application/json' }, json: async () => mockResponse };
};
const { useChannels } = await import('./useChannels.js');
beforeEach(() => { mockResponse = null; lastCall = null; });

test('list() returns channels from edges', async () => {
  mockResponse = { data: { channels: { edges: [{ node: { id: '1', name: '#support', kind: 'SLACK' } }] } } };
  const { list, channels } = useChannels();
  await list();
  assert.equal(channels.value.length, 1);
  assert.equal(channels.value[0].name, '#support');
});

test('create({ name, kind, config }) issues createChannel mutation and refreshes list', async () => {
  // First call: createChannel mutation. Second call: list refresh.
  let calls = 0;
  globalThis.fetch = async (url, opts) => {
    calls++;
    if (calls === 1) {
      const body = JSON.parse(opts.body);
      assert.match(body.query, /createChannel/);
      return { ok: true, status: 200, headers: { get: () => 'application/json' }, json: async () => ({ data: { createChannel: { id: '2', name: 'new', kind: 'SLACK' } } }) };
    }
    return { ok: true, status: 200, headers: { get: () => 'application/json' }, json: async () => ({ data: { channels: { edges: [{ node: { id: '2', name: 'new', kind: 'SLACK' } }] } } }) };
  };
  const { create, channels } = useChannels();
  const channel = await create({ name: 'new', kind: 'SLACK', config: {} });
  assert.equal(channel.id, '2');
  assert.equal(channels.value[0].id, '2');
});
