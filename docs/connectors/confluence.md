# Confluence connector: setup and security

This page is for the administrator who connects Opportunity to Confluence Cloud, and for the
security reviewer who has to approve it. It covers what the connector reads, how it is kept
read-only, and how to set it up.

## Summary

Opportunity can search a person's Confluence Cloud for opportunities in their own work context:
internal calls for ideas, hackathons, mentoring schemes, open roles, calls for speakers, funding
rounds, and strategy pages that show where the company is heading.

The connector is **read-only by construction**, with four independent layers:

1. It asks Atlassian for read scopes only.
2. It checks the scopes Atlassian actually granted, on every connection and every token refresh,
   and refuses any token that could do more than read.
3. It has no code path that can write. Its only access to Confluence data is an HTTP `GET`
   gateway limited to a short list of read endpoints.
4. The AI model never gets tools. It receives text and cannot cause any request.

It sees only what the connected person can already see in Confluence. It keeps short excerpts in
memory for the length of a run and stores none of them.

There are two ways to connect. **OAuth 2.0 (3LO)** is recommended, and all four layers apply.
**A pasted API token** is a fallback with a weaker guarantee: Atlassian doesn't reveal what an API
token can do, so only Opportunity's own enforcement (layers 3 and 4) applies.

## What Opportunity reads

Every request the connector can make is in this table. Nothing else is permitted.

| Request | Why | When |
|---|---|---|
| `POST https://auth.atlassian.com/oauth/token` | Exchange the sign-in code for tokens; refresh them. This is Atlassian's identity service, not the Confluence data API. | OAuth: on connecting, and when the one-hour access token expires |
| `GET https://api.atlassian.com/oauth/token/accessible-resources` | Which Confluence site the token reaches, and with which scopes. Any write-capable scope stops the connector. | OAuth: on every check and before every search |
| `GET …/wiki/rest/api/user/current` | Who is connected (their display name). | On every check of a connection; with an API token also on connecting and before every search |
| `GET …/wiki/rest/api/search?cql=…` | The search itself. | Each run: at most 4 requests of 25 results |
| `GET https://<site>.atlassian.net/_edge/tenant_info` | The site's public cloud id, so that a scoped API token can use Atlassian's API gateway. | API token: on connecting |

With OAuth, `…` is `https://api.atlassian.com/ex/confluence/<cloud id>`. With an API token it is
either that gateway address (scoped tokens) or `https://<site>.atlassian.net` (classic tokens).

### The search

Each run sends one Confluence search (CQL), like this:

```
type in (page, blogpost) AND lastmodified >= "2026-10-27"
  AND (text ~ "hackathon" OR text ~ "\"call for\"" OR text ~ "mentoring" OR …)
  ORDER BY lastmodified DESC
```

It looks for pages and blog posts changed in the last 30 days (configurable) that mention words
which tend to mark an opportunity. Confluence applies the person's own permissions to search, so
results only include content they can already open. Archived spaces are left out.

From each result the connector keeps only:

* the title,
* the short excerpt Atlassian returns around the matching words (with highlight markup removed),
* the space name,
* the date it was last changed, and
* a link to the page.

The results are ranked (a keyword in the title counts most, and recent pages count more), and the
best ones (60 by default) go to the engine.

**Not requested:** page bodies, attachments, comments, version history, user directories, author
names, labels, restrictions or permissions. The search asks for no expansions.

### What is stored

* **Page content: nothing.** Excerpts are fetched at run time, passed to the engine and dropped.
  Only the opportunities the engine extracts are stored, each with a link back to the page.
* **Credentials:** encrypted in the server's vault, never sent to the browser.
* **About the connection:** the site's address, name and cloud id, and the person's display name,
  so that they can see what is connected.

## How read-only is enforced

### With OAuth 2.0 (all four layers)

**1. The credential can't write.** Opportunity requests exactly these scopes:

