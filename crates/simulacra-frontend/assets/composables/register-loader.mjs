// register-loader.mjs — used as `node --import ./register-loader.mjs` so the
// resolver in loader.mjs is active before any test module loads. Production
// (browser) ignores both files; the importmap in /index.html is the real
// equivalent.
import { register } from 'node:module';

register('./loader.mjs', import.meta.url);
