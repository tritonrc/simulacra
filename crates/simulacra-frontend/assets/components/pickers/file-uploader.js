import { defineComponent, ref } from 'vue';
import { useAgentFiles } from '/composables/useAgentFiles.js';

export default defineComponent({
  name: 'FileUploader',
  props: {
    agentId: { type: String, default: null },
    files: { type: Array, default: () => [] },
  },
  emits: ['change'],
  template: `
    <div class="picker">
      <div v-if="!agentId" class="picker__hint">Save the agent first to upload files.</div>
      <div v-else>
        <div v-for="f in files" :key="f.id" class="file-row">
          <span>{{ f.name }} <span class="dim">({{ f.sizeBytes }} bytes)</span></span>
          <button @click="onDetach(f.id)">remove</button>
        </div>
        <input type="file" @change="onPick($event)" :disabled="uploading" />
        <span v-if="uploading">uploading…</span>
        <span v-if="error" class="err">{{ error.message }}</span>
      </div>
    </div>
  `,
  setup(props, { emit }) {
    const composable = ref(null);
    if (props.agentId) composable.value = useAgentFiles(props.agentId);
    const uploading = ref(false);
    const error = ref(null);

    async function onPick(e) {
      if (!props.agentId) return;
      const file = e.target.files[0];
      if (!file) return;
      uploading.value = true;
      try {
        if (!composable.value) composable.value = useAgentFiles(props.agentId);
        const result = await composable.value.upload(file);
        emit('change', [...props.files, result]);
      } catch (err) { error.value = err; }
      finally { uploading.value = false; e.target.value = ''; }
    }
    async function onDetach(fileId) {
      if (!composable.value) composable.value = useAgentFiles(props.agentId);
      try {
        await composable.value.detach(fileId);
        emit('change', props.files.filter(f => f.id !== fileId));
      } catch (err) { error.value = err; }
    }
    return { uploading, error, onPick, onDetach };
  },
});