| Scope | Why it's needed |
|---|---|
| `search:confluence` | Run the CQL search. |
| `read:confluence-content.summary` | The content summary that search results carry: title, type, link. No page bodies. |
| `read:confluence-space.summary` | The space summary that search results carry: the space name. |
| `read:confluence-user` | Read the connected person's own profile (`user/current`) to show who is connected. |
| `offline_access` | Get a refresh token, so that the person doesn't have to sign in again every hour. |

Every one of these is read-only. Opportunity never requests `write:confluence-content`,
`write:confluence-space`, `write:confluence-file`, `write:confluence-props`,
`manage:confluence-configuration`, any granular `write:…` or `delete:…` scope, or a scope from
another product. Atlassian only lets an app request scopes that its administrator added to the app
in the developer console. If the app is configured with read scopes only (see the setup below),
Atlassian cannot grant anything else to it.

**2. What was granted is checked, every time.**

* **When connecting and on every refresh:** Opportunity reads the `scope` field of the token
  reply. Atlassian rotates refresh tokens, so every refresh issues a new token, and each new token
  is checked.
* **On every check and before every search:** Opportunity also reads the scopes Atlassian lists
  for each site in `accessible-resources`.

Each scope must be on Opportunity's read-only allow-list, and it must also pass a hard-coded
deny-list of write markers (`write`, `manage`, `delete`, `admin`, `create`, `update`, `edit` and
others). One unknown or write-capable scope is enough to refuse the token: the connection isn't
saved (or stops being used), and the person is asked to reconnect with read-only access. A token
reply that doesn't say which scopes it granted is refused too, because it can't be checked.

**3. There is no code path that writes.** The connector reaches Confluence data only through
Opportunity's gateway, which:

* sends HTTP `GET` only. No other method exists in it, and method-override headers
  (`X-HTTP-Method-Override` and similar) are refused;
* only contacts allow-listed origins: `https://api.atlassian.com`, and for API tokens
  `https://<one name>.atlassian.net`;
* only requests these exact paths:
  * `/oauth/token/accessible-resources`
  * `/ex/confluence/<cloud id>/wiki/rest/api/search`
  * `/ex/confluence/<cloud id>/wiki/rest/api/user/current`
  * `/wiki/rest/api/search`
  * `/wiki/rest/api/user/current`
  * `/_edge/tenant_info`

  The endpoints that create, change or delete content (`/wiki/rest/api/content`,
  `/wiki/api/v2/pages` and so on) are not on the list, so even a bug can't reach them;
* never follows redirects, and refuses path tricks (`..`, encoded slashes, `;` parameters);
* refuses replies larger than 4 MB.

The only other request the connector can send is the `POST` to Atlassian's token endpoint. That
endpoint can't change anything in Confluence. Every request, and every refusal, is written to an
audit log.

**4. The AI never gets tools.** Connectors are called by ordinary server code on a schedule or
when the person asks for a run. The AI model only ever receives the excerpts as text. It cannot
cause a request of any kind.

The connector also defends itself against bad input:

* Search keywords are reduced to letters, digits and spaces, then escaped as CQL strings, so
  nothing can change the query's meaning.
* Page links are built from the site's own address. Links to any other origin are dropped.
* Atlassian's error messages are never echoed in a way that could expose a code or a token.

### With an API token (layers 3 and 4 only)

Atlassian doesn't reveal which scopes an API token has, so Opportunity cannot check that a token is
read-only. **For API-token connections, only Opportunity's own enforcement applies.** That is the
GET-only gateway with its allow-list (layer 3) and the AI without tools (layer 4). Opportunity
labels these connections "app-enforced" (as opposed to "provider-enforced").

What this means in practice:

* Opportunity still never writes: the gateway has no way to.
* The token itself may be able to write. A *classic* (unscoped) API token carries all of the
  person's Confluence permissions, including editing and deleting. If such a token leaked from
  anywhere, it could be used to write. Opportunity stores it encrypted and never shows it again,
  but the token's own power is outside Opportunity's control.
