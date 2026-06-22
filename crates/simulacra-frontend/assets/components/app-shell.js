// app-shell.js — top nav + <router-view> outlet + global toast.
import { ref, h, defineComponent } from 'vue';

export const toasts = ref([]);

export function showToast(message, kind = 'error', timeoutMs = 5000) {
  const id = Math.random().toString(36).slice(2);
  toasts.value.push({ id, message, kind });
  setTimeout(() => {
    toasts.value = toasts.value.filter(t => t.id !== id);
  }, timeoutMs);
}

export default defineComponent({
  name: 'AppShell',
  template: `
    <div class="app">
      <header class="app__header">
        <strong>simulacra</strong>
        <nav>
          <router-link to="/">Agents</router-link>
        </nav>
      </header>
      <main class="app__main">
        <router-view />
      </main>
      <div class="toasts">
        <div v-for="t in toasts" :key="t.id" :class="['toast', 'toast--' + t.kind]">
          {{ t.message }}
        </div>
      </div>
    </div>
  `,
  setup() {
    return { toasts };
  },
});
