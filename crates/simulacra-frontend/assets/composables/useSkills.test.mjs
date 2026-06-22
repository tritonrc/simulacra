import { test } from 'node:test';
import assert from 'node:assert/strict';
let mockResponse;
globalThis.fetch = async () => ({ ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => mockResponse });
const { useSkills } = await import('./useSkills.js');

test('list() returns skills', async () => {
  mockResponse = { data: { skills: { edges: [{ node: { id: '1', name: 'triage' } }] } } };
  const { list, skills } = useSkills();
  await list();
  assert.equal(skills.value.length, 1);
  assert.equal(skills.value[0].name, 'triage');
});