* **We recommend a scoped API token with read scopes only** (see below), with the shortest expiry
  that is practical. Prefer OAuth whenever the organisation can create an OAuth app.

## Setting up OAuth 2.0 (3LO) (recommended)

One OAuth app serves everyone in the organisation who uses Opportunity. You'll need an Atlassian
account and the address people use to reach Opportunity (`PUBLIC_URL`).

1. Open the Atlassian developer console at <https://developer.atlassian.com/console/myapps/>, and
   choose **Create → OAuth 2.0 integration**. Name it so people recognise it on the consent screen
   (for example "Opportunity"), and accept the developer terms.
2. **Permissions:** next to **Confluence API**, choose **Add**, then **Configure**. Under the
   classic scopes, add exactly these four and nothing else:
   * `search:confluence`
   * `read:confluence-content.summary`
   * `read:confluence-space.summary`
   * `read:confluence-user`

   Don't add any write scope, granular scope, or other API (Jira and so on). `offline_access` isn't
   configured here: Opportunity asks for it when a person connects.
3. **Authorization:** next to **OAuth 2.0 (3LO)**, choose **Configure** and set the callback URL to:

   ```
   {PUBLIC_URL}/api/connectors/confluence/callback
   ```

   For example `https://opportunity.example.com/api/connectors/confluence/callback`. It must match
   exactly, including `https`.
4. **Distribution:** a new app is private, so only you can use it. To let colleagues connect,
   enable sharing. Atlassian then warns people that the app "has not yet been reviewed by
   Atlassian". That is expected for an organisation's own integration. If Atlassian asks about
   personal data: Opportunity stores, per connection, the site and the person's display name. It
   stores no page content.
5. **Settings:** copy the client ID and secret. Give them to the Opportunity server as
   environment variables, and restart it:

   ```
   ATLASSIAN_CLIENT_ID=...
   ATLASSIAN_CLIENT_SECRET=...
   PUBLIC_URL=https://opportunity.example.com
   ```

   Keep the secret like a password. It never leaves the server.

Use an app dedicated to Opportunity. Don't reuse an app that has other scopes: Opportunity refuses
any token that carries a scope outside its read-only list, including scopes for other Atlassian
products.

### What people see

When a person connects, Atlassian's consent screen lists the read permissions above and asks them
to choose their site. Back in Opportunity, the connection shows the site and their name, and it is
marked "provider-enforced".

Atlassian doesn't support PKCE for apps made in the developer console. The sign-in is protected
instead by:

* a random, single-use `state` value that is kept on the server;
* the exact callback URL; and
* the client secret, which is only ever sent from the server to Atlassian's token endpoint.

### Revoking access

