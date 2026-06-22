import { ref } from 'vue';
import { gql } from '/api/graphql.js';
import { useTasks } from '/composables/useTasks.js';

const LIST_QUERY = `
  query {
    agents(page: { first: 100 }) {
      edges {
        node {
          id name updatedAt
          channels { id name kind }
          tools { id name kind }
          skills { id name }
          files { id name sizeBytes }
        }
      }
    }
  }
`;

const GET_QUERY = `
  query($id: ID!) {
    agent(id: $id) {
      id name systemPrompt updatedAt
      capabilities
      channels { id name kind config }
      tools { id name kind description }
      skills { id name }
      files { id name sizeBytes }
    }
  }
`;

const CREATE = `
  mutation($input: CreateAgentInput!) {
    createAgent(input: $input) { id name }
  }
`;

const UPDATE = `
  mutation($id: ID!, $input: UpdateAgentInput!) {
    updateAgent(id: $id, input: $input) { id name }
  }
`;

export function useAgents() {
  const agents = ref([]);
  const loading = ref(false);
  const error = ref(null);

  async function list() {
    loading.value = true; error.value = null;
    try {
      const data = await gql(LIST_QUERY);
      agents.value = data.agents.edges.map(e => e.node);
    } catch (e) { error.value = e; agents.value = []; }
    finally { loading.value = false; }
  }

  async function get(id) {
    loading.value = true; error.value = null;
    try { return (await gql(GET_QUERY, { id })).agent; }
    catch (e) { error.value = e; return null; }
    finally { loading.value = false; }
  }

  async function create(input) {
    error.value = null;
    try {
      const data = await gql(CREATE, { input });
      await list();
      return data.createAgent;
    } catch (e) { error.value = e; throw e; }
  }

  async function update(id, input) {
    error.value = null;
    try {
      const data = await gql(UPDATE, { id, input });
      await list();
      return data.updateAgent;
    } catch (e) { error.value = e; throw e; }
  }

  async function saveAndRun(input, taskPrompt, existingId) {
    const saved = existingId
      ? await update(existingId, input)
      : await create(input);
    // Delegate task creation to useTasks().createTask, which unwraps the
    // standard REST envelope ({ ok, data, error }) and throws Error(message)
    // with the server's error.message on { ok: false } / non-2xx responses.
    const { taskId } = await useTasks().createTask({
      agentType: saved.name,
      task: taskPrompt,
    });
    return { taskId, agentId: saved.id, agentName: saved.name };
  }

  return { agents, loading, error, list, get, create, update, saveAndRun };
}
