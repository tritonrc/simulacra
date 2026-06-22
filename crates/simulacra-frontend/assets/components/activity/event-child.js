import { defineComponent, ref } from 'vue';

export default defineComponent({
  name: 'EventChild',
  props: { event: { type: Object, required: true } },
  template: `
    <div class="ev-child" :class="{ 'ev-child--open': open }">
      <button class="ev-child__head" @click="open = !open">
        {{ open ? '▾' : '▸' }} sub-agent: {{ event.agent_type }}
        <span v-if="event.task_id" class="dim">{{ event.task_id }}</span>
      </button>
      <div v-if="open" class="ev-child__body">
        <pre>{{ JSON.stringify(event.events || [], null, 2) }}</pre>
      </div>
    </div>
  `,
  setup() {
    const open = ref(false);
    return { open };
  },
});
