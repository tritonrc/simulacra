import { ref } from 'vue';
import { restJson } from '/api/rest.js';

export function useTaskArtifacts() {
  const artifacts = ref([]);
  const loading = ref(false);
  const error = ref(null);

  async function refresh(taskId) {
    loading.value = true; error.value = null;
    try {
      const payload = await restJson(`/api/v1/tasks/${taskId}/artifacts`);
      // Server envelope: { ok: true, data: { artifacts: [...] } }
      const data = payload?.data ?? payload ?? {};
      artifacts.value = data.artifacts || [];
    } catch (e) { error.value = e; artifacts.value = []; }
    finally { loading.value = false; }
  }

  function downloadUrl(taskId, path) {
    return `/api/v1/tasks/${taskId}/artifacts/${path}`;
  }

  return { artifacts, loading, error, refresh, downloadUrl };
}
