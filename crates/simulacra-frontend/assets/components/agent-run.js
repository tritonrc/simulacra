import { defineComponent, onMounted, onUnmounted, ref, watch, computed } from 'vue';
import { useTaskStream } from '/composables/useTaskStream.js';
import { useTaskArtifacts } from '/composables/useTaskArtifacts.js';
import { useAgents } from '/composables/useAgents.js';
import EventToken from '/components/activity/event-token.js';
import EventThinking from '/components/activity/event-thinking.js';
import EventToolCall from '/components/activity/event-tool-call.js';
import EventChild from '/components/activity/event-child.js';
import ArtifactSidebar from '/components/activity/artifact-sidebar.js';

// Real SSE event shapes (S031 §events):
//   { event: "agent.message", content, role }
//   { event: "agent.thinking", content }
//   { event: "tool.called", tool_call_id, tool_name, arguments }
//   { event: "tool.output", tool_call_id, line }
//   { event: "tool.result", tool_call_id, tool_name, duration_ms, is_error }
//   { event: "agent.child_spawned" / "child_finished", task_id, agent_type }
//   { event: "task.state_changed", from, to }
//
// The render layer collapses tool.{called,output,result} into a single
// virtual "tool" node keyed by tool_call_id so the user sees one block
// per invocation, not three.

function buildRenderable(events) {
  const toolBuckets = new Map(); // tool_call_id -> aggregated tool node
  const out = [];

  for (const ev of events) {
    const type = ev.event ?? ev.type;
    if (type === 'tool.called') {
      const node = {
        kind: 'tool',
        toolCallId: ev.tool_call_id,
        toolName: ev.tool_name,
        arguments: ev.arguments,
        outputLines: [],
        durationMs: null,
        isError: false,
        finished: false,
      };
      toolBuckets.set(ev.tool_call_id, node);
      out.push({ kind: 'tool', node, key: 'tool:' + ev.tool_call_id });
    } else if (type === 'tool.output') {
      const node = toolBuckets.get(ev.tool_call_id);
      if (node) node.outputLines.push(ev.line);
    } else if (type === 'tool.result') {
      const node = toolBuckets.get(ev.tool_call_id);
      if (node) {
        node.durationMs = ev.duration_ms;
        node.isError = !!ev.is_error;
        node.finished = true;
      }
    } else if (type === 'agent.message') {
      out.push({ kind: 'message', payload: ev, key: 'msg:' + ev.seq });
    } else if (type === 'agent.thinking') {
      out.push({ kind: 'thinking', payload: ev, key: 'think:' + ev.seq });
    } else if (type === 'agent.child_spawned' || type === 'agent.child_finished') {
      out.push({ kind: 'child', payload: ev, key: 'child:' + ev.seq });
    }
    // task.state_changed handled by status; not rendered as a feed entry
  }
  return out;
}

export default defineComponent({
  name: 'AgentRun',
  props: {
    id: { type: String, required: true },
    taskId: { type: String, required: true },
  },
  components: { EventToken, EventThinking, EventToolCall, EventChild, ArtifactSidebar },
  template: `
    <div class="agent-run">
      <div class="agent-run__crumbs">
        <router-link to="/">Agents</router-link>
        <span class="dim">/</span>
        <router-link :to="'/agents/' + id">{{ agentName || id }}</router-link>
        <span v-if="taskId === 'pending'" class="dim">/ run · (no task)</span>
        <span v-else class="dim">/ run · {{ taskId }} · {{ status }}</span>
      </div>
      <div v-if="taskId === 'pending'" class="agent-run__empty dim">
        No task yet — start one from the agent drawer.
      </div>
      <div v-else class="agent-run__grid">
        <section class="agent-run__feed">
          <div v-if="renderable.length === 0 && status === 'running'" class="dim">connecting…</div>
          <template v-for="entry in renderable" :key="entry.key">
            <event-token v-if="entry.kind === 'message'" :event="entry.payload" />
            <event-thinking v-else-if="entry.kind === 'thinking'" :event="entry.payload" />
            <event-tool-call v-else-if="entry.kind === 'tool'" :node="entry.node" />
            <event-child v-else-if="entry.kind === 'child'" :event="entry.payload" />
          </template>
          <div v-if="error" class="err">{{ error.message }}</div>
        </section>
        <artifact-sidebar :artifacts="artifacts" :task-id="taskId" />
      </div>
    </div>
  `,
  setup(props) {
    const { events, status, error, open, close } = useTaskStream();
    const { artifacts, refresh: refreshArtifacts } = useTaskArtifacts();
    const { get: getAgent } = useAgents();
    const agentName = ref(null);

    function startStream(taskId) {
      // The route accepts any string; "pending" is a legacy placeholder
      // value that should never be used to open a stream — render the
      // empty-state instead.
      if (!taskId || taskId === 'pending') return;
      open(taskId);
      refreshArtifacts(taskId);
    }

    async function loadAgentName(id) {
      if (!id) return;
      const agent = await getAgent(id);
      if (agent) agentName.value = agent.name;
    }

    onMounted(() => {
      loadAgentName(props.id);
      startStream(props.taskId);
    });
    onUnmounted(() => close());

    watch(() => props.id, (next, prev) => {
      if (next && next !== prev) loadAgentName(next);
    });

    watch(() => props.taskId, (next, prev) => {
      if (next && next !== prev) {
        close();
        startStream(next);
      }
    });

    watch(events, (next, prev) => {
      const prevLen = prev?.length ?? 0;
      for (let i = prevLen; i < next.length; i++) {
        const t = next[i].event ?? next[i].type;
        if (t === 'artifact.created' || t === 'artifact_written') {
          refreshArtifacts(props.taskId);
          break;
        }
      }
    });

    const renderable = computed(() => buildRenderable(events.value));

    return { events, status, error, artifacts, renderable, agentName };
  },
});
