// SPDX-License-Identifier: AGPL-3.0-only
// The fixed sentences of the tab "Web UI", word for word.

const GUIDE = 'See the recovery guide.';

const LOCK: Record<string, { server?: string; local?: string }> = {
  master_key_missing: {
    server: 'Knoggin cannot find the key that protects this store. Whoever runs this server must put the key back.',
    local: 'Knoggin cannot find the key that protects your memories. Put the key back, and Knoggin unlocks by itself within a minute.',
  },
  snapshot_unreadable: {
    server: 'The saved copies of this store could not be read. Nothing was overwritten. Whoever runs this server can bring back an earlier restore point.',
    local: 'The saved copies of this store could not be read. Nothing was overwritten. See the recovery guide.',
  },
  format_too_new: {
    server: 'This store was saved by a newer version of Knoggin. Whoever runs this server can upgrade it.',
    local: 'This store was saved by a newer version of Knoggin. Upgrade Knoggin to open it.',
  },
  out_of_memory: {
    server: 'This store is larger than the memory it is allowed. Whoever runs this server can raise the limit.',
  },
  state_unreadable: {
    local: 'Knoggin’s own records could not be read. Stop Knoggin, then run knoggin state use-previous.',
  },
  state_missing: {
    local: 'Knoggin’s own records are missing, but your stores are still here. See the recovery guide.',
  },
};

/** The one sentence for a locked store or install. When it ends with the recovery
 *  guide, `text` stops before "recovery guide." so the caller can make it a link. */
export function lockSentence(reason: string, server: boolean): { text: string; guide: boolean } {
  const r = LOCK[reason] ?? LOCK.master_key_missing;
  const t = (server ? (r.server ?? r.local) : (r.local ?? r.server)) as string;
  return t.endsWith(GUIDE) ? { text: t.slice(0, -'recovery guide.'.length), guide: true } : { text: t, guide: false };
}

/** One short cause for "The model is not available.", with a trailing space. */
export function modelCause(reason: string | null): string {
  switch (reason) {
    case 'offline':
      return 'No connection. ';
    case 'files_missing':
    case 'hash_mismatch':
      return 'The model files are missing or damaged. ';
    case 'disk_full':
      return 'The disk is full. ';
    case 'load_failed':
      return 'The model could not be loaded. ';
    default:
      return '';
  }
}

/** A failed sign-in (`/signin?error=<code>`). "server" becomes "install" on a local install. */
export function signInError(code: string, server: boolean): string {
  const where = server ? 'server' : 'install';
  switch (code) {
    case 'provider_error':
      return 'The sign-in service did not answer. Try again in a moment.';
    case 'not_allowed':
      return `This account is not allowed to use this Knoggin ${where}. Ask whoever runs it.`;
    case 'account_removed':
      return `This account was removed from this Knoggin ${where}.`;
    case 'email_unverified':
      return 'Verify your email address with your sign-in provider, then try again.';
    default:
      return 'This sign-in link has expired. Start again.';
  }
}

/** The pages that are not for this person. */
export const NOT_FOR_YOU = {
  member: 'This page is for admins of this server.',
  usersLocal: 'The local install has one user, so it has no Users page.',
  usersSignInOff: 'With sign-in off there is one user, so there is no Users page.',
  statusLocal: 'Server status belongs to a Knoggin server. The local install shows its system details under Health and in Settings.',
};

export const KEYS_ELSEWHERE = 'Keys are managed on the machine that runs Knoggin. Run knoggin open there.';

export const plural = (n: number, one: string, many: string) => (n === 1 ? one : many);
