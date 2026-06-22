import { defineComponent, onMounted, ref, computed } from 'vue';
import { useChannels } from '/composables/useChannels.js';

export default defineComponent({
  name: 'ChannelPicker',
  props: { modelValue: { type: Array, default: () => [] } },
  emits: ['update:modelValue'],
  template: `
    <div class="picker">
      <div class="picker__chips">
        <span v-for="id in modelValue" :key="id" class="chip">
          {{ nameFor(id) }} <button class="chip__x" @click="remove(id)">×</button>
        </span>
      </div>
      <select @change="addFromSelect($event)">
        <option value="">+ add channel…</option>
        <option v-for="ch in available" :key="ch.id" :value="ch.id">{{ ch.name }} ({{ ch.kind }})</option>
        <option value="__new__">+ create new…</option>
      </select>
      <div v-if="creating" class="picker__inline-create">
        <input v-model="newName" placeholder="name" />
        <select v-model="newKind">
          <option>SLACK</option><option>TEAMS</option><option>EMAIL</option><option>WEBHOOK</option><option>MANUAL</option>
        </select>
        <button @click="createChannel">create</button>
        <button @click="creating = false">cancel</button>
      </div>
    </div>
  `,
  setup(props, { emit }) {
    const { channels, list, create } = useChannels();
    const creating = ref(false);
    const newName = ref('');
    const newKind = ref('SLACK');

    onMounted(() => list());

    const available = computed(() => channels.value.filter(c => !props.modelValue.includes(c.id)));

    function nameFor(id) {
      const c = channels.value.find(c => c.id === id);
      return c ? c.name : id;
    }
    function remove(id) {
      emit('update:modelValue', props.modelValue.filter(x => x !== id));
    }
    function addFromSelect(e) {
      const v = e.target.value;
      e.target.value = '';
      if (!v) return;
      if (v === '__new__') { creating.value = true; return; }
      emit('update:modelValue', [...props.modelValue, v]);
    }
    async function createChannel() {
      const ch = await create({ name: newName.value, kind: newKind.value, config: {} });
      newName.value = '';
      creating.value = false;
      emit('update:modelValue', [...props.modelValue, ch.id]);
    }
    return { available, nameFor, remove, addFromSelect, creating, newName, newKind, createChannel };
  },
});
