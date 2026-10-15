# Design: enterprise identity for TT Station (SSO, MFA, central revocation)

*Status: proposal for decision. Written 2026-10-15 against `3d50ad6`. Audience: TT Station
engineering, IT/security, and whoever owns the QuietBox 2 company-wide rollout.*

## Summary

TT Station boxes today trust **devices**, not **people**. Each box pairs its own clients and
keeps its own list of them; nothing ties a paired laptop to an employee, and nothing outside
the box can sign that laptop out. For a company-wide rollout we need sign-in through the
corporate IdP, MFA, and revocation when someone leaves.

**Proposal:** add an optional **org mode** to the agent. A box enrolled in an org accepts an
**OIDC ID token** from the company IdP in place of a pairing code. The box checks the token
and then mints the **same per-device session it mints today**, now tagged with the person's
subject, groups and authentication strength. The rest of the agent keeps working on device
sessions as before. The main changes:

- **The client signs in and the box verifies.** Desktop and iOS run the authorization-code
  flow with PKCE as public native clients (RFC 8252) and hand the box an ID token bound to a
  nonce the box issued. The CLI uses the device authorization grant, run by the box itself.
  The box never holds IdP refresh tokens.
- **Revocation by freshness.** An org session is only good while its holder keeps showing the
  box a fresh ID token, by default at least every 8 hours. Clients get these from the IdP with
  their refresh token. Once a user is disabled in the IdP, refresh fails and their sessions on
  every box stop within the freshness window. This needs no inbound connection to boxes on
  private LANs, which back-channel logout would.
- **Roles come from IdP groups.** Each refresh re-reads them.
- **MFA belongs to the IdP.** Owner actions additionally need a recent authentication
  (`auth_time` / `max_age`) at a required `acr`. When they don't have one, the box answers
  with the RFC 9470 step-up challenge.
- **Personal boxes don't change.** With no `[org]` config, pairing works exactly as it does
  today.

Estimated effort: about **10–13 engineer-weeks** in four phases, plus a security review and
IdP setup. An optional central fleet service (SCIM, push revocation) would add 3–4 weeks.

## 1. Current state (verified in code at `3d50ad6`)

| Piece | Where | What it does today |
|---|---|---|
| Pairing | `routes.rs` (`/pair/init`, `/pair/complete`, `/pair/offer`, `/pair/qr`), `pairing.rs`, `pairqr.rs` | The box shows a 6-digit code, valid for 120 s with 5 wrong attempts per `pair_id` (`PAIR_TTL`, `MAX_PAIR_ATTEMPTS`). `/pair/offer` and the QR are for holders of the box user's `local-secret`, and anything paired that way is an owner. The QR also carries the TLS fingerprint. |
| Device sessions | `sessions.rs` | Each session has a 256-bit bearer token. The box stores only its SHA-256, in `agentd-tokens.json` (0600, store v3). A session ends after **90 days idle** (`IDLE_EXPIRY`); every use slides the window, and there is no absolute lifetime. Revocation is per device (`revoke`) or for everyone (`revoke_all`). A `watch` channel closes the session's open WebSockets (`routes.rs` `signed_out`). |
| Roles | `routes.rs` `Caller` / `OwnerAuth` | Two roles, owner and member. The first device to pair becomes an owner (`owner_claimed`), and the local user is always an owner. When a member is refused, `OwnerAuth` returns 403 and the refusal is audited. |
| Local trust | `localpeer.rs` | Plain HTTP from 127.0.0.1 counts as owner only if `/proc/net/tcp` shows the socket belongs to the agent's own uid. |
| Transport | `tls.rs` | A self-signed certificate on ctrl-port+1. Clients **pin** its SHA-256, which they get from the pairing answer or the QR. Plain HTTP for other hosts is off by default (`/security`). |
| App keys | `gateway.rs` | `ttq-…` keys for the `/v1` model API. The box stores SHA-256 digests (`app-keys.json`), allows at most 32, and supports expiry, rate and token limits, scopes and pausing. A key is owned by the box, not by a person. |
| SSH keys | `authkeys.rs`, `/ssh/authorize` | Writes a public key into the box user's `authorized_keys` with a `ttstation:<label>` tag. Unpairing the device doesn't remove the key. |
| People | `people.rs` | Household "people", each tied to the digest of a pairing token, used to keep assistant memories apart. They aren't identities. |
| Audit | `audit.rs` | `audit.jsonl` (0600, append-only, rotated at 10 MB with 5 files kept). Entries look like `{ts, device_id, device_name, action, target, result}`. Owners read them through `GET /audit`. There is no export. |
| Client secrets | `desktop/electron/tokenstore.cjs`, `ios/Sources/BoxStore.swift`, `libttstation/src/secrets.rs` | Desktop seals tokens with Electron `safeStorage` and refuses to pretend on Linux without a keyring. iOS uses the Keychain with `AfterFirstUnlockThisDeviceOnly`. The CLI uses the macOS Keychain, or a 0600 file elsewhere. |

