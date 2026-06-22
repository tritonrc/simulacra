// loader.mjs — Node ESM resolver hook for tests only.
//
// Production code in /assets uses absolute paths like `/api/graphql.js` that
// resolve via the importmap declared in /index.html. Node has no importmap,
// so we map root-absolute specifiers back to filesystem paths under /assets.
//
// Registered by `package.json` test script via --import.

import { fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, resolve as resolvePath } from 'node:path';

const HERE = dirname(fileURLToPath(import.meta.url));
const ASSETS_ROOT = resolvePath(HERE, '..');

export async function resolve(specifier, context, nextResolve) {
  if (specifier.startsWith('/') && !specifier.startsWith('//')) {
    const target = resolvePath(ASSETS_ROOT, specifier.replace(/^\/+/, ''));
    return nextResolve(pathToFileURL(target).href, context);
  }
  return nextResolve(specifier, context);
}
