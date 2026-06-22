import { defineComponent, onMounted, watch } from 'vue';
import { useTriggers } from '/composables/useTriggers.js';

export default defineComponent({
  name: 'TriggerList',
  props: { agentId: { type: String, default: null } },
  template: `
    <div class="trigger-list">
      <div v-if="!agentId" class="picker__hint">Save the agent first to see triggers.</div>
      <div v-else-if="loading">loading…</div>
      <div v-else-if="webhooks.length === 0 && schedules.length === 0" class="picker__hint">
        No triggers configured. Add via <code>[[webhooks]]</code> / <code>[[schedules]]</code> in <code>simulacra.toml</code>.
      </div>
      <div v-else>
        <div v-for="w in webhooks" :key="w.path" class="trigger-row">
          <span class="dim">webhook</span> {{ w.path }}
          <span v-if="w.hmac" class="badge">HMAC</span>
        </div>
        <div v-for="s in schedules" :key="s.cron" class="trigger-row">
          <span class="dim">cron</span> {{ s.cron }} <span class="dim">({{ s.missed_policy }})</span>
        </div>
      </div>
    </div>
  `,
  setup(props) {
    const { webhooks, schedules, loading, list } = useTriggers();
    function refresh() { if (props.agentId) list(props.agentId); }
    onMounted(refresh);
    watch(() => props.agentId, refresh);
    return { webhooks, schedules, loading };
  },
});
