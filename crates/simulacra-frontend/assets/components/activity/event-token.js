import { defineComponent } from 'vue';

export default defineComponent({
  name: 'EventToken',
  props: { event: { type: Object, required: true } },
  template: `<div class="ev-token">{{ event.content || event.text }}</div>`,
});
