import { defineComponent, onMounted } from 'vue';
import { useSkills } from '/composables/useSkills.js';

export default defineComponent({
  name: 'SkillPicker',
  props: { modelValue: { type: String, default: null } },
  emits: ['update:modelValue'],
  template: `
    <select :value="modelValue" @change="$emit('update:modelValue', $event.target.value || null)">
      <option value="">— none —</option>
      <option v-for="s in skills" :key="s.id" :value="s.id">{{ s.name }}</option>
    </select>
  `,
  setup() {
    const { skills, list } = useSkills();
    onMounted(() => list());
    return { skills };
  },
});
