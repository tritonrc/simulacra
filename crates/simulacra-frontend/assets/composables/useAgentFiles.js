import { ref } from 'vue';
import { gql } from '/api/graphql.js';
import { restMultipart } from '/api/rest.js';

const DETACH = `mutation($agentId: ID!, $fileId: ID!) { detachAgentFile(agentId: $agentId, fileId: $fileId) }`;

export function useAgentFiles(agentId) {
  const error = ref(null);
  const uploading = ref(false);

  async function upload(file) {
    uploading.value = true;
    error.value = null;
    try {
      const fd = new FormData();
      fd.append('file', file, file.name);
      return await restMultipart(`/api/v1/agents/${agentId}/files`, fd);
    } catch (e) { error.value = e; throw e; }
    finally { uploading.value = false; }
  }

  async function detach(fileId) {
    error.value = null;
    try {
      const data = await gql(DETACH, { agentId, fileId });
      return data.detachAgentFile;
    } catch (e) { error.value = e; throw e; }
  }

  return { uploading, error, upload, detach };
}
