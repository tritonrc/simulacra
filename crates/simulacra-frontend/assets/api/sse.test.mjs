import { test } from 'node:test';
import assert from 'node:assert/strict';

// Mock EventSource — node doesn't have one.
class MockEventSource {
  constructor(url) {
    this.url = url;
    this.listeners = {};
    MockEventSource.lastInstance = this;
  }
  addEventListener(type, fn) { this.listeners[type] = fn; }
  close() { this.closed = true; }
  emit(type, data) {
    const listener = this.listeners[type];
    if (listener) listener({ data: JSON.stringify(data) });
  }
}

globalThis.EventSource = MockEventSource;

const { openTaskStream } = await import('./sse.js');

test('opens EventSource at /api/v1/tasks/:id/events', () => {
  const handle = openTaskStream('task_abc');
  assert.equal(MockEventSource.lastInstance.url, '/api/v1/tasks/task_abc/events');
  handle.close();
});

test('parses message events into onEvent callbacks', () => {
  let received = [];
  const handle = openTaskStream('task_abc', (event) => received.push(event));
  MockEventSource.lastInstance.emit('message', { type: 'token', text: 'hi' });
  MockEventSource.lastInstance.emit('message', { type: 'task_complete' });
  assert.equal(received.length, 2);
  assert.equal(received[0].type, 'token');
  assert.equal(received[1].type, 'task_complete');
  handle.close();
});

test('close() shuts down the EventSource', () => {
  const handle = openTaskStream('task_abc');
  handle.close();
  assert.equal(MockEventSource.lastInstance.closed, true);
});

test('onError fires when EventSource dispatches error', () => {
  let error;
  const handle = openTaskStream('task_abc', () => {}, (err) => { error = err; });
  MockEventSource.lastInstance.listeners.error?.({});
  assert.ok(error);
  handle.close();
});
