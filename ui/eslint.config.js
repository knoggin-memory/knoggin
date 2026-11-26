// SPDX-License-Identifier: AGPL-3.0-only
import js from '@eslint/js';
import svelte from 'eslint-plugin-svelte';
import globals from 'globals';
import ts from 'typescript-eslint';

export default ts.config(
  { ignores: ['dist/', '.svelte-kit/', 'test-results/', 'playwright-report/', 'node_modules/'] },
  js.configs.recommended,
  ...ts.configs.recommended,
  ...svelte.configs.recommended,
  {
    languageOptions: { globals: { ...globals.browser, ...globals.node } },
    rules: {
      // Stored strings are always text (tab "Security"): no {@html} anywhere.
      'svelte/no-at-html-tags': 'error',
      'svelte/require-each-key': 'off',
      'svelte/no-navigation-without-resolve': 'off',
      // The ported screens read a value on its own line to make an effect depend on it,
      // and keep plain Maps and Sets where no one renders from them.
      '@typescript-eslint/no-unused-expressions': 'off',
      'svelte/prefer-svelte-reactivity': 'off',
      '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_', varsIgnorePattern: '^_' }],
    },
  },
  {
    files: ['**/*.svelte', '**/*.svelte.ts'],
    languageOptions: { parserOptions: { projectService: true, extraFileExtensions: ['.svelte'], parser: ts.parser } },
  },
);
