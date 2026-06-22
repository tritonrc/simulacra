// useTasks.js — REST wrapper for task lifecycle operations.
//
// Currently exposes `createTask({ agentType, task })` which POSTs to
// `/api/v1/tasks/create`. The server response is the standard envelope
// `{ ok: true, data: { task_id, state } }` (or `{ ok: false, error: { ... } }`
// on failure) — see `simulacra-server::server::create_task`. We unwrap the
// envelope here so callers get a clean `{ taskId, state }` shape.
//
// On HTTP error or `ok: false`, this throws `Error(message)` so the caller
// can surface the message inline (no toasts/alerts from the composable
// layer — UI decides how to render).

import { ref } from 'vue';

export function useTasks() {
  const loading = ref(false);
  const error = ref(null);

  async function createTask({ agentType, task }) {
    loading.value = true;
    error.value = null;
    try {
      const response = await fetch('/api/v1/tasks/create', {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          // NoAuthProvider ignores this in dev, but the header is the
          // documented tenant hint and is harmless to send. Production
          // auth providers may key off it.
          'x-simulacra-tenant': 'default',
        },
        body: JSON.stringify({
          agent_type: agentType,
          task,
        }),
      });

      const text = await response.text();
      let payload = null;
      if (text) {
        try { payload = JSON.parse(text); } catch { /* not JSON */ }
      }

      if (!response.ok) {
        const msg = payload?.error?.message
          || payload?.error
          || text
          || `HTTP ${response.status}`;
        throw new Error(typeof msg === 'string' ? msg : JSON.stringify(msg));
      }

      // Server envelope: { ok: true, data: { task_id, state } }
      if (payload && payload.ok === false) {
        const msg = payload.error?.message || 'task creation failed';
        throw new Error(msg);
      }
      const data = payload?.data ?? payload ?? {};
      const taskId = data.task_id ?? data.taskId;
      if (!taskId) {
        throw new Error('task_id missing from response');
      }
      return { taskId, state: data.state };
    } catch (e) {
      error.value = e;
      throw e;
    } finally {
      loading.value = false;
    }
  }

  return { loading, error, createTask };
}
