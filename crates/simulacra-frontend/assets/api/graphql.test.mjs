import { test } from 'node:test';
import assert from 'node:assert/strict';
import { gql, GraphQLError } from './graphql.js';

function mockFetch(response) {
  return async (url, opts) => {
    mockFetch.lastCall = { url, opts };
    return {
      ok: response.ok ?? true,
      status: response.status ?? 200,
      json: async () => response.body,
    };
  };
}

test('gql posts query + variables to /graphql', async () => {
  globalThis.fetch = mockFetch({ body: { data: { __typename: 'Query' } } });
  const data = await gql('{ __typename }', { x: 1 });
  assert.equal(data.__typename, 'Query');
  assert.equal(mockFetch.lastCall.url, '/graphql');
  assert.equal(mockFetch.lastCall.opts.method, 'POST');
  const body = JSON.parse(mockFetch.lastCall.opts.body);
  assert.equal(body.query, '{ __typename }');
  assert.deepEqual(body.variables, { x: 1 });
});

test('gql throws GraphQLError on errors[]', async () => {
  globalThis.fetch = mockFetch({ body: { errors: [{ message: 'boom' }] } });
  await assert.rejects(() => gql('{ x }'), e => e instanceof GraphQLError && e.message.includes('boom'));
});

test('gql throws on non-OK status', async () => {
  globalThis.fetch = mockFetch({ ok: false, status: 500, body: {} });
  await assert.rejects(() => gql('{ x }'));
});
