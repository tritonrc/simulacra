// sse.js — wraps EventSource for /api/v1/tasks/:id/events.
//
// onEvent(event)  — called for each parsed message
// onError(err)    — called if the underlying EventSource errors
// Returns { close() }.

export function openTaskStream(taskId, onEvent = () => {}, onError = () => {}) {
  const url = `/api/v1/tasks/${taskId}/events`;
  const source = new EventSource(url);

  source.addEventListener('message', (e) => {
    try {
      const data = JSON.parse(e.data);
      onEvent(data);
    } catch (parseErr) {
      onError(parseErr);
    }
  });
  source.addEventListener('error', (e) => {
    onError(e);
  });

  return { close: () => source.close() };
}
