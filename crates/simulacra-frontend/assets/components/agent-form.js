import { defineComponent, ref, onMounted, watch, computed } from 'vue';
import { useAgents } from '/composables/useAgents.js';
import { showToast } from '/components/app-shell.js';
import ChannelPicker from '/components/pickers/channel-picker.js';
import ToolPicker from '/components/pickers/tool-picker.js';
import SkillPicker from '/components/pickers/skill-picker.js';
import FileUploader from '/components/pickers/file-uploader.js';
import TriggerList from '/components/pickers/trigger-list.js';

export default defineComponent({
  name: 'AgentForm',
  props: { id: { type: String, default: null } },
  components: { ChannelPicker, ToolPicker, SkillPicker, FileUploader, TriggerList },
  template: `
    <form class="agent-form" @submit.prevent="onSave">
      <div class="agent-form__crumbs">
        <router-link to="/">Agents</router-link>
        <span class="dim">/</span>
        <span>{{ isEdit ? 'Edit' : 'New' }}</span>
      </div>

      <div class="agent-form__grid">
        <div class="agent-form__meta">
          <div class="field">
            <div class="label">Title</div>
            <input v-model="form.name" required placeholder="my-agent" />
          </div>

          <div class="field">
            <div class="label">Channels</div>
            <channel-picker v-model="form.channelIds" />
          </div>

          <div class="field">
            <div class="label">Tools</div>
            <tool-picker v-model="form.capabilities" />
          </div>

          <div class="field">
            <div class="label">Skill</div>
            <skill-picker v-model="form.skillId" />
          </div>

          <div class="field">
            <div class="label">Triggers (read-only)</div>
            <trigger-list :agent-id="id" />
          </div>
        </div>

        <div class="agent-form__main">
          <div class="field">
            <div class="label">Instructions</div>
            <textarea v-model="form.systemPrompt" rows="14" placeholder="You are a..."></textarea>
          </div>
          <div class="field">
            <div class="label">Files</div>
            <file-uploader :agent-id="id" :files="form.files" @change="form.files = $event" />
          </div>

          <div class="agent-form__actions">
            <router-link to="/"><button type="button">Cancel</button></router-link>
            <button type="submit" :disabled="saving">{{ saving ? 'Saving…' : 'Save' }}</button>
            <button type="button" class="primary" @click="onSaveAndRun" :disabled="saving">▶ Save &amp; Run</button>
          </div>

          <div v-if="runDialog" class="run-dialog">
            <div class="run-dialog__panel">
              <div class="label">What should this agent do?</div>
              <textarea v-model="runPrompt" rows="4" placeholder="describe the task…"></textarea>
              <div class="agent-form__actions">
                <button type="button" @click="runDialog = false">cancel</button>
                <button type="button" class="primary" @click="confirmRun">Run</button>
              </div>
            </div>
          </div>
        </div>
      </div>
    </form>
  `,
  setup(props) {
    const { get, create, update, saveAndRun } = useAgents();
    const form = ref({
      name: '',
      systemPrompt: '',
      capabilities: [],
      channelIds: [],
      skillId: null,
      files: [],
    });
    const saving = ref(false);
    const runDialog = ref(false);
    const runPrompt = ref('');

    const isEdit = computed(() => !!props.id);

    async function load() {
      if (!props.id) return;
      const a = await get(props.id);
      if (!a) { showToast('Agent not found'); return; }
      form.value = {
        name: a.name,
        systemPrompt: a.systemPrompt,
        capabilities: a.capabilities || [],
        channelIds: (a.channels || []).map(c => c.id),
        skillId: (a.skills && a.skills[0]?.id) || null,
        files: a.files || [],
      };
    }

    onMounted(load);
    watch(() => props.id, load);

    function buildInput() {
      return {
        name: form.value.name,
        systemPrompt: form.value.systemPrompt,
        capabilities: form.value.capabilities,
        channelIds: form.value.channelIds,
        skillIds: form.value.skillId ? [form.value.skillId] : [],
      };
    }

    async function onSave() {
      saving.value = true;
      try {
        if (isEdit.value) await update(props.id, buildInput());
        else await create(buildInput());
        window.location.hash = '#/';
      } catch (e) {
        showToast(`Save failed: ${e.message}`);
      } finally { saving.value = false; }
    }

    function onSaveAndRun() {
      runDialog.value = true;
      runPrompt.value = '';
    }

    async function confirmRun() {
      saving.value = true;
      runDialog.value = false;
      try {
        const { taskId, agentId } = await saveAndRun(buildInput(), runPrompt.value, props.id || undefined);
        window.location.hash = `#/agents/${agentId}/run/${taskId}`;
      } catch (e) {
        showToast(`Run failed: ${e.message}`);
      } finally { saving.value = false; }
    }

    return { form, saving, runDialog, runPrompt, isEdit, onSave, onSaveAndRun, confirmRun };
  },
});
