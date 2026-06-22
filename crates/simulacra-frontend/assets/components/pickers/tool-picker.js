import { defineComponent, onMounted, computed } from 'vue';
import { useTools } from '/composables/useTools.js';

export default defineComponent({
  name: 'ToolPicker',
  props: { modelValue: { type: Array, default: () => [] } },
  emits: ['update:modelValue'],
  template: `
    <div class="picker tool-picker">
      <div v-for="(group, kind) in groupedTools" :key="kind">
        <div class="label">{{ kind }}</div>
        <label v-for="tool in group" :key="tool.id" class="tool-picker__row">
          <input
            type="checkbox"
            :checked="modelValue.includes(tool.id)"
            @change="toggle(tool.id, $event.target.checked)"
          />
          {{ tool.name }}
        </label>
      </div>
    </div>
  `,
  setup(props, { emit }) {
    const { tools, list } = useTools();
    onMounted(() => list());
    const groupedTools = computed(() => {
      const groups = {};
      for (const t of tools.value) {
        groups[t.kind] = groups[t.kind] || [];
        groups[t.kind].push(t);
      }
      return groups;
    });
    function toggle(id, on) {
      const next = on
        ? [...props.modelValue, id]
        : props.modelValue.filter(x => x !== id);
      emit('update:modelValue', next);
    }
    return { groupedTools, toggle };
  },
});
