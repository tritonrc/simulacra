import { ref } from 'vue';
import { gql } from '/api/graphql.js';

const LIST = `query { availableTools { id name kind description } }`;

export function useTools() {
  const tools = ref([]);
  const loading = ref(false);
  const error = ref(null);

  async function list() {
    loading.value = true; error.value = null;
    try {
      const data = await gql(LIST);
      tools.value = data.availableTools;
    } catch (e) { error.value = e; tools.value = []; }
    finally { loading.value = false; }
  }

  return { tools, loading, error, list };
}