What's missing for enterprise use:

- **No person identity.** A session names a device, not an employee.
- **No MFA.**
- **No central directory.** Owner and member are set separately on each box.
- **No central revocation.** A departed employee's laptop works on every box it paired with
  until each box's owner removes it, or until it sits idle for 90 days. App keys and SSH keys
  they made keep working after that.
- **No audit export.**

## 2. Goals and non-goals

**Goals:**

- Sign in with the corporate IdP on every client.
- Require MFA where the IdP enforces it.
- Owner rights follow IdP groups.
- A deprovisioned user loses access to every box within a bounded, configurable time, with
  no work needed on each box.
- Audit events reach the company SIEM.
- Enrolled boxes keep working through IdP outages, within a stated grace period.
- Personal boxes are unaffected.

**Non-goals for this design:**

- Managing the box's Linux accounts and OS-level SSH. That stays with IT's existing tooling;
  see §9.
- Multi-tenant isolation between people on one box. See
  [isolated-chips-per-user.md](isolated-chips-per-user.md).
- A cloud fleet manager, beyond the optional Phase 4.

## 3. Architecture

```
  Client (desktop / iOS / CLI)            Box (tt-station-agentd)                 IdP
  ---------------------------            -----------------------                 ---
  1  GET /auth/config  ------------------> issuer, client_id, scopes, acr policy
  2  POST /auth/oidc/start --------------> challenge_id + nonce (single use, 5 min)
  3  authorization code + PKCE, nonce, (acr_values / max_age for owners) ------> sign-in + MFA
                                                                     <------ code -> tokens
  4  POST /auth/oidc/complete {challenge_id, id_token} ----->
                                          verify: sig (cached JWKS), iss, aud=client_id,
                                          exp/iat, nonce == challenge, auth_time, acr,
                                          groups -> role
                                   <----- device session token (as today) + role + fresh_until
  5  every <= 8 h: IdP refresh_token -> new id_token ---------------------------> (fails if user disabled)
     POST /auth/oidc/refresh {id_token} (with the session bearer) ->  fresh_until += 8 h, role re-read
```

### 3.1 Why the client signs in and the box verifies

The alternative is to make each box a confidential OIDC client. That would need a redirect URI
per box: a hostname, which LAN boxes often don't have, registered in the IdP for every box, plus
a client secret on every box. Instead:

- **One public native-app registration** ("TT Station") serves the whole org. Its redirect
  URIs are the loopback (`http://127.0.0.1/callback`, any port, per RFC 8252 §7.3) and the
  iOS callback scheme. The device authorization grant is enabled on it.
- **The box is a resource that verifies ID tokens.** It needs only the issuer's discovery
  document and JWKS. It caches both, so verification works with no outbound connection
  between JWKS refreshes.
- **The box never sees refresh tokens.** A compromised box can't use someone's IdP session
  anywhere else.
- **Replay is blocked by a box-issued nonce.** ID tokens for the shared client id are valid
  for every box in the org. The nonce from step 2 makes a token from step 3 usable only at
  the box that issued the challenge, and only once.

### 3.2 Flows per client

