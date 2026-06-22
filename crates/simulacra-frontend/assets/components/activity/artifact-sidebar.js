import { defineComponent } from 'vue';

export default defineComponent({
  name: 'ArtifactSidebar',
  props: {
    artifacts: { type: Array, required: true },
    taskId: { type: String, required: true },
  },
  template: `
    <aside class="artifacts">
      <div class="label">Artifacts</div>
      <div v-if="artifacts.length === 0" class="dim">none yet</div>
      <a
        v-for="a in artifacts"
        :key="a.path"
        class="artifacts__row"
        :href="downloadHref(a.path)"
        target="_blank"
        download
      >
        <span>📄 {{ a.path }}</span>
        <span class="dim">{{ formatSize(a.size) }} · ⬇</span>
      </a>
    </aside>
  `,
  methods: {
    downloadHref(path) { return `/api/v1/tasks/${this.taskId}/artifacts/${path}`; },
    formatSize(b) {
      if (b == null) return '';
      if (b < 1024) return `${b}B`;
      if (b < 1024 * 1024) return `${(b/1024).toFixed(1)}KB`;
      return `${(b/1024/1024).toFixed(1)}MB`;
    },
  },
});
