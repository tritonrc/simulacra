import { test } from 'node:test';
import assert from 'node:assert/strict';
let mockResponse;
globalThis.fetch = async () => ({ ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => mockResponse });
const { useTools } = await import('./useTools.js');

test('list() returns tools', async () => {
  mockResponse = { data: { availableTools: [{ id: 'shell:exec', name: 'shell', kind: 'shell', capabilities: ['shell:exec'] }] } };
  const { list, tools } = useTools();
  await list();
  assert.equal(tools.value.length, 1);
  assert.equal(tools.value[0].id, 'shell:exec');
});

test('list() keeps MCP server tools from availableTools', async () => {
  mockResponse = {
    data: {
      availableTools: [
        { id: 'mcp:fetcher', name: 'fetcher MCP', kind: 'MCP_SERVER', description: 'Configured MCP server fetcher.' },
      ],
    },
  };

  const { list, tools } = useTools();
  await list();

  assert.deepEqual(tools.value, [
    {
      id: 'mcp:fetcher',
      name: 'fetcher MCP',
      kind: 'MCP_SERVER',
      description: 'Configured MCP server fetcher.',
    },
  ]);
});
