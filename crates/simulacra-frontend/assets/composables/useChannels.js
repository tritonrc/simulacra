import { ref } from 'vue';
import { gql } from '/api/graphql.js';

const LIST = `query { channels(page:{first:100}) { edges { node { id name kind config } } } }`;
const CREATE = `mutation($input: CreateChannelInput!) { createChannel(input: $input) { id name kind config } }`;

export function useChannels() {
  const channels = ref([]);
  const loading = ref(false);
  const error = ref(null);

  async function list() {
    loading.value = true; error.value = null;
    try {
      const data = await gql(LIST);
      channels.value = data.channels.edges.map(e => e.node);
    } catch (e) { error.value = e; channels.value = []; }
    finally { loading.value = false; }
  }

  async function create(input) {
    error.value = null;
    try {
      const data = await gql(CREATE, { input });
      await list();
      return data.createChannel;
    } catch (e) { error.value = e; throw e; }
  }

  return { channels, loading, error, list, create };
}