| Client | Flow | Library | Where the refresh token lives |
|---|---|---|---|
| **Desktop (Electron)** | Authorization code + PKCE (S256) in the **system browser** (`shell.openExternal`). The callback is a one-shot loopback listener on `127.0.0.1:<random port>` in the main process. Never an embedded `BrowserWindow` (RFC 8252 §8.12): the system browser carries the user's existing SSO and passkeys. | [`openid-client`](https://github.com/panva/openid-client) v6 (MIT), main process only. | `safeStorage`, the same as box tokens (`tokenstore.cjs`). On Linux without a keyring, don't store it; the user signs in again each freshness window, and the UI says so. |
| **iOS** | Authorization code + PKCE through **`ASWebAuthenticationSession`**, with `prefersEphemeralWebBrowserSession = false` so Safari's IdP session and Okta Verify or Authenticator work. The callback is the app's custom scheme, or a claimed universal link if the IdP requires https. | Plain Swift (about 300 lines: PKCE, URL building, token POST), or **AppAuth-iOS** (Apache-2.0). Use **MSAL** (MIT) instead if the IdP is Entra with device-compliance Conditional Access: only the broker can satisfy that. | Keychain, `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly` (as `BoxStore.swift` already does). |
| **CLI (`tt-station`)** | **Loopback + PKCE** when a browser is available, the default on a desktop. The **device authorization grant** (RFC 8628) over SSH or when headless (`tt-station login --device`). For the device grant, the **box** starts the flow and polls the IdP (`POST /auth/oidc/device`), so the ID token goes straight from the IdP to the box and needs no nonce. | Rust [`oauth2`](https://crates.io/crates/oauth2) 5.x (MIT/Apache) for loopback, in `libttstation`. | macOS Keychain; elsewhere the existing 0600 `FileStore`. With the device grant, the CLI holds no IdP token at all, only the box session; when it goes stale, the user runs the device flow again. |
| **Box (agentd)** | Verifies ID tokens. Discovery and JWKS caching. A device-grant client for the CLI path. | [`openidconnect`](https://crates.io/crates/openidconnect) 4.x (MIT/Apache) with `default-features = false` and reqwest/rustls on **ring** (the tree already uses ring; avoid aws-lc). Its `CoreIdTokenVerifier` checks issuer, audience, algorithm, expiry and nonce. Both crates pass `deny.toml`'s allow-list. | None. |

The device grant is the weakest flow: it's open to device-code phishing, where an attacker
starts a flow and gets a victim to approve it. To limit it:

- The CLI is the only client offered it.
- An org can turn it off (`device_grant = false`).
- A session from the device grant is **member-only until a browser step-up**.
- Each one posts a box notification: "Jane signed in from the CLI at 10.1.2.3".
- On Entra, Conditional Access can restrict the "device code flow" to managed networks.

### 3.3 Transport trust for org boxes

Pairing does two jobs today: the 6-digit code proves a human saw the box, and the pairing
answer and QR give the client the TLS pin. OIDC replaces the first job but **not the second**.

Without an authenticated channel, a LAN attacker can relay a real box's challenge to a
victim, receive the victim's ID token, and redeem it at the real box. So org boxes need a
trusted certificate:

1. **Preferred: corporate PKI.** At enrollment the box gets a certificate for its DNS name
   from the internal CA, through ACME (step-ca, AD CS with ACME, Vault PKI) or from config
   management. Clients validate it the normal way through the OS trust store, which MDM
   already provisions.
2. **Fallback: no PKI.** On a client's first contact with an org box, it pins the certificate
   with a 6-digit code, as pairing does today. Sign-in is still OIDC.

One code per laptop per box, ever, is cheap. The code then confirms the channel, not identity.

### 3.4 Enrollment (box joins an org)

Enrollment is a local owner action on the box: the agent's user, or root through
`tt-station-agentd enroll`. It can also be pushed by config management. It writes an `[org]`
section:

```toml
[org]
name            = "tenstorrent"
issuer          = "https://tenstorrent.okta.com"        # exact `iss`; https only
client_id       = "0oa…"                                 # the shared public native app
scopes          = ["openid", "profile", "email", "groups", "offline_access"]
owner_groups    = ["tt-station-admins", "tt-station-owners-{box}"]   # {box} = box name
member_groups   = ["tt-station-users", "tt-station-users-{box}"]
owner_acr       = ["phr", "urn:okta:loa:2fa:any"]        # IdP-specific, see §5
owner_max_age   = "15m"                                  # step-up freshness for owner actions
freshness       = "8h"                                   # how often a session must re-prove itself
offline_grace   = "72h"                                  # extra time when the IdP is unreachable
idle_expiry     = "14d"                                  # replaces 90 d for org sessions
absolute_expiry = "30d"                                  # then sign in again, regardless
device_grant    = true
pairing         = "first-contact-only"                   # "off" | "first-contact-only" | "owner-approved" | "on"
jwks_ca         = ""                                     # optional PEM for an on-prem IdP's TLS
[org.audit]
sink            = "https://siem.example/hec"             # or "syslog+tls://host:6514", or "" (file only)
```

`enroll`:

1. Fetches `issuer/.well-known/openid-configuration`.
2. Requires `issuer` in the response to equal the configured value exactly.
3. Requires the `jwks_uri` to be https.
4. Caches the discovery document and JWKS, and prints the issuer and key ids for the operator
   to confirm.

Afterwards the issuer is **pinned** in config. Changing it means un-enrolling, which takes an
owner with a fresh step-up or local root, and is audited.

How the cached keys are handled:

- **Kept on disk.** JWKS is cached with its fetch time, and a key in use is never thrown away
  because a fetch failed.
- **Refetch on a new `kid`.** An unknown `kid` triggers a refetch, at most once a minute.
- **Refreshed every 24 h.** A routine refetch runs daily when the box can reach the IdP.
- **Algorithms.** Only RS256, PS256 and ES256 are accepted; never `none`, never HS*.

## 4. Sessions, lifetimes and revocation

### 4.1 What a session becomes

The store goes to **v4**: each session gains the optional fields below. Pairing sessions
leave them empty and behave exactly as today.

| Field | Meaning |
|---|---|
| `auth` | `pairing` \| `oidc` \| `oidc-device` |
| `issuer`, `subject` | `iss` + `sub`, the person's identity. Never use email as the key. |
| `email`, `display_name` | Display and audit only. |
| `role_source` | `groups` (org) or `local` (pairing / promoted). |
| `groups_digest` | Hash of the mapped groups, to detect role changes. |
| `auth_time`, `acr`, `amr` | From the last ID token. They drive step-up. |
| `fresh_until` | Last fresh ID token + `freshness`. |
| `absolute_expiry` | Sign-in time + `absolute_expiry`. |
| `idp_sid` | `sid` claim, if present. Kept for a future back-channel logout. |

### 4.2 Lifetimes side by side

| Credential | Held by | Lifetime | Notes |
|---|---|---|---|
| IdP ID token | Client, briefly | IdP default, about 1 h | Shown to the box once. The box keeps only the claims it needs. |
| IdP refresh token | Client keychain | IdP policy (Okta: rotating, idle up to days; Entra: 90 d idle, revoked on disable or "revoke sessions") | The box never sees it. |
| Box session token | Client keychain; box keeps the digest | Must be fresh every **8 h**. Idle **14 d**, absolute **30 d** (org); 90 d idle (personal) | It's still a 256-bit bearer token, so every existing route and WebSocket works unchanged. |
| Owner step-up | Box (`auth_time`) | **15 min** | Only for the routes in §5. |
| App keys (`ttq-`) | Apps | Owner-set. On org boxes, **at most 90 d**, and **paused while their creator is stale** (see §4.4) | |

### 4.3 How revocation works

**Chosen: freshness through IdP refresh.**

- An org session whose `fresh_until` has passed gets `401` with
  `WWW-Authenticate: Bearer error="session_stale"`.
- The client quietly uses its refresh token, gets a new ID token and posts it to
  `/auth/oidc/refresh`. The user never sees this unless the IdP refuses.
- The box checks:
  - signature, `iss`, `aud` and `exp`;
  - `iat` within the last 5 minutes;
  - `sub` matches the session's subject;
  - the groups map to a role.
- If all of that passes, it extends `fresh_until` and **re-derives the role**.
- If the IdP refuses the refresh (`invalid_grant`: user disabled, password reset, sessions
  revoked, removed from the app), the client can't refresh, and the session stops working on
  every box within one freshness window.
- If a user leaves the TT Station groups, the next refresh drops them to member or denies them.

**Worst-case revocation time = `freshness` + `offline_grace` (when the IdP is unreachable).**
With the defaults that is 8 h, or 80 h during an IdP outage.

The grace period is granted **only if the box itself also can't reach the IdP**. The box checks
this by fetching discovery with a short timeout. A client that just stops refreshing while the
IdP is up gets no grace.

**Options considered and not chosen as the main mechanism:**

- **OIDC back-channel logout.** The IdP must POST to each box, and boxes sit on private LANs
  behind NAT. Entra doesn't implement it at all, and Okta and Google cover it only partly. Keep
  `idp_sid` so it can be added where an org runs a reachable relay (Phase 4).
- **Validating access tokens on every request through introspection.** It needs IdP
  reachability for every request and puts IdP latency on every model call. It also fails
  closed whenever the internet is down.
- **SCIM deprovisioning to every box.** It also needs inbound reachability. It's offered
  through a central fleet service (Phase 4, optional): the service receives SCIM from the IdP
  and publishes a **signed deny-list** (subjects plus a timestamp, signed with an org key the
  box pins at enrollment), which boxes poll every 5 minutes. That brings revocation latency
  down to minutes for orgs that want it.

**Local revocation still works.** An owner can remove any device on the box, and a local
`POST /reset` signs everyone out.

### 4.4 Credentials that live outside sessions

**App keys.** On org boxes:

- each app key records the `subject` of the owner who made it;
- it has a mandatory maximum lifetime (90 d);
- it **pauses** when its creator has had no fresh session on this box for 7 days (a "dead
  man's switch" that works with any IdP);
- another owner can re-home it.

The shared "Coding sessions" key (`coding.rs`) belongs to the box (`subject = box`), and an
app key never grants owner routes.

**SSH keys from `/ssh/authorize`.** Their tag gains the subject: `ttstation:<label>:<sub>`.
Revoking a session, or a session lapsing past `absolute_expiry`, removes that subject's keys.
On org boxes the route is **off by default**, because OS access should come from IT's own
mechanisms (§9).

**People** (`people.rs`). On org boxes a person **is** the subject: name from the `name`
claim, memories keyed by `sub`. Phones no longer claim people.

## 5. Roles and MFA

**Group mapping.**

- Order of evaluation: deny if the user matches neither list; owner if any `owner_groups`
  matches; member if any `member_groups` matches.
- `{box}` expands to the box name, so owners can be per box (`tt-station-owners-qb2-lab3`) or
  org-wide (`tt-station-admins`).
- Use **app-scoped group or role claims** to avoid group overage:
  - **Okta:** a groups claim filter (`tt-station-*`).
  - **Entra:** **app roles** (`roles` claim), not `groups`, because users in more than 200
    groups get an overage pointer instead of the list.
  - **Google Workspace:** no groups claim. You'd need the Cloud Identity Groups API from a
    fleet service, which is one reason Google is the weakest fit (§12).

**MFA.** The IdP enforces MFA at sign-in for the TT Station app; that's the IdP's sign-on
policy, not ours.

**Step-up for owner actions.** The box adds step-up for **sensitive owner routes**:

- managing devices and owners;
- SSH keys;
- creating app keys;
- power, updates, reset;
- security settings;
- enroll and un-enroll;
- on shared boxes, switching the served model (see
  [code-serving.md](code-serving.md) §6).

Such a route needs `auth_time` within `owner_max_age` **and** an `acr` from `owner_acr`.

Otherwise the box answers `401` with
`WWW-Authenticate: Bearer error="insufficient_user_authentication", acr_values="…", max_age=900`
(RFC 9470). The client re-runs the flow with those parameters and `prompt=login`, then posts to
`/auth/oidc/stepup`.

How `acr` maps onto each IdP:

- **Okta:** `acr_values` (`urn:okta:loa:2fa:any`, `phr` / phishing-resistant).
- **Entra:** "authentication context", requested through a `claims` parameter with
  `acrs: c1`, returned as `acrs`. The box treats it as `acr`.

Ordinary owner reads such as `GET /audit` and `GET /devices` don't need step-up. Approvals for
agent actions don't either: they're frequent, and each one is already a human decision.

## 6. API changes (agent)

| Route | Auth | Purpose |
|---|---|---|
| `GET /auth/config` | public | `mode` (`personal`/`org`), issuer, client_id, scopes, device-grant availability, pairing policy. Contains no secrets. |
| `POST /auth/oidc/start` | public, rate-limited per IP | `{purpose: signin|stepup}` → `{challenge_id, nonce, expires_in: 300}` |
| `POST /auth/oidc/complete` | public | `{challenge_id, id_token, name, platform}` → `{token, device_id, role, fresh_until}`. It reuses `Sessions::pair` and the existing pairing audit and notification. |
| `POST /auth/oidc/refresh` | bearer | `{id_token}` → `{fresh_until, role}` |
| `POST /auth/oidc/stepup` | bearer | `{challenge_id, id_token}` → `{auth_time}` |
| `POST /auth/oidc/device`, `GET /auth/oidc/device/{id}` | public | The box runs RFC 8628: `{user_code, verification_uri_complete}`, then a poll for the session token. |
| `GET /audit/export?after=<seq>` | owner | Pull export (§7). |

Extractor changes in `routes.rs`:

- `resolve_caller` rejects stale org sessions (`AuthFailure::Stale`).
- A new `StepUpOwner` extractor wraps `OwnerAuth` for the sensitive routes.
- `Caller::actor()` carries subject and email.
- `PUBLIC_ROUTES` gains the `/auth/*` routes.

On enrolled boxes, `/pair/init` follows the `pairing` policy. Under `first-contact-only`, a
completed pair gives a TLS pin and an **unauthenticated** handle that only `/auth/oidc/*`
accepts.

## 7. Audit export

**What changes in each entry.** `audit.rs` entries gain:

- `v` (schema version);
- `seq` (monotonic per box);
- `box` (name and id);
- `org`;
- `subject`, `email`, `auth` (`pairing|oidc|oidc-device|local|app_key`), `acr`;
- `src_ip`, `user_agent`;
- `prev` (SHA-256 of the previous line). The hash chain makes deletion or editing on the box
  detectable once lines have been shipped.

New actions:

- `signin`, `signin_failed` (with reason);
- `refresh_denied`, `stale`;
- `stepup`, `stepup_failed`;
- `enroll`, `unenroll`;
- `jwks_rotated`;
- `export_gap`.

Still never logged: tokens, codes, keys or ID-token contents.

**Format.** The native JSON line is the source of truth. The exporter maps it to **OCSF**
classes:

- Authentication (3002) for sign-in, step-up and refresh;
- Account Change (3001) for role, device, key and SSH changes;
- API Activity (6003) for power, updates, model switches and denials.

That makes it usable in Splunk, Sentinel, Chronicle or Elastic without a custom parser. A CEF
mapping is a later option if a SIEM requires it.

**Transport, in order of preference:**

1. **Built-in HTTPS push.** Batched NDJSON with gzip and a bearer or HEC token from the box's
   secrets file. It retries with backoff and records its position in `audit.cursor`. Delivery
   is **at least once**, with `seq` for de-duplication. Undelivered lines survive in the
   rotated files (50 MB, about months at normal volume). If the cursor falls behind rotation,
   the exporter emits `export_gap`.
2. **Syslog over TLS** (RFC 5425, port 6514), one OCSF JSON per message, for orgs with a
   syslog collector.
3. **Org log agent.** Point Vector, Fluent Bit or the Splunk UF at `audit.jsonl`. Needs only a
   documented path and a file group. This is available in Phase 1 with no new code.

Pull (`GET /audit/export`) suits ad-hoc collection by owners.

## 8. Personal boxes and migration

**Personal boxes.** No `[org]` section means nothing changes:

- same pairing (code, QR, `local-secret`);
- same 90-day idle expiry;
- same roles.

The only differences: `GET /auth/config` returns `mode: personal`, and clients hide "Sign in
with SSO".

**Migrating an existing box into an org.** `enroll --migrate=link` is the default:

1. **Existing pairing sessions keep working, as members, for a grace period** (default
   30 days), marked `auth: pairing` and shown as "Not signed in" in Devices.
2. A client that signs in with OIDC on the same device has its pairing session **linked**:
   the same device id, upgraded in place. The person keeps their device name and their
   `people.rs` memories, which are re-keyed to `sub`.
3. **Pre-enrollment owners are demoted to member** at enrollment. After that, owners come only
   from groups. The local box user stays owner (it can read `local-secret` anyway).
4. After the grace period, unlinked pairing sessions are revoked. Each revocation is audited.
5. App keys are assigned to the enrolling owner's subject and get a 90-day expiry if they had
   none. Owners see them in a review list.
6. SSH keys installed through TT Station are listed for owner review. Nothing is removed
   automatically at enrollment.

`--migrate=clean` revokes everything at enrollment (`revoke_all`), for boxes that change hands.
Un-enrolling keeps OIDC-created sessions as plain pairing sessions with local roles. Owners get
a list of them to review.

## 9. Threat model

| Threat | Today | With org mode | Remaining risk and mitigations |
|---|---|---|---|
| **Stolen or lost laptop or phone, locked** | Tokens are in the OS keychain; hard to extract. | Same. The refresh token is in the keychain too. | IT disables the user or device in the IdP. Refresh fails and sessions die within 8 h. An owner can also remove the device immediately. |
| **Stolen laptop, unlocked** | The token works for up to 90 days idle, on every paired box. | Works until the IdP stops refreshing: at most `freshness` after IT acts, or `freshness` + `offline_grace` during an IdP outage. Owner actions need step-up, which needs the user's MFA factor. | Bearer tokens can be copied. A later option is device-bound sessions: DPoP with a Secure Enclave or TPM key. |
| **Departed employee** | Keeps access until each box's owner notices. | Loses access within 8 h. App keys pause after 7 days, SSH keys are removed when their session lapses. | OS accounts and SSH set up outside TT Station are IT's job: SSSD or LDAP, short-lived SSH certificates. Phase 4 SCIM brings this down to minutes. |
| **Compromised box (root)** | Gets session digests (useless), `local-secret` (owner here), the audit log (editable), and any traffic. | Same on this box. It also **can't** reuse ID tokens elsewhere: they're nonce-bound to this box's challenge, or arrive with `iat` within 5 minutes plus another box's session token, which it doesn't have. It never receives refresh tokens. | Shipped audit with the hash chain shows tampering after the fact. Re-image the box and revoke its TLS certificate. Re-enroll. |
| **LAN attacker** | TLS pinning protects tokens; pairing codes are brute-force limited (5 tries, 120 s). | Corporate-PKI certs (or a first-contact pin) stop challenge-relay and impersonation. PKCE stops theft of loopback codes. | Without PKI, the first-contact code is the trust step: tell users to read it off the box. Watch device-code phishing (§3.2). |
| **IdP outage** | n/a | Existing sessions keep working for `freshness` + `offline_grace` (the box confirms it can't reach the IdP). New sign-ins fail. | Break-glass: the local owner on the box (console, `tt-station-agentd pair`) can still pair a device under `pairing = owner-approved`. Audited as `breakglass`. |
| **IdP compromise or bad signing key** | n/a | The box trusts whatever the pinned issuer signs. | That's inherent to SSO. Only RS256/PS256/ES256 accepted, `kid` pinned per JWKS fetch, `jwks_rotated` audited. |
| **Malicious member** | Can do member things; member routes include `POST /serve`. | Same. On shared boxes, model switching becomes owner-only (see the code-serving doc). | Per-key quotas (`gateway.rs`) for API abuse. |

## 10. Security review checklist

- [ ] The ID token check covers signature, `iss` (exact), `aud == client_id` (and `azp` when
      several audiences), `exp`, `iat` skew ≤ 5 min, `nonce` single-use and box-issued, and an
      algorithm allow-list. A test exists for each rejection.
- [ ] JWKS: https only, optional CA pin, refetch on an unknown `kid` is rate-limited, and the
      cache survives a failed fetch.
- [ ] PKCE S256 is required on every client. `state` is checked on every redirect. The
      loopback listener binds 127.0.0.1 only, on a random port, accepts one request, and
      times out.
- [ ] No embedded webviews for IdP pages on any client.
- [ ] Refresh tokens are stored only in OS keychains, never in logs or crash reports, and never
      sent to the box.
- [ ] The box stores no IdP tokens; only the claims listed in §4.1.
- [ ] Freshness and grace: no grace while the box can reach the IdP. A `sub` mismatch on
      refresh is rejected and audited.
- [ ] Group mapping: unknown means deny, matching is exact and case-sensitive, and there's an
      Entra overage test.
- [ ] Step-up is enforced on the full list of sensitive routes in §5. A test fails if a route
      using `OwnerAuth` isn't classified as either step-up or no-step-up.
- [ ] Device grant: member-only until step-up, a notification for each, and it can be
      disabled.
- [ ] The enrolled-box pairing policy can't be bypassed through `/pair/offer` without
      `local-secret`.
- [ ] Transport: org clients refuse plain HTTP, and certificates are validated through PKI or
      the first-contact pin.
- [ ] Audit: no secrets in it, the hash chain verifies, the exporter's at-least-once delivery
      and gap handling are tested, and the SIEM token is in the secrets file at 0600.
- [ ] Migration: owners demoted, links verified on the same device id, unlinked sessions
      revoked after the grace period.
- [ ] SSH: keys tagged with the subject are removed when the session lapses or is revoked.
      `/ssh/authorize` is off by default on org boxes.
- [ ] App keys: creator subject recorded, maximum lifetime enforced, pausing works.
- [ ] Dependencies: `cargo deny` passes with `openidconnect`/`oauth2` on rustls+ring, and the
      npm `openid-client` licence is recorded.
- [ ] Pen test: challenge relay, ID-token replay between boxes, nonce reuse, clock skew, and
      `alg` confusion.

## 11. Phased plan

| Phase | Scope | Effort (eng-weeks) | Exit criteria |
|---|---|---|---|
| **0. Decide and register** | The decisions in §12. Register the IdP app (redirects, device grant, group or role claims). Threat-model sign-off. | 0.5–1 (plus IT time) | A test tenant on Okta or Entra issues tokens with the agreed claims. |
| **1. Org sign-in, MVP** | `[org]` config and `enroll`; JWKS cache; ID-token checks (`openidconnect`); `/auth/*` routes; session store v4 (`subject`, `fresh_until`…); group→role; freshness + grace; audit fields. Desktop loopback + PKCE; CLI loopback and box-run device grant. | 4–5 | A desktop and the CLI sign in on a test box, the role follows a group change within 8 h, and a disabled user is cut off within 8 h. Personal-box tests unchanged. |
| **2. iOS, step-up, credential hygiene** | iOS `ASWebAuthenticationSession`; RFC 9470 step-up and the `StepUpOwner` extractor; app-key ownership and pausing; SSH keys tagged with the subject; `people.rs` keyed by subject; migration (`--migrate=link`). | 3–4 | Owner actions prompt for MFA. An existing paired fleet box migrates cleanly. |
| **3. Audit export and PKI** | Hash chain; HTTPS and syslog-TLS exporter with OCSF mapping; `GET /audit/export`; corporate-PKI certificates via ACME at enrollment, with client validation through the OS store. | 2–3 | Events reach the SIEM within a minute and survive a 1-day network cut. Clients connect to an org box with no pin. |
| **4. Optional: fleet service** | A small central service: a SCIM 2.0 receiver, a signed deny-list feed that boxes poll every 5 min, optional back-channel logout relay, and box inventory. DPoP device binding. | 3–4 + ops | Deprovisioning takes effect in under 10 minutes. |

Phases 1–3 total about **10–13 eng-weeks**. Phase 4 is separate and needs an owner to run the
service.

## 12. Decisions needed

1. **IdP.** Okta, Entra ID, or Google Workspace.
   - **Recommendation:** whichever already holds employee MFA. Okta and Entra both support
     everything here.
   - **Entra** means app roles, authentication contexts for step-up, and possibly MSAL on iOS
     for device-compliance CA.
   - **Google Workspace** has no groups claim and no `acr` step-up. It would need Phase 4 for
     groups and would lose per-action MFA.
2. **Group or role names.** Proposed: `tt-station-admins` (owner on every box),
   `tt-station-users` (member on every box), and per-box `tt-station-owners-<box>` and
   `tt-station-users-<box>`. Decide whether membership by default (any employee is a member)
   is acceptable.
3. **Freshness window.** Proposed **8 h**: the worst-case revocation time when the IdP is up.
   A shorter window means more silent refreshes; a longer one means slower revocation.
4. **Offline grace period.** Proposed **72 h**, covering a weekend IdP or WAN outage. Lab
   networks that are isolated by design may want longer, and high-security boxes `0`, which
   fails closed.
5. **Owner step-up.** `max_age` (proposed 15 min) and the required `acr` (proposed
   phishing-resistant: FIDO2 or passkey).
6. **Device grant for the CLI.** Allow it, as member-only until step-up, or require
   loopback/browser only.
7. **Pairing on enrolled boxes.** Proposed `first-contact-only`, or `off` where corporate PKI
   is in place. Plus whether break-glass owner pairing through the local console is
   acceptable.
8. **Box TLS.** Does every QuietBox get a corporate-PKI certificate (needs DNS names and an
   ACME-capable internal CA), or do we accept first-contact pins?
9. **Migration policy.** `link` with a 30-day grace (proposed) or `clean`. And whether
   pre-enrollment owners are demoted (proposed: yes).
10. **SIEM and transport.** Which SIEM; HTTPS/HEC or syslog-TLS; OCSF accepted as the format;
    retention on the box.
11. **Phase 4.** Is minutes-level revocation (SCIM plus a deny-list service) required, or is
    8 h acceptable? This decides whether someone has to run a central service.
