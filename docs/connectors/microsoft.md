# Microsoft 365 connector: setup and security

This page is for the administrator who connects Prospect to Microsoft 365, and for the security
reviewer who has to approve it. It covers what the connector reads, the exact permissions it uses
and why, how to register it in Microsoft Entra ID, and how "read-only" is enforced. That last part
doesn't rest on the permissions alone: Prospect enforces it independently in its own code.

## In one paragraph

When a person connects Microsoft 365, Prospect reads two things with that person's own
delegated access: recent mail in their **Outlook Inbox** that looks like an opportunity (an
invitation to speak, an RFP, an internal opening, a partnership request), and **calendar events in
the next 60 days** that look like one (a conference, a panel, an interview). For each item it reads
the subject, the sender or organiser, the date, a preview of at most 255 characters and the link
that opens it in Outlook. It never sends, deletes, moves, flags, accepts or changes anything. It
couldn't if it tried: it holds no write permission, it refuses any token that carries one, and its
only path to Microsoft Graph is a component that can send nothing but `GET` requests to four fixed
read endpoints.

## What is read, and what isn't

| | Read | Not read |
|---|---|---|
| Mail | Inbox messages received in the look-back window (30 days by default) that match opportunity keywords: subject, sender name and address, received time, `bodyPreview` (the first 255 characters, plain text), `webLink` | Full message bodies, attachments, headers, recipients, any folder other than the Inbox (Sent, Deleted, Junk, Archive, subfolders), other people's or shared mailboxes |
| Calendar | Events from today to 60 days ahead: subject, organiser, start time, location name, `bodyPreview`, `webLink`, whether it was cancelled, the person's response | Attendee lists, full descriptions, attachments, online-meeting links, other people's calendars |
| Profile | The signed-in user's id, display name, email address and user principal name | Anything about other users, groups or the directory |

* **The keywords** ("opportunity", "speaker", "rfp", "hiring", "partnership" and so on, about 40
  of them) are sent to Microsoft as a mailbox search, so Graph returns only matching mail.
  Prospect then ranks the results locally. Newsletters, notifications, cancelled events and
  routine meetings (stand-ups, 1:1s, focus time) are pushed down or dropped.
* **Bounded**: at most 200 messages and 150 events are looked at per run, and at most the run's
  item limit (60 by default) is passed on.
* **Reading changes nothing.** Reading through Microsoft Graph doesn't mark mail as read or answer
  invitations.
* **Where the data goes.** Items are fetched when a run starts and handed to Prospect's engine
  for that run only. The engine gives them, as text, to the AI model the server is configured with
  (DeepSeek by default, or Claude on Amazon Bedrock in your own AWS account), so it can recognise
  opportunities. The AI only receives text; it has no tools and can't cause any request. Fetched
  mail and events are not stored. What is stored is the opportunities found: a title, a short
  summary and the link back to Outlook.
* **Credentials.** The access token, refresh token, granted permissions and account name are
  stored encrypted at rest and never sent to the browser.

## Permissions

Prospect uses **delegated permissions only**: it acts as the signed-in person and can never see
more than they can. It uses **no application permissions** (no tenant-wide access to mailboxes).

| Permission (Microsoft Graph, delegated) | Why it's needed |
|---|---|
| `openid` | Sign the person in (OpenID Connect) |
| `profile` | Their name and user id, to show which account is connected |
| `email` | Their email address, for the same reason |
| `offline_access` | A refresh token, so scheduled runs work while the person is away. Microsoft's own description: it "does not give the app any additional permissions". |
| `User.Read` | Read the signed-in person's own profile, to check the connection works and whose it is |
| `Mail.Read` | Read mail. `Mail.ReadBasic` would be narrower, but it excludes the message preview, which is what makes an item useful. `Mail.Read` is the narrowest permission that includes it. |
| `Calendars.Read` | Read calendar events. `Calendars.ReadBasic` excludes the event description, so the same reasoning applies. |

None of these can write. `Mail.Read` can't send, delete, move or modify mail; that needs
`Mail.Send` or `Mail.ReadWrite`. `Calendars.Read` can't create, change or answer events; that needs
`Calendars.ReadWrite`. Prospect never asks for those.

> **Be precise with reviewers:** `Mail.Read` lets an app read the *whole* mailbox, bodies
> included. Prospect's code limits itself to the Inbox and the fields above, and its gateway
> limits it to four endpoints (see below). Microsoft doesn't enforce those limits; Prospect
> does. Microsoft *does* enforce the read-only part: these permissions can't write anything.

## Registering the app in Microsoft Entra ID

You need a Microsoft Entra role that can register applications and grant consent, such as Cloud
Application Administrator or Application Administrator.

