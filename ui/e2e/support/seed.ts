// SPDX-License-Identifier: AGPL-3.0-only
// The data the end-to-end tests read: memories in two namespaces of the
// personal store, one saved over MCP by a client whose name holds markup, a
// recall and a reinforcement for the Timeline, and two apps.
import { api, type DaemonInfo } from './daemon';

export const MARKUP = {
  memory: 'Markup test <img src=x onerror="window.__xss=1"> <b>not bold</b>',
  client: '<b>bot</b><script>window.__xss=1</script>',
  app: '<i>Ops</i> & <script>window.__xss=1</script>',
};
export const PERSONAL = ['Maya prefers window seats on long flights', 'The Lisbon office opens at nine', 'Ferry schedules change on Thursdays'];
export const HARBOR = ['Fares are stored in cents, never as floats', 'Staging deploys on every merge to main'];
export const SUPPORT = 'Refund requests go to a person after two messages';

interface Created {
  id: string;
}

async function mcpStore(d: DaemonInfo, store: string | null, summary: string, client: string) {
  const h = { Accept: 'application/json, text/event-stream', ...(store ? { 'Knoggin-Store': store } : {}) };
  const init = await api<unknown>(d, 'POST', '/mcp', {
    jsonrpc: '2.0',
    id: 1,
    method: 'initialize',
    params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: client, version: '1' } },
  }, h);
  const sid = String(init.headers['mcp-session-id']);
  const s = { ...h, 'Mcp-Session-Id': sid, 'MCP-Protocol-Version': '2025-06-18' };
  await api(d, 'POST', '/mcp', { jsonrpc: '2.0', method: 'notifications/initialized' }, s);
  await api(d, 'POST', '/mcp', { jsonrpc: '2.0', id: 2, method: 'tools/call', params: { name: 'store_memory', arguments: { summary } } }, s);
}

export async function seed(d: DaemonInfo) {
  const first = await api<Created>(d, 'POST', '/v1/memories', { summary: PERSONAL[0], fullText: 'Said so while booking the Lisbon trip.', tags: ['travel'], entities: ['Maya'] });
  await api(d, 'POST', '/v1/memories', { summary: PERSONAL[1], parentId: first.body.id, entities: ['Lisbon'] });
  await api(d, 'POST', '/v1/memories', { summary: PERSONAL[2], tags: ['travel'] });
  await api(d, 'POST', '/v1/namespaces', { name: 'project-harbor' });
  for (const s of HARBOR) await api(d, 'POST', '/v1/memories', { summary: s, namespace: 'project-harbor', tags: ['harbor'] });
  await mcpStore(d, null, MARKUP.memory, MARKUP.client);
  await api(d, 'POST', '/v1/recall', { query: 'window seats', recordAccess: true });
  await api(d, 'POST', `/v1/memories/${first.body.id}/reinforce`, { quality: 3 });

  const app = await api<Created>(d, 'POST', '/v1/stores', { name: 'Support bot' });
  await api(d, 'POST', '/v1/memories', { summary: SUPPORT }, { 'Knoggin-Store': app.body.id });
  await api(d, 'POST', '/v1/stores', { name: MARKUP.app });
}
