import { test } from 'node:test';
import assert from 'node:assert/strict';

class MockEventSource {
  constructor(url) {
    this.url = url;
    this.listeners = {};
    MockEventSource.lastInstance = this;
  }
  addEventListener(type, fn) { this.listeners[type] = fn; }
  close() { this.closed = true; }
  emit(type, data) { this.listeners[type]?.({ data: typeof data === 'string' ? data : JSON.stringify(data) }); }
}
globalThis.EventSource = MockEventSource;

const { useTaskStream } = await import('./useTaskStream.js');

test('opens EventSource and pushes events into reactive array', () => {
  const { events, status, open, close } = useTaskStream();
  open('task_x');
  assert.equal(MockEventSource.lastInstance.url, '/api/v1/tasks/task_x/events');
  MockEventSource.lastInstance.emit('message', { type: 'token', text: 'hi' });
  MockEventSource.lastInstance.emit('message', { type: 'task_complete' });
  assert.equal(events.value.length, 2);
  assert.equal(status.value, 'completed');
  close();
});

test('close() shuts down EventSource', () => {
  const { open, close } = useTaskStream();
  open('task_x');
  close();
  assert.equal(MockEventSource.lastInstance.closed, true);
});

test('error event sets status to error', () => {
  const { open, status } = useTaskStream();
  open('task_x');
  MockEventSource.lastInstance.listeners.error?.({});
  // reconnect attempt happens — status remains "running" until second error
  MockEventSource.lastInstance.listeners.error?.({});
  assert.equal(status.value, 'error');
});