1. **Register it.** In the [Microsoft Entra admin center](https://entra.microsoft.com), open
   **App registrations → New registration**.
   * **Name:** `Prospect` (shown to people on the consent screen).
   * **Supported account types:**
     * *Accounts in this organizational directory only (single tenant)* if this Prospect server
       is for your organisation alone. This is the recommended choice. Then set `MS_TENANT` to your
       **Directory (tenant) ID**.
     * *Accounts in any organizational directory (multitenant)* if one server serves several
       organisations. Then leave `MS_TENANT` unset (it defaults to `organizations`). Each
       organisation's administrator consents for their own tenant.
     * Don't choose the options that include personal Microsoft accounts: the connector is meant
       for work accounts.
   * **Redirect URI:** platform **Web**, value `{PUBLIC_URL}/api/connectors/microsoft/callback`,
     for example `https://prospect.example.com/api/connectors/microsoft/callback`. It must
     match exactly. Use https; Microsoft allows plain http only for `localhost`.
2. **Note the IDs.** On the app's **Overview** page, copy the **Application (client) ID** (this
   becomes `MS_CLIENT_ID`) and the **Directory (tenant) ID** (this becomes `MS_TENANT` for a single
   tenant).
3. **Create a client secret.** Go to **Certificates & secrets → Client secrets → New client
   secret**. Give it a description and an expiry; Microsoft recommends 180 days, and 24 months is
   the maximum. Copy its **Value** straight away (it's shown only once; the *Secret ID* is not the
   secret). This becomes `MS_CLIENT_SECRET`. Store it like a password, in your secret manager, not
   in source control.
4. **Set exactly these API permissions.** Go to **API permissions → Add a permission → Microsoft
   Graph → Delegated permissions** and add `openid`, `profile`, `email`, `offline_access`,
   `User.Read`, `Mail.Read` and `Calendars.Read`. (`User.Read` is usually there already.)
   * Add nothing else, and never any application permission.
   * **This matters:** Microsoft includes *every* permission consented for the app in each token,
     not just the ones Prospect asks for. If someone later adds `Mail.Send`, `Mail.ReadWrite`,
     `Calendars.ReadWrite` or any other permission to this registration and it gets consented,
     Prospect refuses every token from then on. New connections fail, and existing ones stop at
     their next token refresh, with a message to reconnect "with read-only access". The fix is to
     remove the extra permission and revoke its consent.
5. **Grant consent.** Choose **Grant admin consent for _your organisation_** on the API permissions
   page. These permissions don't require an administrator, so people can consent for themselves
   where your tenant allows user consent. Most organisations restrict user consent, though, and
   admin consent saves everyone a prompt. For a multitenant app, each organisation's administrator
   does this in their own tenant, for example by signing in once through Prospect and choosing
   *Consent on behalf of your organization*.
6. **Optional: limit who can connect.** Open **Enterprise applications → Prospect → Properties**
   and set **Assignment required** to **Yes**. Then add the people or groups allowed to connect
   under **Users and groups**. Conditional Access policies apply to this app like any other.

### Rotating the client secret

An app registration can hold more than one secret at a time, so rotation needs no downtime:

1. Create a new secret (step 3) before the current one expires.
2. Put its value in `MS_CLIENT_SECRET` and restart Prospect.
3. Check that connecting Microsoft 365 still works, then delete the old secret in Entra ID.

People's connections are unaffected: their refresh tokens are tied to the app, not to a particular
secret. If the secret expires or is wrong, Prospect reports that the app registration was
refused and that the administrator needs to check `MS_CLIENT_ID`, `MS_CLIENT_SECRET` and
`MS_TENANT`. People aren't asked to reconnect, because reconnecting wouldn't help.

## Configuring Prospect

| Variable | Value |
|---|---|
| `MS_CLIENT_ID` | The Application (client) ID |
| `MS_CLIENT_SECRET` | The client secret's value |
| `MS_TENANT` | Optional. Your Directory (tenant) ID (or a verified domain such as `contoso.onmicrosoft.com`) for a single-tenant app; `organizations` (the default) for any work or school account; `common` also allows personal accounts, which isn't recommended. |
| `PUBLIC_URL` | The address people use to reach Prospect; the redirect URI is built from it |

Microsoft 365 shows up as available in Settings once both `MS_CLIENT_ID` and `MS_CLIENT_SECRET`
are set. A malformed `MS_TENANT` is never "corrected" into a broader one: sign-in fails instead.

Only the global Microsoft cloud is supported (`login.microsoftonline.com`, `graph.microsoft.com`),
not national clouds. Outbound, the connector talks to those two hosts only.

## How read-only is enforced

Four independent layers. Any one of them alone prevents a change to a mailbox or calendar.

**1. The token can't write.** Prospect asks only for the read permissions above. Delegated
access also means it can never exceed what the person can do.

**2. Every token's granted permissions are checked.** Microsoft's token response states what the
token is valid for, which can be more than was requested (see step 4 above). On sign-in and on
**every** refresh, Prospect checks that list against an allow-list (`User.Read`, `Mail.Read`,
`Mail.ReadBasic`, `Calendars.Read`, `Calendars.ReadBasic`, plus the sign-in scopes) *and* against a
deny-list of write markers ("write", "send", "manage" and others). A token carrying anything else
is refused before it is used for anything:

