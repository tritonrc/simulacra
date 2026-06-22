import { ref } from 'vue';
import { restJson } from '/api/rest.js';

export function useTriggers() {
  const webhooks = ref([]);
  const schedules = ref([]);
  const loading = ref(false);
  const error = ref(null);

  async function list(agentId) {
    loading.value = true; error.value = null;
    try {
      const path = agentId ? `/api/v1/triggers?agent=${encodeURIComponent(agentId)}` : '/api/v1/triggers';
      const payload = await restJson(path);
      // Server envelope: { ok: true, data: { webhooks, schedules } }
      const data = payload?.data ?? payload ?? {};
      webhooks.value = data.webhooks || [];
      schedules.value = data.schedules || [];
    } catch (e) { error.value = e; webhooks.value = []; schedules.value = []; }
    finally { loading.value = false; }
  }

  return { webhooks, schedules, loading, error, list };
}
