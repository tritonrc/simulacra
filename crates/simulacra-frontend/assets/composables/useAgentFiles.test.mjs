import { test } from 'node:test';
import assert from 'node:assert/strict';

let calls = [];
globalThis.fetch = async (url, opts) => {
  calls.push({ url, opts });
  if (url.endsWith('/files') && opts.method === 'POST') {
    return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ id: 'f1', name: 'r.pdf', size: 100 }) };
  }
  // detach via GraphQL
  return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ data: { detachAgentFile: true } }) };
};
const { useAgentFiles } = await import('./useAgentFiles.js');

test('upload(agentId, file) POSTs multipart to /api/v1/agents/:id/files', async () => {
  calls = [];
  const { upload } = useAgentFiles('agent_1');
  const blob = new Blob(['hello']);
  const file = new File([blob], 'r.pdf');
  const result = await upload(file);
  assert.equal(result.id, 'f1');
  assert.equal(calls[0].url, '/api/v1/agents/agent_1/files');
  assert.equal(calls[0].opts.method, 'POST');
  assert.ok(calls[0].opts.body instanceof FormData);
});

test('detach(fileId) issues detachAgentFile mutation', async () => {
  calls = [];
  const { detach } = useAgentFiles('agent_1');
  const ok = await detach('f1');
  assert.equal(ok, true);
  const body = JSON.parse(calls[0].opts.body);
  assert.match(body.query, /detachAgentFile/);
  assert.equal(body.variables.agentId, 'agent_1');
  assert.equal(body.variables.fileId, 'f1');
});
