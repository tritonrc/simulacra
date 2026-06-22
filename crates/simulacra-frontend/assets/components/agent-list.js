// agent-list.js — Card grid of agents with a drawer that opens to show
// composition. Drawer state is local; URL stays at "/".
import { defineComponent, ref, onMounted, computed } from 'vue';
import { useAgents } from '/composables/useAgents.js';
import { useTasks } from '/composables/useTasks.js';

export default defineComponent({
  name: 'AgentList',
  template: `
    <div class="agent-list">
      <div class="agent-list__header">
        <input v-model="filter" placeholder="filter…" class="agent-list__filter" />
        <router-link to="/agents/new"><button class="primary">+ New agent</button></router-link>
      </div>
      <div v-if="loading">Loading…</div>
      <div v-else-if="error">Failed to load: {{ error.message }}</div>
      <div v-else class="agent-list__grid">
        <div
          v-for="agent in filteredAgents"
          :key="agent.id"
          class="agent-card"
          @click="openDrawer(agent.id)"
        >
          <strong>{{ agent.name }}</strong>
          <div class="agent-card__meta">
            {{ (agent.tools || []).length }} tools · {{ formatRelative(agent.updatedAt) }}
          </div>
          <div class="agent-card__chips">
            <span v-for="ch in agent.channels" :key="ch.id" class="chip">{{ ch.name }}</span>
          </div>
        </div>
      </div>

      <aside v-if="drawerAgent" class="drawer" @click.self="closeDrawer">
        <div class="drawer__panel">
          <header class="drawer__header">
            <h2>{{ drawerAgent.name }}</h2>
            <div class="drawer__actions">
              <router-link :to="'/agents/' + drawerAgent.id"><button>Edit</button></router-link>
              <button @click="closeDrawer">×</button>
            </div>
          </header>
          <dl class="drawer__details">
            <dt>Channels</dt><dd>{{ (drawerAgent.channels||[]).map(c=>c.name).join(', ') || '—' }}</dd>
            <dt>Tools</dt><dd>{{ (drawerAgent.tools||[]).map(t=>t.name).join(', ') || '—' }}</dd>
            <dt>Skills</dt><dd>{{ (drawerAgent.skills||[]).map(s=>s.name).join(', ') || '—' }}</dd>
            <dt>Files</dt><dd>{{ (drawerAgent.files||[]).map(f=>f.name).join(', ') || '—' }}</dd>
            <dt>Capabilities</dt><dd>{{ (drawerAgent.capabilities||[]).join(', ') || '—' }}</dd>
            <dt>Prompt</dt><dd><pre>{{ drawerAgent.systemPrompt }}</pre></dd>
          </dl>

          <div class="drawer__run">
            <div class="label">What should this agent do?</div>
            <div v-if="runError" class="err drawer__run-error">{{ runError }}</div>
            <textarea
              v-model="prompt"
              rows="4"
              placeholder="describe the task…"
              :disabled="running"
            ></textarea>
            <div class="drawer__run-actions">
              <button
                class="primary"
                @click="runAgent(drawerAgent)"
                :disabled="running || !prompt.trim()"
              >{{ running ? 'Starting…' : '▶ Run' }}</button>
            </div>
          </div>
        </div>
      </aside>
    </div>
  `,
  setup() {
    const { agents, loading, error, list, get } = useAgents();
    const { createTask } = useTasks();
    const filter = ref('');
    const drawerAgent = ref(null);
    const prompt = ref('');
    const runError = ref(null);
    const running = ref(false);

    onMounted(() => list());

    const filteredAgents = computed(() => {
      const q = filter.value.toLowerCase();
      if (!q) return agents.value;
      return agents.value.filter(a => a.name.toLowerCase().includes(q));
    });

    async function openDrawer(id) {
      // Reset transient run state whenever a new drawer opens.
      prompt.value = '';
      runError.value = null;
      running.value = false;
      drawerAgent.value = await get(id);
    }
    function closeDrawer() {
      drawerAgent.value = null;
      prompt.value = '';
      runError.value = null;
      running.value = false;
    }

    async function runAgent(agent) {
      if (!agent || running.value) return;
      const taskText = prompt.value.trim();
      if (!taskText) {
        runError.value = 'Enter a task description before running.';
        return;
      }
      running.value = true;
      runError.value = null;
      // The GraphQL `Agent` type exposes `name` as the canonical identifier
      // used by the runtime as `agent_type` (see useAgents.saveAndRun).
      const agentType = agent.type || agent.agentType || agent.name;
      try {
        const { taskId } = await createTask({ agentType, task: taskText });
        window.location.hash = `#/agents/${agent.id}/run/${taskId}`;
      } catch (e) {
        runError.value = e.message || String(e);
      } finally {
        running.value = false;
      }
    }

    return {
      agents, loading, error,
      filter, filteredAgents,
      drawerAgent, openDrawer, closeDrawer,
      prompt, runError, running, runAgent,
      formatRelative,
    };
  },
});

function formatRelative(iso) {
  if (!iso) return 'unknown';
  const ms = Date.now() - new Date(iso).getTime();
  const days = Math.round(ms / 86_400_000);
  if (days < 1) return 'today';
  if (days === 1) return '1d ago';
  if (days < 30) return `${days}d ago`;
  return new Date(iso).toLocaleDateString();
}
