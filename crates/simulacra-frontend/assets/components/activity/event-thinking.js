import { defineComponent, ref } from 'vue';

export default defineComponent({
  name: 'EventThinking',
  props: { event: { type: Object, required: true } },
  template: `
    <div class="ev-thinking" :class="{ 'ev-thinking--open': open }">
      <button class="ev-thinking__toggle" @click="open = !open">
        {{ open ? '▾' : '▸' }} thinking
      </button>
      <pre v-if="open" class="ev-thinking__body">{{ event.text }}</pre>
    </div>
  `,
  setup() {
    const open = ref(false);
    return { open };
  },
});
