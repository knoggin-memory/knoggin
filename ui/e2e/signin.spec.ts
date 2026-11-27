// SPDX-License-Identifier: AGPL-3.0-only
// Sign-in on, against the test issuer: a daemon in mode oauth, in test
// mode, whose server.public_url is its own fixed listener's loopback address. Sign in, see
// the profile, sign out, return_to, and each callback error. This file runs its own daemon
// and issuer; the issuer approves at once and signs in `login_hint`, else alice.
import { expect, test, type Page } from '@playwright/test';
import type { ChildProcess } from 'node:child_process';
import { cleanup, freePort, makeHome, start, startIssuer, type DaemonInfo } from './support/daemon';

test.describe.configure({ mode: 'serial' });

const ALICE = { name: 'alice', email: 'alice@test.example' };
let d: DaemonInfo;
let issuer: { url: string; process: ChildProcess } | undefined;
let pub: string;

test.beforeAll(async () => {
  d = { bin: process.env.KNOGGIN_BIN!, home: makeHome() };
  try {
    issuer = await startIssuer(d);
    const port = await freePort();
    pub = `http://127.0.0.1:${port}`;
    d.extra = {
      KNOGGIN_SERVER__BIND: `127.0.0.1:${port}`,
      KNOGGIN_SERVER__PUBLIC_URL: pub,
      KNOGGIN_AUTH__MODE: 'oauth',
      KNOGGIN_AUTH__ISSUER: issuer.url,
      KNOGGIN_AUTH__PROVIDER_NAME: 'Test issuer',
      KNOGGIN_AUTH__WEB_CLIENT_ID: 'knoggin-web',
      KNOGGIN_AUTH__WEB_CLIENT_SECRET: 'test-secret',
      // carol-unverified is listed, so her refusal comes from the unverified email, not the list.
      KNOGGIN_AUTH__ALLOWED_EMAILS: `${ALICE.email},carol-unverified@test.example`,
    };
    await start(d);
  } catch (e) {
    cleanup(d);
    issuer?.process.kill('SIGTERM');
    throw e;
  }
});

test.afterAll(() => {
  cleanup(d);
  issuer?.process.kill('SIGTERM');
});

/** Starts a sign-in at the server as the issuer's person `who`, as a link with a hint would. */
const signInAs = (page: Page, who: string, returnTo = '/map') => page.goto(`${pub}/auth/login?return_to=${encodeURIComponent(returnTo)}&login_hint=${who}`);

async function failedWith(page: Page, code: string, sentence: string, tryAgain = true) {
  await expect(page).toHaveURL(`${pub}/signin?error=${code}`);
  await expect(page.getByRole('heading', { name: 'Sign-in did not finish' })).toBeVisible();
  await expect(page.getByText(sentence, { exact: true })).toBeVisible();
  await expect(page.getByRole('link', { name: 'Sign in', exact: true })).toHaveCount(tryAgain ? 1 : 0);
  await expect(page.getByRole('link', { name: 'Source code' })).toBeVisible();
  await expect(page.getByText(/@test\.example/)).toHaveCount(0);
}

