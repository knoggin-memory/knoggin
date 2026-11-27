# Sign-in

Sign-in is one setting, `auth.mode`: `none` or `oauth`. With `oauth`, people and agents sign in through an identity provider that you choose; Knoggin itself has no passwords. It checks the provider's tokens and never sees a password.

## The two modes

| | `none` | `oauth` |
| --- | --- | --- |
| Default for | The local install | A server |
| Who uses it | Locally, only your own programs, through the Unix socket, or agents with an API key. On a server, **anyone who can reach the address**, as one user who is an admin | People who sign in at the provider, and agents with their tokens or an API key |
| What you set | Nothing | `server.public_url`, `auth.issuer`, `auth.web_client_id`, `auth.web_client_secret`, and the allow-list |

A server with sign-in off logs a warning at every start, and its web UI says that anyone with the address has access. A server that already has users who signed in refuses to start with sign-in off. Turn it back on, or remove those users first.

## What each credential reaches

- **Browser sign-in** (a session cookie): everything in the web UI for that person, including apps, keys and their account. Admins also see Users and Server status. A session ends after 7 days, after 24 hours without use, at sign-out, or when the person is removed.
- **An agent's token** from the provider: the memories of that person's personal store, to read and write. Nothing else: no apps, no keys, no account, no admin pages. A short-lived token held by an agent cannot make a long-lived key or delete an account.
- **An API key:** one store, read-only or read-write ([Apps and API keys](apps-and-keys.md)).

When an agent connects to `<public_url>/mcp` without a credential, Knoggin answers with the address of its protected-resource metadata. The agent finds your provider there and signs the person in by itself. That flow runs between the agent and the provider, and it ends back in the agent. Knoggin never shows a page of its own at the end of it.

## Setting up a server

In the config (or the matching variables, such as `KNOGGIN_AUTH__ISSUER`):

```toml
[server]
public_url = "https://memory.example.com"   # must be https

[auth]
mode = "oauth"
issuer = "https://auth.example.com"         # your provider's issuer, character for character
provider_name = "Example sign-in"           # shown on the sign-in button
web_client_id = "your-web-client-id"
web_client_secret_file = "/run/secrets/web_client_secret"
admin_subjects = ["user_01..."]             # who is an admin
allowed_email_domains = ["example.com"]     # or allowed_emails; who may use Knoggin at all
```

At the provider, register a **web application** (a confidential client) for the web UI, with the redirect address `<public_url>/auth/callback`. Its ID and secret go in the config. Access tokens must name this server as their audience: `<public_url>/mcp`, `<public_url>/mcp/plugin` or `<public_url>`, or a value you add to `auth.audiences`.

**Who may use it.** If `auth.allowed_emails` or `auth.allowed_email_domains` is set, only people whose provider-verified email matches may use Knoggin. A domain matches exactly; a subdomain does not. The list is checked on every request, so a change takes effect at once. With both lists empty, everyone your provider admits gets an account and a container, and the router warns about it at start.

**Admins.** A person is an admin when their subject is in `auth.admin_subjects`, or their verified email is in `auth.admin_emails`. Prefer subjects: a subject never changes hands. The check runs on every request, so taking someone out of the config takes effect at once. Nobody becomes an admin by signing in first. An admin can list and remove users and read the server status, but cannot read anyone's memories through Knoggin.

**Removing someone.** Remove them at the provider and in Knoggin (the Users page). To keep someone out for good, take them off the allow-list or remove them at the provider.

## Setting up AuthKit

WorkOS AuthKit is the first provider Knoggin was written and tested against. In the WorkOS dashboard:

1. Use an environment of its own for Knoggin. Under **Connect**, **Configuration**, turn on client ID metadata documents, so that agents can register themselves. Turn on dynamic client registration too if older agents must connect.
2. Add the resource indicators `<public_url>/mcp` and `<public_url>/mcp/plugin`, and make the first the default.
3. Create a confidential OAuth application for the web UI, with the redirect address `<public_url>/auth/callback`. Its client ID goes in `auth.web_client_id`, its secret in `auth.web_client_secret`.
4. Turn off open sign-up and invite people, or rely on Knoggin's allow-list.
5. Set `auth.issuer` to the environment's AuthKit address, such as `https://example-name-12.authkit.app`, and `auth.provider_name` to what the button should say.

**Knoggin's look on the sign-in page.** In AuthKit's branding settings:

- Logo: `ui/static/logo-horizontal.svg` for light backgrounds, `ui/static/logo-horizontal-white.svg` for dark ones. Icon: `ui/static/logo-mark.svg`. Convert to PNG if the dashboard asks for one.
- Colours: page background Cream `#FCF7EF`, buttons and links Forest `#2A442A`, text Chocolate `#372416`. The accent is Terracotta `#CE6B36`.

**Two things not yet confirmed against a real AuthKit environment**. The rules below cover both outcomes:

- **Pre-registered clients.** If AuthKit's tokens for a client you registered by hand carry no audience that names this deployment, such clients are not supported; use agents that register themselves. Never put a client ID that other applications share into `auth.audiences`: every token of that environment would then be accepted.
- **Email in access tokens.** An agent's token creates a user only when it carries a verified email. If AuthKit's access tokens carry none, each person signs in to the web UI once first; after that their agents connect. If the provider puts a verified email in a claim of its own, name it in `auth.email_claim` (and then do not use `auth.admin_emails`; the router refuses both together).

## Other providers

Any provider works that publishes an OpenID Connect discovery document and signs access tokens as JWTs (RS256 or ES256). It needs the same three things: its issuer, the audience it writes into tokens, and a web application for the UI. A provider whose access tokens are opaque strings is not supported.

To try sign-in without an account anywhere, run Keycloak in development mode on your machine and point a test deployment at it. Nothing in Knoggin depends on it.

## A local install with sign-in

A local install can be reached from a hosted chat client, such as claude.ai or ChatGPT, through a tunnel. Turn sign-in on only for that.

1. Start a tunnel that gives an `https` address and forwards to `127.0.0.1:7690`. The tunnel must not log request addresses.
2. In `~/.knoggin/config.toml`:

   ```toml
   [server]
   public_url = "https://my-tunnel.example.com"

   [auth]
   mode = "oauth"
   issuer = "https://auth.example.com"
   web_client_id = "your-web-client-id"
   web_client_secret_file = "/path/to/web_client_secret"
   allowed_emails = ["you@example.com"]
   ```

   The daemon refuses this mode without `auth.allowed_emails`, refuses `auth.allowed_email_domains` (a domain would let in everyone at it), and refuses a `public_url` that is not `https`. Keep the secret file readable by you alone (mode 0600).
3. `knoggin stop` and `knoggin start`.
4. Add the connector in the chat client with `<public_url>/mcp` ([claude.ai](../integrations/connectors/claude-ai/README.md), [ChatGPT](../integrations/connectors/chatgpt/README.md)).

Everyone who signs in acts as you, the one local user, and each identity is checked against the allow-list by its own verified email on every request. An agent's token must itself carry a verified email on the list; if your provider's tokens carry none, set `auth.email_claim` if it has such a claim, and otherwise give agents API keys instead. On your own machine nothing changes: `knoggin open` and the socket work as before. Keys can still be made only there, never from the tunnel's address.

## See also

- [Deploy on one host](deploy-one-host.md), [Deploy on AWS](deploy-aws.md)
- [Connecting agents](connect-agents.md)
