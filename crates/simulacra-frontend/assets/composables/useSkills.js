import { ref } from 'vue';
import { gql } from '/api/graphql.js';

const LIST = `query { skills(page:{first:200}) { edges { node { id name } } } }`;

export function useSkills() {
  const skills = ref([]);
  const loading = ref(false);
  const error = ref(null);

  async function list() {
    loading.value = true; error.value = null;
    try {
      const data = await gql(LIST);
      skills.value = data.skills.edges.map(e => e.node);
    } catch (e) { error.value = e; skills.value = []; }
    finally { loading.value = false; }
  }

  return { skills, loading, error, list };
}