test('sign in, see the profile, sign out', async ({ page, context }) => {
  // A signed-out browser at the public address goes to the sign-in page, with where it was going.
  await page.goto(`${pub}/explore`);
  await expect(page).toHaveURL(`${pub}/signin?return_to=%2Fexplore`);
  await expect(page.getByRole('heading', { name: 'Sign in to Knoggin' })).toBeVisible();
  await expect(page.getByText('Sign-in is handled by Test issuer. Knoggin never sees your password.')).toBeVisible();
  await expect(page.getByRole('link', { name: 'Source code' })).toBeVisible();

  await page.getByRole('button', { name: 'Continue to sign in' }).click();
  // Back from the issuer, at the page it started from.
  await expect(page).toHaveURL(`${pub}/explore`);
  await expect(page.getByText(/^(daemon )?connected$/)).toBeVisible();
  const cookies = await context.cookies();
  expect(cookies.map((c) => c.name)).toEqual(['knoggin_session']);
  expect(cookies[0]).toMatchObject({ httpOnly: true, sameSite: 'Lax', path: '/', secure: false });
  expect(await page.evaluate(() => localStorage.getItem('knoggin.sessionToken'))).toBeNull();

  // The sidebar names who is signed in; its link opens the Account page.
  await page.getByRole('navigation', { name: 'Manage' }).getByRole('link', { name: ALICE.name, exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Account' })).toBeVisible();
  const profile = page.getByRole('region', { name: 'Profile' });
  await expect(profile.getByText(ALICE.name, { exact: true })).toBeVisible();
  await expect(profile.getByText(ALICE.email, { exact: true })).toBeVisible();
  await expect(profile.getByText('Profile from Test issuer')).toBeVisible();

  await profile.getByRole('button', { name: 'Sign out' }).click();
  await expect(page).toHaveURL(`${pub}/signin?reason=signed_out`);
  await expect(page.getByRole('heading', { name: 'You are signed out' })).toBeVisible();
  expect(await context.cookies()).toEqual([]);
  // The session is gone on the server too: the app sends the browser back to sign in.
  await page.goto(`${pub}/account`);
  await expect(page).toHaveURL(`${pub}/signin?return_to=%2Faccount`);
  await expect(page.getByRole('button', { name: 'Continue to sign in' })).toBeVisible();
});

test('return_to: only a path on this site is followed, by the server and by the page', async ({ page }) => {
  for (const bad of ['//evil.example/map', '/\\evil.example', 'https://evil.example/']) {
    await page.context().clearCookies();
    await signInAs(page, 'alice', bad);
    await expect(page).toHaveURL(`${pub}/map`);
  }
  // Signed in, the sign-in page goes on to its return_to, but never off the site.
  await page.goto(`${pub}/signin?return_to=%2Fhealth`);
  await expect(page).toHaveURL(`${pub}/health`);
  await page.goto(`${pub}/signin?return_to=%2F%2Fevil.example`);
  await expect(page).toHaveURL(`${pub}/map`);
});

test('each callback error shows its sentence on the sign-in page', async ({ page }) => {
  // state_mismatch: a callback without the login cookie.
  await page.goto(`${pub}/auth/callback?code=x&state=y`);
  await failedWith(page, 'state_mismatch', 'This sign-in link has expired. Start again.');

  // provider_error: the provider answers the sign-in with an error. Playwright cannot route the
  // redirect to the issuer, so the login's own answer (and its login cookie) is kept and its
  // Location is replaced by the callback the provider would send.
  await page.route(`${pub}/auth/login?**`, async (r) => {
    const login = await r.fetch({ maxRedirects: 0 });
    const state = new URL(login.headers()['location']).searchParams.get('state') ?? '';
    await r.fulfill({ response: login, headers: { ...login.headers(), location: `${pub}/auth/callback?error=access_denied&state=${encodeURIComponent(state)}` } });
  });
  await page.goto(`${pub}/signin`);
  await page.getByRole('button', { name: 'Continue to sign in' }).click();
  await failedWith(page, 'provider_error', 'The sign-in service did not answer. Try again in a moment.');
  await page.unrouteAll();

  // not_allowed: a verified email that is not on the allow-list ("install" on a local install).
  await signInAs(page, 'mallory');
  await failedWith(page, 'not_allowed', 'This account is not allowed to use this Knoggin install. Ask whoever runs it.');

  // email_unverified: the issuer marks a subject ending in -unverified as unverified.
  await signInAs(page, 'carol-unverified');
  await failedWith(page, 'email_unverified', 'Verify your email address with your sign-in provider, then try again.');

  // account_removed: a local install has one user, whom no admin can remove, so the daemon's
  // callback never sends it; the page it would land on has the sentence and no Sign in button.
  await page.goto(`${pub}/signin?error=account_removed`);
  await failedWith(page, 'account_removed', 'This account was removed from this Knoggin install.', false);

  // A failure page's Sign in tries again.
  await page.goto(`${pub}/auth/callback?code=x&state=y`);
  await page.getByRole('link', { name: 'Sign in', exact: true }).click();
  await expect(page).toHaveURL(`${pub}/map`);
  await expect(page.getByText(/^(daemon )?connected$/)).toBeVisible();
});
