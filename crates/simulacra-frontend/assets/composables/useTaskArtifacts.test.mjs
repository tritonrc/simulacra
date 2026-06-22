import { test } from 'node:test';
import assert from 'node:assert/strict';

let mockResponse, lastUrl;
globalThis.fetch = async (url) => {
  lastUrl = url;
  return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => mockResponse };
};

const { useTaskArtifacts } = await import('./useTaskArtifacts.js');

test('refresh(taskId) unwraps the { ok, data: { artifacts } } envelope', async () => {
  mockResponse = { ok: true, data: { artifacts: [{ path: 'duplicates.csv', size: 1024 }] } };
  const { artifacts, refresh } = useTaskArtifacts();
  await refresh('task_x');
  assert.equal(lastUrl, '/api/v1/tasks/task_x/artifacts');
  assert.equal(artifacts.value.length, 1);
  assert.equal(artifacts.value[0].path, 'duplicates.csv');
});

test('refresh(taskId) tolerates the bare { artifacts } shape for older fixtures', async () => {
  mockResponse = { artifacts: [{ path: 'duplicates.csv', size: 1024 }] };
  const { artifacts, refresh } = useTaskArtifacts();
  await refresh('task_x');
  assert.equal(artifacts.value.length, 1);
});

test('downloadUrl(taskId, path) returns the artifact byte URL', () => {
  const { downloadUrl } = useTaskArtifacts();
  assert.equal(downloadUrl('task_x', 'sub/file.csv'), '/api/v1/tasks/task_x/artifacts/sub/file.csv');
});
