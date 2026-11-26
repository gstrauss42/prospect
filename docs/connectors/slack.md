# Slack connector: setup and security review

This page is for the administrator who sets up Slack for Opportunity, and for the security
reviewer who has to sign it off. It says exactly what the connector reads, how "read-only" is
enforced, and how to install it.

## In short

* Each person connects their own Slack account. Opportunity then reads, with that person's own
  **user token**, recent messages that look like opportunities (an internal hackathon, a call for
  speakers, a request for an intro, an internal role, a partnership ask). It can only see what that
  person can already see in Slack.
* It asks Slack for two read permissions and nothing else: `search:read` and `users:read`. There is
  no bot user, and Opportunity never asks for a bot token.
* It uses three Slack methods, all with HTTP GET: `auth.test`, `search.messages` and `users.info`.
  The connector code has no way to call any other method: no posting, reacting, editing, deleting,
  uploading, inviting or revoking.
* Every reply from Slack says what the token can really do, in its `x-oauth-scopes` header.
  Opportunity checks that header on **every call**. If the token can do anything more than read, it
  stops before using the reply, and the connection has to be made again.
* Message text is fetched when a search for opportunities runs, trimmed, used and then dropped. It is
  never stored. Tokens are stored encrypted and never reach the browser.

## What is read, exactly

