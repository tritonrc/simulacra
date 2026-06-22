import { test, beforeEach } from 'node:test';
import assert from 'node:assert/strict';

let mockResponse;
let lastCall;
globalThis.fetch = async (url, opts) => {
  lastCall = { url, opts };
  return {
    ok: true,
    status: 200,
    headers: { get: () => 'application/json' },
    json: async () => mockResponse,
  };
};

const { useAgents } = await import('./useAgents.js');

beforeEach(() => { lastCall = null; mockResponse = null; });

test('list() issues agents query', async () => {
  mockResponse = { data: { agents: { edges: [{ node: { id: '1', name: 'a' } }] } } };
  const { list, agents, loading, error } = useAgents();
  await list();
  assert.equal(loading.value, false);
  assert.equal(error.value, null);
  assert.equal(agents.value.length, 1);
  assert.equal(agents.value[0].name, 'a');
  const body = JSON.parse(lastCall.opts.body);
  assert.match(body.query, /agents/);
});

test('list() captures errors into error ref without throwing', async () => {
  mockResponse = { errors: [{ message: 'denied' }] };
  const { list, error, agents } = useAgents();
  await list();
  assert.match(error.value.message, /denied/);
  assert.deepEqual(agents.value, []);
});

test('get(id) fetches a single agent', async () => {
  mockResponse = { data: { agent: { id: '1', name: 'a', systemPrompt: 'be helpful' } } };
  const { get } = useAgents();
  const agent = await get('1');
  assert.equal(agent.name, 'a');
  const body = JSON.parse(lastCall.opts.body);
  assert.match(body.query, /agent\(/);
  assert.deepEqual(body.variables, { id: '1' });
});

test('create(input) issues createAgent mutation and returns the new agent', async () => {
  let calls = 0;
  globalThis.fetch = async (url, opts) => {
    calls++;
    const body = JSON.parse(opts.body);
    if (calls === 1) {
      assert.match(body.query, /createAgent/);
      return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ data: { createAgent: { id: 'new1', name: 'foo' } } }) };
    }
    return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ data: { agents: { edges: [] } } }) };
  };
  const { create } = useAgents();
  const agent = await create({ name: 'foo', systemPrompt: 'be helpful', capabilities: [], skillIds: [], channelIds: [] });
  assert.equal(agent.id, 'new1');
});

test('update(id, patch) issues updateAgent mutation', async () => {
  globalThis.fetch = async (url, opts) => {
    const body = JSON.parse(opts.body);
    assert.match(body.query, /updateAgent/);
    assert.equal(body.variables.id, 'a1');
    return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ data: { updateAgent: { id: 'a1', name: 'patched' } } }) };
  };
  const { update } = useAgents();
  const out = await update('a1', { name: 'patched' });
  assert.equal(out.name, 'patched');
});

test('saveAndRun unwraps REST envelope and returns { taskId, agentId, agentName }', async () => {
  let phase = 0;
  let createTaskBody;
  globalThis.fetch = async (url, opts) => {
    phase++;
    if (phase === 1) {
      // createAgent mutation
      return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ data: { createAgent: { id: 'a2', name: 'foo' } } }) };
    }
    if (phase === 2) {
      // refresh list() after create
      return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ data: { agents: { edges: [] } } }) };
    }
    // POST /api/v1/tasks/create — server returns the standard envelope.
    assert.equal(url, '/api/v1/tasks/create');
    assert.equal(opts.method, 'POST');
    createTaskBody = JSON.parse(opts.body);
    return {
      ok: true,
      status: 200,
      headers: { get: (k) => (k.toLowerCase() === 'content-type' ? 'application/json' : null) },
      text: async () => JSON.stringify({ ok: true, data: { task_id: 'task_xyz', state: 'queued' } }),
      json: async () => ({ ok: true, data: { task_id: 'task_xyz', state: 'queued' } }),
    };
  };
  const { saveAndRun } = useAgents();
  const result = await saveAndRun({ name: 'foo', systemPrompt: 'p', capabilities: [], skillIds: [], channelIds: [] }, 'do the thing');
  assert.equal(result.taskId, 'task_xyz');
  assert.equal(result.agentId, 'a2');
  assert.equal(result.agentName, 'foo');
  assert.equal(createTaskBody.agent_type, 'foo');
  assert.equal(createTaskBody.task, 'do the thing');
});

test('saveAndRun rejects with server error message when create endpoint returns { ok: false }', async () => {
  let phase = 0;
  globalThis.fetch = async (url, opts) => {
    phase++;
    if (phase === 1) {
      return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ data: { createAgent: { id: 'a3', name: 'bar' } } }) };
    }
    if (phase === 2) {
      return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ data: { agents: { edges: [] } } }) };
    }
    // POST /api/v1/tasks/create — server returns 500 with envelope error.
    return {
      ok: false,
      status: 500,
      headers: { get: (k) => (k.toLowerCase() === 'content-type' ? 'application/json' : null) },
      text: async () => JSON.stringify({ ok: false, error: { code: 'internal_error', message: 'boom' } }),
      json: async () => ({ ok: false, error: { code: 'internal_error', message: 'boom' } }),
    };
  };
  const { saveAndRun } = useAgents();
  await assert.rejects(
    () => saveAndRun({ name: 'bar', systemPrompt: 'p', capabilities: [], skillIds: [], channelIds: [] }, 'do the thing'),
    /boom/,
  );
});