* at sign-in, the connection isn't saved;
* at refresh, the connection is suspended until the person reconnects.

A response that doesn't state the granted permissions is refused too: what can't be verified isn't
used. Token lifetimes are capped at a day, so a connection in regular use is refreshed, and
re-checked, at least daily. Stored connections are re-checked before every use.

**3. There is no code path that writes.** All data access goes through one component, the gateway,
whose only network method is HTTP `GET`. It sends a request only when the URL's origin is
`https://graph.microsoft.com` and its path is exactly one of:

| Path | Used for |
|---|---|
| `/v1.0/me` | Checking the connection and whose it is (`$select=id,displayName,mail,userPrincipalName`) |
| `/v1.0/me/mailFolders/inbox/messages` | Inbox mail: `$search` (keywords) or `$filter` (date), `$select`, `$top=50` |
| `/v1.0/me/calendarView` | Events: `startDateTime`/`endDateTime`, `$select`, `$orderby`, `$top=50` |
| `/v1.0/me/messages` | Not used by the connector; allow-listed only for the gateway's own test suite |

The gateway also enforces the following:

* Nothing else can be reached: not `/sendMail`, not `/$batch`, not another user's
  `/users/{id}/…`, not an item path.
* Redirects are never followed.
* Only the `Accept`, `Accept-Language`, `ConsistencyLevel` and `Prefer` headers can be added, so
  no method-override header can turn a read into a write.
* Paths with tricks (`..`, encoded slashes) are refused.
* Each next-page link Microsoft returns is checked like any other URL; one that points anywhere
  else ends paging.

The only other request the connector can make is a form `POST` to
`https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token`, for sign-in and refresh. That's an
identity endpoint, which can't touch mailbox data.

**4. The AI never gets tools.** The connector is called by ordinary server code. The AI model only
ever receives the resulting text and can't trigger any request.

### Audit trail

Every request and every refusal is recorded for the person: the method, the host and path, the
status, size and duration, and the time. Query strings, message content and secrets are never
recorded. So the audit log for a Microsoft connection contains `GET`s to the paths above, `POST`s
to the token endpoint, and nothing else. A refused token appears as `scope_rejected`, a refused
URL as `blocked`.

### Sign-in hardening

* PKCE (S256) on every sign-in.
* A random `state` that the server checks on the callback.
* The authorization code is redeemed server-side with the client secret, so tokens never pass
  through the browser.
* The ID token is decoded only to display the account's name and email, and is never used for
  authorization.
* Pasting a token instead of signing in is not supported for Microsoft 365.

## Revoking access

* **The person** can disconnect Microsoft 365 in Prospect, which removes the stored credential.
* **An administrator** can do any of the following:
  * remove the person's assignment, or disable sign-in for the enterprise application;
  * revoke the app's consent;
  * revoke the person's sessions, which invalidates their refresh tokens.

Prospect notices at the next refresh ("the connection was rejected; please reconnect") and
stops using the connection. An access token already issued stays valid until it expires (usually
about an hour), as with any Microsoft 365 app.

## Behaviour under load and errors

* **Throttling.** If Microsoft Graph asks Prospect to wait (HTTP 429 with `Retry-After`) for 5
  seconds or less, it waits and retries once; a brief 503 is also retried once. It never spends
  more than 10 seconds waiting in one run. A longer wait means the run uses what it has already
  read, or reports that the service asked it to slow down.
* **Partial failures.** If only mail or only the calendar can be read (for example because a policy
  blocks calendar access), the other is still used. If the token is rejected, the run stops and the
  connection is refreshed or marked for reconnection.
* **Search unavailable.** If a mailbox refuses the keyword search, Prospect falls back to listing
  the window's Inbox mail by date (still only the same fields) and ranks it locally.

## Known limitations

* Only the Inbox is read. Mail that rules file into subfolders isn't seen.
* Prospect judges an email by its subject and the first 255 characters only. (Microsoft's search
  does match keywords further down the message.)
* Only the global Microsoft cloud is supported, and the client must authenticate with a secret
  (certificate credentials aren't supported yet).
* The organisation name shown for a connection is the person's email domain: showing the tenant's
  display name would need another permission.

## Verifying it yourself

* The policy (endpoints and permissions) is in `crates/connectors/src/microsoft.rs` (`POLICY`).
  The enforcement is in `crates/connectors/src/lib.rs` and `crates/connectors/src/gateway.rs`.
* `cargo test -p connectors` runs, among others, an end-to-end suite against a faithful mock of
  Microsoft's identity platform and Graph (`tests/mock/microsoft_mock.py`). It covers the
  following:
  * sign-in with PKCE;
  * a tenant whose administrator consented `Mail.Send` and `Mail.ReadWrite` (refused at sign-in);
  * a refresh that gains `Calendars.ReadWrite` (refused);
  * revoked and expiring tokens;
  * throttling;
  * a next-page link pointing at `/sendMail` (refused and audited).

  Every test ends by checking that the mock received no write request of any kind and that the
  audit log holds only `GET` reads and token `POST`s.