| Slack method | HTTP | Scope it needs | Why | What is kept from the reply |
|---|---|---|---|---|
| [`auth.test`](https://docs.slack.dev/reference/methods/auth.test) | GET | none | Checks the token works, says who and which workspace it is, and reports the token's scopes | Workspace id, name and URL, and the person's Slack handle. Their member id is used for the mention search |
| [`search.messages`](https://docs.slack.dev/reference/methods/search.messages) | GET | `search:read` | Finds recent messages that look like opportunities | Per message: its text (trimmed to 1,200 characters), the channel name, the author, the date and the permalink |
| [`users.info`](https://docs.slack.dev/reference/methods/users.info) | GET | `users:read` | Turns the member ids of authors and of people mentioned into names, so "ping @Priya Natarajan" stays meaningful | The person's real name or display name. Email addresses are not requested (`users:read.email` isn't asked for) |

Plus the token endpoint, [`oauth.v2.access`](https://docs.slack.dev/reference/methods/oauth.v2.access)
(POST), which turns the sign-in code into a token and, when token rotation is on, renews it. It is an
identity endpoint and can't change anything in the workspace.

**Which conversations.** Slack search with a user token covers what that person can see: public
channels, private channels they belong to, and their direct and group messages. It also follows the
search filters the person has set in Slack. Opportunity doesn't widen that, and it can't see
anything the person can't.

**How much.** One search run makes:

* one `auth.test`;
* at most 9 searches, of up to 25 results each, limited to the period being looked at (30 days by
  default) with Slack's `after:` modifier:
  * six searches for one strong opportunity word each: `hackathon`, `"call for"`, `hiring`,
    `speaker`, `intro`, `partnership`;
  * two searches that join further words with `OR` (for example
    `opportunity OR secondment OR applications OR …`);
  * one for messages that mention the person (`<@their member id>`);
* at most 20 `users.info` lookups.

The results are de-duplicated, ranked (by how strongly they suggest an opportunity, whether they
mention the person or are direct messages, and how recent they are) and capped, 60 by default. The
words come from Opportunity's list of opportunity markers; the server can pass its own. Either way
they are cleaned, so that they can never become search modifiers such as `in:` or `from:`.

**Where the text goes.** The trimmed messages are given to the AI model that this deployment of
Opportunity is configured to use, which judges which ones are real opportunities for the person.
Only the opportunities it finds are stored, with a link back to the Slack message. The model only
ever receives text. It has no tools and can't make any request to Slack or anywhere else.

**When.** Only while a search for opportunities runs, on the person's schedule or when they ask for
one. There are no event subscriptions, no webhooks and no background polling. Slack never sends
Opportunity anything.

## Scopes

Requested (user scopes only):

| Scope | Allows | Used for |
|---|---|---|
| `search:read` | Searching the workspace's content as the person | `search.messages` (the scope also covers file search; Opportunity doesn't use it) |
| `users:read` | Viewing people in the workspace | `users.info`, for names |

Opportunity doesn't request bot scopes, write scopes, or history scopes such as
`channels:history`. Search is enough, and those scopes would allow bulk reading.

A token is refused if it carries any scope outside a short list of read-only scopes:
`search:read`, `users:read`, `users:read.email`, `team:read`, `channels:read`, `groups:read`,
`im:read`, `mpim:read`, `channels:history`, `groups:history`, `im:history`, `mpim:history` and
`identity.basic`. Every scope is also checked against a hard-coded list of write markers (`write`,
`post`, `admin`, `commands`, `incoming-webhook` and others). That check wins over the read-only list,
whatever the list says.

## How read-only is enforced

Four independent layers. Any one of them would stop a write on its own.

1. **The token can't write.** The Slack app asks for two read scopes, has no bot user, and asks for
   no write scope. Slack itself refuses any method the token's scopes don't cover.
2. **What the token can do is checked, every time.**
   * When someone connects, the granted scopes are checked. Then, on every Web API call, the
     `x-oauth-scopes` header (the scopes Slack says the token really has) is checked before the reply
     is used. If a token gains `chat:write` or anything else that isn't on the read-only list, for
     example because someone added a scope to the app and the person approved it again, its very
     next call fails. The connection is suspended until the person reconnects with read-only access.
   * A successful reply without that header is treated as suspicious and not used. This can happen
     behind a proxy that strips headers.
   * Only user tokens are accepted. Bot tokens (`xoxb-`), app-level tokens (`xapp-`), rotating
     tokens pasted by hand (`xoxe.`), refresh tokens, browser-session and legacy tokens are refused
     before they are sent anywhere. A legacy all-access token (`identify`, `read`, `post`, `client`)
     is refused by its scopes. If `auth.test` says the token belongs to a bot, it is refused.
   * If a sign-in somehow also returns a bot token (only possible when someone has added bot scopes
     to the app), the bot token is discarded on the spot, and the connection is refused if those
     scopes can write.
3. **There is no code path that writes.** All Slack data is read through one gateway, whose only
   network method is HTTP GET. It allows exactly three URLs: `/api/auth.test`,
   `/api/search.messages` and `/api/users.info` on `https://slack.com`. This matters because Slack
   accepts GET even for methods that change things. A GET to `chat.postMessage`, `reactions.add`,
   `auth.revoke`, `conversations.history` or any other method is refused before anything is sent.
   Redirects are never followed. Headers that could change the method are refused. The only other
   request the connector can make is a POST to Slack's token endpoint, `oauth.v2.access`. Every
   request, every request the gateway refuses, and every stored connection refused for its scopes
   is written to an audit log as metadata only: method, endpoint, status, size and time. The query
   string is never recorded, so the log holds no tokens, no search words and no message content.
4. **The AI never gets tools.** The connector is called by ordinary server code. The AI model only
   receives text, and can't cause any request.

How this is tested: the automated tests run the connector against a Slack stand-in that records any
request other than a GET to the three read methods as a violation. Every test checks that this
record is empty, and that the audit log holds only GET reads and token-endpoint POSTs. They cover
signing in, pasted tokens, refused token types, tokens that have write scopes at connect time or
gain them later (including in the middle of a search run), revoked tokens, token renewal and rate
limits. See `crates/connectors/tests/slack.rs`.

## Setting it up

### 1. Create the Slack app

At <https://api.slack.com/apps>, choose **Create New App → From a manifest**, pick the workspace,
and paste the manifest below. Replace `https://opportunity.example.com` with your Opportunity
server's `PUBLIC_URL`. In production this must be `https`. The redirect URL must be exactly
`{PUBLIC_URL}/api/connectors/slack/callback`.

```yaml
display_information:
  name: Opportunity (read-only)
  description: Finds opportunities in Slack messages you can already see. It can only read.
oauth_config:
  redirect_urls:
    - https://opportunity.example.com/api/connectors/slack/callback
  scopes:
    user:
      - search:read
      - users:read
settings:
  org_deploy_enabled: false
  socket_mode_enabled: false
  token_rotation_enabled: true
```

What the manifest leaves out matters as much as what it has. There is no `features` section (so no
bot user, slash commands or app home), no `bot` scopes, no event subscriptions, no interactivity and
no incoming webhooks.

Recommended settings, all under **OAuth & Permissions**:

* **Token rotation** (`token_rotation_enabled: true`, as above). Access tokens then expire after 12
  hours and are renewed automatically with a single-use refresh token, so a leaked access token is
  short-lived. Slack doesn't let rotation be turned off once it's on. Pasted tokens (see below) need
  an app *without* rotation.
* **Restrict API token usage** to the public IP addresses your Opportunity server connects from.
  Slack then rejects this app's tokens from anywhere else, so a stolen token is useless. In the
  manifest this is `settings.allowed_ip_address_ranges` (up to 20 addresses or CIDR ranges, IPv4
  only).
* **Leave PKCE off.** Slack's PKCE setting is meant for desktop and mobile apps. Turning it on makes
  the app a "public client", limits refresh tokens to 30 days, and can't be undone without
  contacting Slack support. Opportunity is a server-side app. Its sign-in code is useless without
  the client secret, which never leaves the server, and each sign-in is tied to the person's session
  with a one-time `state` value.

Keep the app internal to your workspace: don't distribute it publicly. Organisation-wide
installation on Enterprise Grid isn't supported, so install the app on each workspace.

If your workspace requires admin approval for apps, approve **Opportunity (read-only)** once. Each
person then connects their own account from Opportunity's settings page. Slack's consent screen
shows them the two permissions.

### 2. Configure Opportunity

From **Basic Information → App Credentials**, set these on the Opportunity server and restart it:

| Variable | Value |
|---|---|
| `SLACK_CLIENT_ID` | the app's Client ID |
| `SLACK_CLIENT_SECRET` | the app's Client Secret. Treat it like a password: it never leaves the server and is sent to Slack only with HTTP Basic authentication |
| `PUBLIC_URL` | the origin people use to reach Opportunity, e.g. `https://opportunity.example.com` |

`SLACK_API_BASE` and `SLACK_AUTH_BASE` exist only to point the tests at the Slack stand-in. Leave
them unset in production.

### 3. Connect

Each person opens Opportunity's settings, chooses **Connect Slack**, reviews Slack's consent screen
and allows it. Opportunity checks the granted scopes, asks `auth.test` who the token belongs to, and
shows the workspace and the person's Slack name.

## Pasting a token instead

For an evaluation, or where a redirect URL isn't possible, a person can paste a Slack **user token**
instead of signing in. No `SLACK_CLIENT_ID` is needed for this.

1. Create an app from the same manifest, with `token_rotation_enabled: false`. A rotating token
   expires in 12 hours and can't be renewed without signing in, so it is refused.
2. **OAuth & Permissions → Install to Workspace**, and allow.
3. Copy the **User OAuth Token** (it starts with `xoxp-`) and paste it into Opportunity.

Slack issues that token to the person who installed the app, so this works for one person per app.
The same checks apply: the token's scopes are read from `x-oauth-scopes` and must be read-only, and
it must have `search:read`. Opportunity refuses bot, app-level, rotating, refresh, browser-session
and legacy tokens, and says why. Such a token doesn't expire, so the person should remove it from
Slack when they're done (see below).

## Disconnecting and revoking

* **In Opportunity:** disconnecting deletes the stored token.
* **In Slack:** a person can remove their own authorization from the app's page in the workspace's
  app management (`https://<workspace>.slack.com/apps/manage`). An admin can remove the app for the
  whole workspace there, which revokes all of its tokens.

Opportunity never calls Slack's `auth.revoke`. That method changes state, so it is outside the
read-only allow-list by design. To invalidate a token at Slack immediately, revoke it in Slack as
above.

## If something goes wrong

| What the person sees | What it means | What to do |
|---|---|---|
| "This connection could do more than read (chat:write), so it wasn't used." | The token has a scope that isn't read-only. Slack's scopes add up: a token keeps every scope the person ever approved for the app | Remove anything but the two user scopes from the app. The person removes the app from their Slack account, then connects again |
| "Slack refused this server's app settings (bad_client_secret / bad_redirect_uri …)" | The server's credentials or redirect URL don't match the Slack app | Check `SLACK_CLIENT_ID`, `SLACK_CLIENT_SECRET` and that the app's redirect URL is exactly `{PUBLIC_URL}/api/connectors/slack/callback` |
| "The connection was rejected; please reconnect (… token_revoked / token_expired / invalid_auth)" | The token was revoked or the app removed, it expired, or it was used from an IP address outside the allowed ranges | Reconnect. If IP restriction is on, check the server's outgoing IP address |
| "Slack didn't report what this token is allowed to do (no x-oauth-scopes header)" | Something between Opportunity and Slack strips response headers | Let Opportunity reach `slack.com` without a header-stripping proxy |
| "Rate limited; try again in … s" | Slack's limit for `search.messages` (about 20 calls per minute per workspace, shared by everyone using the app) was reached before anything was found. If some searches had already worked, their results are used instead | Nothing; the next run will pick up |

## Known limits

* `search.messages` is what Slack now calls a legacy method. It is fully supported, but Slack
  recommends its newer Real-time Search API for apps with AI features. That API is documented for
  POST requests and needs the app's AI features turned on, so moving to it would be a deliberate
  change to the read-only design and has to be reviewed as one.
* The `OR` searches rely on Slack's `OR` operator, which Slack documents for its search. If a
  workspace doesn't honour it, those two searches find nothing, and the six one-word searches and
  the mention search still work.
* Organisation-wide installation on Enterprise Grid isn't supported. GovSlack (`slack-gov.com`)
  isn't tested.
