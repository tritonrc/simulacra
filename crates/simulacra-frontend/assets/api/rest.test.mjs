import { test } from 'node:test';
import assert from 'node:assert/strict';
import { restJson, restMultipart } from './rest.js';

function mockFetch(response) {
  return async (url, opts) => {
    mockFetch.lastCall = { url, opts };
    return {
      ok: response.ok ?? true,
      status: response.status ?? 200,
      headers: { get: (k) => k === 'content-type' ? 'application/json' : null },
      json: async () => response.body,
    };
  };
}

test('restJson GETs by default', async () => {
  globalThis.fetch = mockFetch({ body: { ok: true } });
  const out = await restJson('/api/v1/foo');
  assert.equal(out.ok, true);
  assert.equal(mockFetch.lastCall.opts.method ?? 'GET', 'GET');
});

test('restJson POSTs with body', async () => {
  globalThis.fetch = mockFetch({ body: { ok: true } });
  await restJson('/api/v1/foo', { method: 'POST', body: { a: 1 } });
  const opts = mockFetch.lastCall.opts;
  assert.equal(opts.method, 'POST');
  assert.equal(opts.headers['content-type'], 'application/json');
  assert.equal(JSON.parse(opts.body).a, 1);
});

test('restJson throws on non-OK', async () => {
  globalThis.fetch = mockFetch({ ok: false, status: 404, body: { error: 'nope' } });
  await assert.rejects(() => restJson('/api/v1/foo'));
});

test('restMultipart sends FormData', async () => {
  globalThis.fetch = mockFetch({ body: { id: 'abc' } });
  const fd = new FormData();
  fd.append('file', new Blob(['hi']), 'hi.txt');
  const out = await restMultipart('/api/v1/upload', fd);
  assert.equal(out.id, 'abc');
  assert.equal(mockFetch.lastCall.opts.method, 'POST');
  assert.ok(mockFetch.lastCall.opts.body instanceof FormData);
});
