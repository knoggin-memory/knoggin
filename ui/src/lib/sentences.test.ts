// SPDX-License-Identifier: AGPL-3.0-only
import { describe, expect, it } from 'vitest';
import { lockSentence, modelCause, signInError } from './sentences';
import { setupSteps } from './setup';
import { dayLabel, safeReturnTo, who } from './session.svelte';
import { resolveTheme } from './prefs.svelte';

describe('fixed sentences', () => {
  it('locked stores, server and local', () => {
    expect(lockSentence('master_key_missing', true).text).toBe('Knoggin cannot find the key that protects this store. Whoever runs this server must put the key back.');
    expect(lockSentence('format_too_new', false).text).toBe('This store was saved by a newer version of Knoggin. Upgrade Knoggin to open it.');
    expect(lockSentence('out_of_memory', true).text).toBe('This store is larger than the memory it is allowed. Whoever runs this server can raise the limit.');
    expect(lockSentence('state_unreadable', false).text).toBe('Knoggin’s own records could not be read. Stop Knoggin, then run knoggin state use-previous.');
  });
  it('ends with a link to the recovery guide where the sentence names it', () => {
    expect(lockSentence('state_missing', false)).toEqual({ text: 'Knoggin’s own records are missing, but your stores are still here. See the ', guide: true });
    expect(lockSentence('snapshot_unreadable', false).guide).toBe(true);
    expect(lockSentence('snapshot_unreadable', true).guide).toBe(false);
  });
  it('sign-in failures say install on a local install', () => {
    expect(signInError('not_allowed', true)).toBe('This account is not allowed to use this Knoggin server. Ask whoever runs it.');
    expect(signInError('account_removed', false)).toBe('This account was removed from this Knoggin install.');
    expect(signInError('state_mismatch', true)).toBe('This sign-in link has expired. Start again.');
  });
  it('one short cause for a model that is not available', () => {
    expect(modelCause('hash_mismatch')).toBe('The model files are missing or damaged. ');
    expect(modelCause('offline')).toBe('No connection. ');
  });
});

describe('names and dates', () => {
  it('falls back to the email, then "User" and 8 characters of the ID', () => {
    expect(who({ id: '0193b7c2-5e1a', name: 'Maya Okafor', email: 'm@x' })).toEqual({ text: 'Maya Okafor', initials: 'MO' });
    expect(who({ id: '0193b7c2-5e1a', name: null, email: 'r.costa@x' })).toEqual({ text: 'r.costa@x', initials: 'R' });
    expect(who({ id: '0193b7c2-5e1a', name: null, email: null })).toEqual({ text: 'User 0193b7c2', initials: 'U' });
  });
  it('shows today, yesterday or the date for a UTC date', () => {
    const now = new Date('2026-11-25T01:00:00Z');
    expect(dayLabel('2026-11-25', now)).toBe('today');
    expect(dayLabel('2026-11-24', now)).toBe('yesterday');
    expect(dayLabel('2026-11-02', now)).toBe('Nov 2, 2026');
    expect(dayLabel(null, now)).toBe('Never');
  });
  it('System follows the device', () => {
    expect(resolveTheme('system', true)).toBe('dark');
    expect(resolveTheme('system', false)).toBe('light');
    expect(resolveTheme('light', true)).toBe('light');
  });
});

describe('set-up steps', () => {
  const base = { app: null, addressForm: false, signInOn: false, publicUrl: 'https://m.example', mcpUrl: 'https://m.example/mcp', version: '0.1.0', keysElsewhere: false };
  it('claude.ai needs sign-in on a server and an internet address locally', () => {
    expect(setupSteps({ ...base, tab: 'claude-ai', addressForm: true })[0].text).toBe('claude.ai connects only when sign-in is on. Whoever runs this server can turn it on. ');
    expect(setupSteps({ ...base, tab: 'chatgpt' })[0].text).toMatch(/^ChatGPT needs an address it can reach from the internet, with sign-in on\./);
    expect(setupSteps({ ...base, tab: 'claude-ai', signInOn: true }).some((s) => s.code === 'https://m.example/mcp')).toBe(true);
  });
  it('the plugin tab gives the plugin address or the local command, and the pinned installer', () => {
    const server = setupSteps({ ...base, tab: 'plugin', addressForm: true });
    expect(server.map((s) => s.code)).toContain('https://m.example/mcp/plugin');
    expect(server.map((s) => s.code)).toContain('uvx knoggin@0.1.0 init --server https://m.example');
    expect(setupSteps({ ...base, tab: 'plugin' }).map((s) => s.code)).toContain('uvx knoggin@0.1.0 init');
  });
  it('an app never shows a key, and says where keys are made at a public address', () => {
    const steps = setupSteps({ ...base, tab: 'claude-code', app: 'Support bot', addressForm: true, keysElsewhere: true });
    expect(steps[0].keysElsewhere).toBe(true);
    expect(steps.every((s) => !s.code || !s.code.includes('knoggin_live_'))).toBe(true);
    expect(steps.map((s) => s.code)).toContain('KNOGGIN_BASE_URL=https://m.example\nKNOGGIN_API_KEY=<key>');
  });
});

describe('return_to', () => {
  it('follows a path on this site and nothing else', () => {
    for (const ok of ['/', '/explore', '/apps/x?tab=keys#a']) expect(safeReturnTo(ok)).toBe(ok);
    for (const bad of [null, '', 'map', '//evil.example', '/\\evil.example', '/a\\b', 'https://evil.example', '/a\nb', 'javascript:alert(1)']) expect(safeReturnTo(bad)).toBe('/map');
  });
});
