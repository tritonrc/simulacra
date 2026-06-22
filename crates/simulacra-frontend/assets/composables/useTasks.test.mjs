import { test, beforeEach } from 'node:test';
import assert from 'node:assert/strict';

let mockResponse;
let lastCall;

function makeResponse({ ok = true, status = 200, body = '', contentType = 'application/json' } = {}) {
  return {
    ok,
    status,
    headers: { get: (k) => (k.toLowerCase() === 'content-type' ? contentType : null) },
    text: async () => (typeof body === 'string' ? body : JSON.stringify(body)),
  };
}

globalThis.fetch = async (url, opts) => {
  lastCall = { url, opts };
  return mockResponse;
};

const { useTasks } = await import('./useTasks.js');

beforeEach(() => { lastCall = null; mockResponse = null; });

test('createTask POSTs to /api/v1/tasks/create with json body and tenant header', async () => {
  mockResponse = makeResponse({ body: { ok: true, data: { task_id: 'task_abc', state: 'queued' } } });
  const { createTask } = useTasks();
  const result = await createTask({ agentType: 'foo', task: 'do thing' });
  assert.equal(lastCall.url, '/api/v1/tasks/create');
  assert.equal(lastCall.opts.method, 'POST');
  assert.equal(lastCall.opts.headers['content-type'], 'application/json');
  assert.equal(lastCall.opts.headers['x-simulacra-tenant'], 'default');
  const body = JSON.parse(lastCall.opts.body);
  assert.equal(body.agent_type, 'foo');
  assert.equal(body.task, 'do thing');
  assert.equal(result.taskId, 'task_abc');
  assert.equal(result.state, 'queued');
});

test('createTask unwraps envelope { ok: true, data: { task_id } }', async () => {
  mockResponse = makeResponse({ body: { ok: true, data: { task_id: 't1' } } });
  const { createTask } = useTasks();
  const result = await createTask({ agentType: 'a', task: 't' });
  assert.equal(result.taskId, 't1');
});

test('createTask throws Error with server message on { ok: false }', async () => {
  mockResponse = makeResponse({
    ok: false,
    status: 500,
    body: { ok: false, error: { code: 'internal_error', message: 'boom' } },
  });
  const { createTask, error } = useTasks();
  await assert.rejects(
    () => createTask({ agentType: 'a', task: 't' }),
    /boom/,
  );
  assert.match(error.value.message, /boom/);
});

test('createTask throws on non-2xx with text body', async () => {
  mockResponse = makeResponse({ ok: false, status: 503, body: 'service unavailable', contentType: 'text/plain' });
  const { createTask } = useTasks();
  await assert.rejects(
    () => createTask({ agentType: 'a', task: 't' }),
    /service unavailable|HTTP 503/,
  );
});

test('createTask throws when task_id missing', async () => {
  mockResponse = makeResponse({ body: { ok: true, data: {} } });
  const { createTask } = useTasks();
  await assert.rejects(
    () => createTask({ agentType: 'a', task: 't' }),
    /task_id missing/,
  );
});