* **The person** can disconnect in Opportunity, which deletes the stored tokens. They can also
  revoke the app in their Atlassian account under **Connected apps**
  (<https://id.atlassian.com/manage-profile/apps>).
* **An administrator** can remove the app from the site. Atlassian doesn't let administrators
  revoke one user's access to an app individually.
* **Automatically:** refresh tokens expire after 90 days without use, and when the person changes
  their Atlassian password. Opportunity then asks them to reconnect.

## The API-token alternative

Use this only if your organisation can't create an OAuth app. Remember the weaker guarantee
described above.

1. The person signs in at <https://id.atlassian.com/manage-profile/security/api-tokens>.
2. **Recommended:** choose **Create API token with scopes**. Pick an expiry (Atlassian allows 1 to
   365 days; shorter is better), choose **Confluence**, and select read scopes only:
   `search:confluence` and `read:confluence-user`, optionally with
   `read:confluence-content.summary` and `read:confluence-space.summary`. If only granular scopes
   are offered, `read:content-details:confluence` covers both endpoints Opportunity uses. Never
   select a `write:`, `delete:` or `manage:` scope.
3. In Opportunity, enter:
   * the site, for example `https://your-company.atlassian.net`;
   * the email address of the Atlassian account; and
   * the token.

How Opportunity uses the token:

* **Site check.** The site must be `https://<name>.atlassian.net`. Plain `http`, other domains,
  extra subdomains, ports, paths, queries and embedded credentials are refused before anything is
  sent. A trailing `/` or `/wiki` is accepted and removed.
* **Authentication.** The token is sent with HTTP basic authentication (email and token), over
  HTTPS only.
* **Choosing the API.** Atlassian accepts scoped tokens only through its API gateway
  (`https://api.atlassian.com/ex/confluence/<cloud id>`). So Opportunity reads the site's public
  cloud id from `https://<site>/_edge/tenant_info` and tries the gateway first. A classic token is
  then used on the site's own API (`https://<site>/wiki/rest/api/…`).
* **Identity check.** Before each run, Opportunity checks who the token signs in as. Confluence
  can answer a failed sign-in as an anonymous visitor; that answer is treated as a refusal, never
  as a working connection.

If your organisation uses Atlassian Guard, administrators can also review and revoke people's API
tokens centrally.

## Audit log

Every request and refusal is recorded per person with the following fields:

* provider;
* action (`read`, `token`, `blocked` or `scope_rejected`);
* method;
* origin and path (no query string, so search keywords aren't logged);
* status, size and duration;
* time.

The log never contains page content, tokens, codes or query values.

## Limits and failures

* **Rate limits:** when Atlassian answers 429, Opportunity reports how long to wait (from
  `Retry-After`) and tries again later. If a later page of results fails, the run keeps what it
  already found. Atlassian's points-based quotas for OAuth apps apply; a run uses 2 to 6
  requests.
* **Rejected tokens:** a rejected access token is refreshed. A rejected refresh token, or a token
  that gained a write scope, makes Opportunity stop and ask the person to reconnect.

## Known limitations

* **Several sites.** If a person has granted the app access to several Confluence sites,
  Opportunity uses the one recorded for the connection. Before one is recorded, it uses the first
  site with Confluence search that Atlassian lists. There is no site picker yet.
* **Unverified Atlassian behaviour.** Two API-token details rest on Atlassian's documentation and
  community reports, not on a live site: `/_edge/tenant_info` is not part of Atlassian's official
  API, and we assume classic tokens are refused at the gateway, so they fall back to the site's API.
  If tenant_info can't be read, classic tokens still work, but scoped tokens can't be used.
* **No author names.** Author names aren't fetched (see "What Opportunity reads").

## Reference

| Setting | Meaning |
|---|---|
| `ATLASSIAN_CLIENT_ID`, `ATLASSIAN_CLIENT_SECRET` | The OAuth app (both required to offer OAuth). |
| `PUBLIC_URL` | Where people reach Opportunity. The callback is `{PUBLIC_URL}/api/connectors/confluence/callback`. |
| `ATLASSIAN_AUTH_BASE`, `ATLASSIAN_API_BASE`, `CONFLUENCE_SITE_BASES` | Test overrides only (the mock server). Only https, or http on the local machine, is accepted. |

Atlassian documentation:
[OAuth 2.0 (3LO) apps](https://developer.atlassian.com/cloud/confluence/oauth-2-3lo-apps/),
[scopes](https://developer.atlassian.com/cloud/confluence/scopes-for-oauth-2-3LO-and-forge-apps/),
[search API](https://developer.atlassian.com/cloud/confluence/rest/v1/api-group-search/),
[CQL](https://developer.atlassian.com/cloud/confluence/advanced-searching-using-cql/),
[scoped API tokens](https://support.atlassian.com/confluence/kb/scoped-api-tokens-in-confluence-cloud/),
[rate limiting](https://developer.atlassian.com/cloud/confluence/rate-limiting/).
