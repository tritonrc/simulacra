import { test } from 'node:test';
import assert from 'node:assert/strict';
let mockResponse, lastCall;
globalThis.fetch = async (url) => {
  lastCall = url;
  return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => mockResponse };
};
const { useTriggers } = await import('./useTriggers.js');

test('list(agentId) unwraps the { ok, data: { webhooks, schedules } } envelope', async () => {
  mockResponse = {
    ok: true,
    data: {
      webhooks: [{ path: '/hooks/x', agent_type: 'a', hmac: true }],
      schedules: [{ cron: '0 9 * * 1', agent_type: 'a', missed_policy: 'skip' }],
    },
  };
  const { list, webhooks, schedules } = useTriggers();
  await list('a');
  assert.equal(lastCall, '/api/v1/triggers?agent=a');
  assert.equal(webhooks.value.length, 1);
  assert.equal(schedules.value.length, 1);
});

test('list() with no agentId hits /api/v1/triggers (no filter)', async () => {
  mockResponse = { ok: true, data: { webhooks: [], schedules: [] } };
  const { list } = useTriggers();
  await list();
  assert.equal(lastCall, '/api/v1/triggers');
});

test('list() tolerates the bare { webhooks, schedules } shape for older fixtures', async () => {
  mockResponse = { webhooks: [{ path: '/x', agent_type: 'a', hmac: false }], schedules: [] };
  const { list, webhooks } = useTriggers();
  await list();
  assert.equal(webhooks.value.length, 1);
});
