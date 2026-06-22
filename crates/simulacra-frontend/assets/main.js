// main.js — Vue app entry. vue-router in hash mode.
import { createApp, defineAsyncComponent } from 'vue';
import { createRouter, createWebHashHistory } from 'vue-router';

import AppShell from '/components/app-shell.js';

const routes = [
  { path: '/', component: defineAsyncComponent(() => import('/components/agent-list.js')) },
  { path: '/agents/new', component: defineAsyncComponent(() => import('/components/agent-form.js')) },
  { path: '/agents/:id', component: defineAsyncComponent(() => import('/components/agent-form.js')), props: true },
  { path: '/agents/:id/run/:taskId', component: defineAsyncComponent(() => import('/components/agent-run.js')), props: true },
];

const router = createRouter({
  history: createWebHashHistory(),
  routes,
});

createApp(AppShell).use(router).mount('#app');
