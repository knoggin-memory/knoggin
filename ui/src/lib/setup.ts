// SPDX-License-Identifier: AGPL-3.0-only
// The steps of "Set up an agent". The words are the prototype's; the commands for
// each agent are its examples until integrations/connectors is written (milestone 8).

export const TABS = [
  { id: 'claude-code', name: 'Claude Code' },
  { id: 'claude-ai', name: 'claude.ai' },
  { id: 'codex', name: 'Codex' },
  { id: 'cursor', name: 'Cursor' },
  { id: 'chatgpt', name: 'ChatGPT' },
  { id: 'plugin', name: 'Plugin' },
] as const;
export type Tab = (typeof TABS)[number]['id'];

export interface Step {
  text: string;
  code?: string;
  note?: string;
  /** A guide link after the text: "Sign-in guide", "sign-in guide" or "plugin guide". */
  link?: string;
  linkAfter?: string;
  /** "Open API keys" under the step. */
  keysLink?: boolean;
  /** The one sentence for a cookie session at a local install's public address. */
  keysElsewhere?: boolean;
}

export interface SetupInput {
  tab: Tab;
  /** The app's name, or null for the personal store. */
  app: string | null;
  /** The address form: a server, or a cookie session at a local install's public address. */
  addressForm: boolean;
  signInOn: boolean;
  publicUrl: string;
  mcpUrl: string;
  version: string;
  keysElsewhere: boolean;
}

const localEntry = '{\n  "mcpServers": {\n    "knoggin": { "command": "knoggin", "args": ["mcp"] }\n  }\n}';

export function setupSteps(i: SetupInput): Step[] {
  const sv = i.addressForm, on = i.signInOn, M = i.mcpUrl, B = i.publicUrl;
  const out: Step[] = [];
  const s = (text: string, code?: string, note?: string) => out.push({ text, code, note });
  // claude.ai and ChatGPT connect from the internet and sign in with OAuth.
  const hosted = (who: string, steps: () => void) => {
    if (!on && sv) out.push({ text: `${who} connects only when sign-in is on. Whoever runs this server can turn it on. `, link: 'Sign-in guide', linkAfter: '' });
    else if (!on) out.push({ text: `${who} needs an address it can reach from the internet, with sign-in on. That is a Knoggin server, or this install behind a tunnel. See the `, link: 'sign-in guide', linkAfter: '.' });
    else steps();
  };

  if (i.app !== null) {
    if (i.keysElsewhere) out.push({ text: '', keysElsewhere: true });
    else out.push({ text: `Create an API key for ${i.app}. Leave Write off if the agent should only recall.`, keysLink: true });
    if (sv) {
      s('For the Python SDK, give the agent the API address and the key.', `KNOGGIN_BASE_URL=${B}\nKNOGGIN_API_KEY=<key>`, 'The SDK reads these two variables.');
      s('For an MCP client, use the MCP address.', M, 'The address is the same for every store. The key selects this app.');
      s('Send the key as a header.', 'Authorization: Bearer $KNOGGIN_API_KEY');
    } else {
      s('For the Python SDK, give the agent the key. On this machine no address is needed.', 'KNOGGIN_API_KEY=<key>', 'The SDK reads this variable.');
      s('For an MCP client, add the local bridge.', localEntry, 'Set KNOGGIN_API_KEY in the environment that starts the agent. On this machine no address is needed. The key selects this app.');
    }
    return out;
  }

  switch (i.tab) {
    case 'claude-code':
      if (!sv) s('Add Knoggin to Claude Code.', 'claude mcp add knoggin -- knoggin mcp', 'Claude Code starts Knoggin on this machine when it needs it.');
      else {
        s('Add the server to Claude Code.', `claude mcp add --transport http knoggin ${M}`);
        if (on) s('In Claude Code, run /mcp, choose knoggin and sign in.', undefined, 'Your browser opens to sign in.');
      }
      break;
    case 'claude-ai':
      hosted('claude.ai', () => {
        s('In Claude, open Customize, then Connectors, and choose Add custom connector.');
        s('Paste the server address and choose Connect.', M);
        s('Sign in when the browser asks.');
        // The user's own instruction to their assistant; Knoggin itself sends no such instruction.
        s('Optional. Add this line to your personal preferences in claude.ai:', 'At the start of each conversation, use Knoggin to load relevant context.');
      });
      break;
    case 'codex':
      if (!sv) s('Add Knoggin to Codex.', 'codex mcp add knoggin -- knoggin mcp');
      else {
        s('Add the server to Codex.', `codex mcp add knoggin --url ${M}`);
        if (on) s('Sign in.', 'codex mcp login knoggin');
      }
      break;
    case 'cursor':
      s('Add this to .cursor/mcp.json in a project, or to the global file.', sv ? `{\n  "mcpServers": {\n    "knoggin": { "url": "${M}" }\n  }\n}` : localEntry);
      if (sv && on) s('Open Cursor settings, then MCP, and sign in to knoggin.');
      break;
    case 'chatgpt':
      hosted('ChatGPT', () => {
        s('Turn on developer mode under Settings, then Connectors.', undefined, 'Reported to need a Business, Enterprise or Edu plan.');
        s('Add a connector with the server address.', M);
        s('Sign in when asked.');
      });
      break;
    case 'plugin':
      out.push({
        text: 'Install the Knoggin plugin. The ',
        link: 'plugin guide',
        linkAfter: ' has the steps for Claude and for Codex.',
        note: 'It adds the connector and a memory skill. In Claude, the plugin adds /remember and /recall.',
      });
      if (sv) s('If the plugin asks for an address, give it this one:', `${M}/plugin`);
      else s('It connects through knoggin mcp --tool-set plugin. There is nothing to enter.');
      s(
        'Or let the installer set up the agents it finds on this machine.',
        sv ? `uvx knoggin@${i.version} init --server ${B}` : `uvx knoggin@${i.version} init`,
        'It shows each change and asks before making it.',
      );
      break;
  }
  return out;
}
