import { ref } from 'vue';

// The server emits events with `event:` (dotted, S031) AND legacy
// `type:` (snake_case). Accept both for forward-compat.
const TERMINAL_TYPES = new Set([
  'task_complete', 'task_failed', 'task_cancelled',
  'task.completed', 'task.failed', 'task.cancelled',
]);
const COMPLETED_TYPES = new Set(['task_complete', 'task.completed']);

export function useTaskStream() {
  const events = ref([]);
  const status = ref('idle'); // idle | running | completed | failed | error
  const error = ref(null);
  let source = null;
  let attemptedReconnect = false;
  let currentTaskId = null;

  function open(taskId) {
    currentTaskId = taskId;
    status.value = 'running';
    error.value = null;
    events.value = [];
    attemptedReconnect = false;
    connect();
  }

  function connect() {
    source = new EventSource(`/api/v1/tasks/${currentTaskId}/events`);
    source.addEventListener('message', (e) => {
      try {
        const ev = JSON.parse(e.data);
        events.value = [...events.value, ev];
        // task.state_changed { to: "completed" | "failed" | ... } is the
        // primary terminal signal; the legacy task_complete/task_failed
        // top-level types are a fallback.
        const eventType = ev.event ?? ev.type;
        const stateTo = ev.event === 'task.state_changed' ? ev.to : null;
        if (TERMINAL_TYPES.has(eventType) || (stateTo && ['completed', 'failed', 'cancelled'].includes(stateTo))) {
          const completed = COMPLETED_TYPES.has(eventType) || stateTo === 'completed';
          status.value = completed ? 'completed' : 'failed';
          source?.close();
        }
      } catch (parseErr) {
        error.value = parseErr;
      }
    });
    source.addEventListener('error', () => {
      if (!attemptedReconnect && status.value === 'running') {
        attemptedReconnect = true;
        source?.close();
        connect();
      } else {
        status.value = 'error';
        error.value = new Error('stream interrupted — task may still be running');
        source?.close();
      }
    });
  }

  function close() {
    source?.close();
    source = null;
  }

  return { events, status, error, open, close };
}
