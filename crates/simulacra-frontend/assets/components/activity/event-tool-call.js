import { defineComponent, ref, computed } from 'vue';

export default defineComponent({
  name: 'EventToolCall',
  props: { node: { type: Object, required: true } },
  template: `
    <div class="ev-tool" :class="{ 'ev-tool--open': open, 'ev-tool--error': node.isError }">
      <button class="ev-tool__head" @click="open = !open">
        {{ open ? '▾' : '▸' }} <strong>{{ node.toolName }}</strong>
        <span class="dim"> {{ summary }}</span>
        <span v-if="node.durationMs != null" class="dim"> · {{ node.durationMs }}ms</span>
        <span v-if="node.isError" class="dim" style="color: var(--danger)"> · error</span>
        <span v-if="!node.finished" class="dim"> · running…</span>
      </button>
      <div v-if="open" class="ev-tool__body">
        <div class="ev-tool__section">
          <div class="label">arguments</div>
          <pre>{{ JSON.stringify(node.arguments, null, 2) }}</pre>
        </div>
        <div v-if="node.outputLines.length > 0" class="ev-tool__section">
          <div class="label">output</div>
          <pre>{{ node.outputLines.join('\\n') }}</pre>
        </div>
      </div>
    </div>
  `,
  setup(props) {
    const open = ref(false);
    const summary = computed(() => {
      const a = props.node.arguments;
      if (!a) return '';
      if (typeof a === 'string') return a.slice(0, 80);
      if (typeof a.command === 'string') return a.command.slice(0, 80);
      if (typeof a.path === 'string') return a.path;
      if (typeof a.code === 'string') return '(' + a.code.length + ' chars)';
      return Object.keys(a).slice(0, 3).join(', ');
    });
    return { open, summary };
  },
});
