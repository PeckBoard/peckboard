# Skills Hub API

The contract between PeckBoard and `skills.peckboard.com`. Every skill and
every fork is a bare git repo on the hub; git is never exposed directly.
All reads are public. All writes are signed by the caller's install key
(Ed25519, the box identity key).

## Identity and Signing

- `public_key`: standard base64 of the 32-byte Ed25519 public key.
- `GET /api/challenge?public_key=<key>` → `{nonce, expires_in, pow_difficulty}`.
  Nonces are single-use. `pow_difficulty` is non-zero only for a key the hub
  has never seen.
- Proof of work (only when `pow_difficulty > 0`): `pow` is any string such
  that `sha256(nonce + pow)` has at least `pow_difficulty` leading zero bits
  (`nonce` exactly as received).

### `POST /api/publish` and `POST /api/yank` (Unchanged)

As already implemented: JSON body with `public_key`, `nonce`, `pow?`,
`signature`; publish signs `"peckboard-skills-publish-v1\n" + nonce + "\n" +
hex(sha256(skill_md))`, yank signs `"peckboard-skills-yank-v1\n" + nonce +
"\n" + name + "@" + version`.

Publishing to a fork: the SKILL.md frontmatter `name` is the fork name
(`<name>@<handle>`); only the fork's owner may publish to it.

### Every Other Write: Signed Headers

| Header             | Value                        |
| ------------------ | ---------------------------- |
| `X-Peck-Key`       | `public_key`                 |
| `X-Peck-Nonce`     | nonce from `/api/challenge`  |
| `X-Peck-Pow`       | proof of work, when required |
| `X-Peck-Signature` | base64 Ed25519 signature     |

Signed message (UTF-8): `"peckboard-skills-request-v1\n" + METHOD + "\n" +
PATH + "\n" + nonce + "\n" + hex(sha256(raw request body))`, where `PATH`
is the request path without the query string and the body is `""` when
empty.

## Errors

JSON `{error, code, findings?}`, `error` in plain words. Codes:
`bad_request` 400, `bad_signature` 401, `nonce_invalid` 401,
`pow_required` 403, `banned` 403, `not_owner` 403, `not_found` 404,
`name_taken` 409, `version_exists` 409, `handle_taken` 409,
`already_forked` 409, `not_mergeable` 409, `too_large` 413,
`content_findings` 422, `rate_limited` 429.

## Handles

- `POST /api/handle` (signed) `{handle}` → `{handle}`. `[a-z0-9-]{2,32}`,
  unique. A key without a chosen handle gets `k-` + the first 8 hex of
  `sha256(public_key bytes)`.
- `GET /api/publishers/<public_key>` → `{public_key, handle}`.

## Reads (Public)

- `GET /index.json`, `GET /skills/<name>/<version>/SKILL.md`,
  `GET /groups/<slug>.json`: unchanged. Forks are **not** top-level
  entries in `index.json`; each skill entry gains `forks: <count>`.
  `/skills/<fork>/<version>/SKILL.md` serves fork versions.
- `GET /api/skills/<name>` → `{name, owner_handle, owner_key, parent?
(fork's original), latest, versions, description, tags, groups}`
  (works for skills and forks).
- `GET /api/skills/<name>/history` → `[{commit, parents, author_key,
author_handle, message, at, tag?}]`, newest first.
- `GET /api/skills/<name>/forks` → `[{fork, owner_handle, owner_key,
latest, ahead, behind}]`.
- `GET /api/skills/<name>/diff?from=<rev>&to=<rev>` → `{from, to, diff}`.
  `rev` = a commit id, a version (`1.2.0`), or `main`. `diff` is a unified
  diff of SKILL.md.

## Forks (Signed)

- `POST /api/fork` `{name}` → 201 `{fork, url}`. One fork per key per skill
  (`already_forked`). A fork can't be forked.
- `POST /api/forks/<fork>/sync` → `{status}`, one of `up_to_date`,
  `fast_forward`, `merged`, `conflict` (nothing changed on conflict).
  Owner only.

## Merge Requests

- `POST /api/merge-requests` (signed, fork owner) `{fork, title,
description}` → 201 `<MergeRequest>`. At most one open MR per fork.
- `GET /api/merge-requests?target=<name>&state=open|merged|closed|all` and
  `GET /api/merge-requests?key=<public_key>&role=owner|author&state=…`
  (`owner` = MRs into skills this key owns; `author` = MRs this key
  opened) → `[<MergeRequest> without diff and comments]`.
- `GET /api/merge-requests/<id>` → `<MergeRequest>`.
- `POST /api/merge-requests/<id>/comments` (signed, target owner or MR
  author) `{body}` → 201 `{id, author_handle, author_key, body, at}`.
- `POST /api/merge-requests/<id>/merge` (signed, target owner)
  `{publish: bool, bump?: "patch"|"minor"|"major", version?: string}` →
  `<MergeRequest>` with `state: "merged"`, `merged_commit`, and
  `merged_version` when `publish`. When publishing, the merged SKILL.md's
  frontmatter `name` is rewritten to the target's name and `version` to the
  new version, and the content check runs before the tag is made
  (`content_findings`). `not_mergeable` when it no longer merges cleanly.
- `POST /api/merge-requests/<id>/close` (signed, target owner or MR author)
  → `<MergeRequest>` with `state: "closed"`.

`<MergeRequest>`:

```json
{
  "id": 12,
  "target": "pr-review",
  "fork": "pr-review@alice",
  "title": "...",
  "description": "...",
  "state": "open",
  "author_key": "...",
  "author_handle": "alice",
  "target_owner_key": "...",
  "base_commit": "...",
  "head_commit": "...",
  "mergeable": true,
  "diff": "unified diff (detail only)",
  "comments": [
    {
      "id": 1,
      "author_handle": "...",
      "author_key": "...",
      "body": "...",
      "at": "..."
    }
  ],
  "created_at": "...",
  "updated_at": "...",
  "merged_commit": null,
  "merged_version": null
}
```

New commits published to the fork update an open MR's `head_commit`,
`diff` and `mergeable` automatically.

## Teams (`team.peckboard.com`)

A team is a set of PeckBoard servers. There are no accounts: a member is a
server, named by its install key (`public_key`, standard padded base64 —
the same key and text form as everywhere above). Same program and same
signed headers as the skills endpoints; `team.peckboard.com` is another
name for the same hub.

The hub is storage and a gatekeeper, not the authority. A team is its
**membership log**, and every member server replays and checks the whole
log itself (`peckboard-team-format` holds the format and the rules, shared
by both sides). The hub refuses a record that breaks the rules, but it has
no key a member would accept, so it cannot add a server to a team.

All team reads are signed and members-only. Signed `GET`s sign the empty
body. Ids in paths: a team id is 32 hex chars; invite and request ids are
`[A-Za-z0-9_-]`.

### Membership Log

A record: `{seq, prev, at, body, signer, signature}`.

- `body`: the change, as the exact JSON text that was signed. One of
  `{"type":"create","name","founder","label","salt"}`,
  `{"type":"add","key","role","label"}`, `{"type":"remove","key"}`,
  `{"type":"set_role","key","role"}`, `{"type":"rename","name"}`.
  `role` is `owner`, `admin` or `member`.
- `signature`: standard padded base64 Ed25519 signature by `signer` over
  `"peckboard-team-log-v1\n" + seq + "\n" + prev + "\n" + at + "\n" +
hex(sha256(body))`. Keys and signatures must be in exactly this text form.
- `prev`: `hex(sha256(message + "\n" + signer + "\n" + signature))` of the
  record before it (`message` = that record's signed text); empty for
  `seq` 0.
- The team id is the first 32 hex chars of
  `sha256("peckboard-team-id-v1\n" + body of record 0)`.

Rules: record 0 is `create`, signed by `founder`, who becomes an owner.
Owners may do everything. Admins may add and remove plain members. Any
member may remove itself. A team always keeps at least one owner. At most
200 members.

A member server remembers the newest `(length, head hash)` it has verified
and refuses a log that does not continue it, so hiding a removal by serving
older history is detected.

### Endpoints

- `POST /api/teams` (signed) `{record}` → 201 `{team_id, name}`. The record
  must be a valid record 0 signed by the caller. At most 20 teams per
  founding key.
- `GET /api/teams` (signed) → `{teams: [{team_id, name, role, members}]}`:
  the caller's teams.
- `GET /api/teams/<id>/log` (signed, member) → `{team_id, records}`.
- `POST /api/teams/<id>/log` (signed, member) `{record, request_id?}` →
  `{team_id, len, head}`. The record must be the next one and signed by the
  caller. With `request_id`, an `add` for that join request's key approves
  it and uses up its invite.
- `POST /api/teams/<id>/invites` (signed, owner/admin)
  `{invite_id, gate, expires_at}` → 201. At most 7 days ahead, 20 open
  invites per team. `GET` (signed, owner/admin) lists open invites;
  `DELETE /api/teams/<id>/invites/<invite_id>` withdraws one.
- `GET /api/teams/<id>/preview` with header `X-Peck-Invite:
<invite_id>.<gate>` (not signed) → `{team_id, name, members}`.
- `POST /api/teams/<id>/join` (signed by the joining server)
  `{invite_id, gate, proof, label}` → 201 `{request_id, team_id, name}`.
  One invite accepts up to 5 open requests.
- `GET /api/teams/<id>/requests` (signed, owner/admin) →
  `{requests: [{request_id, invite_id, key, label, proof, created_at,
invited_by}]}`.
- `GET /api/teams/<id>/requests/<request_id>` (signed by the server that
  filed it) → `{request_id, state}`, `state` = `pending`, `approved` or
  `rejected`. `DELETE` on the same path (same signer) takes the request
  back → 204.
- `POST /api/teams/<id>/requests/<request_id>/reject` (signed,
  owner/admin) → 204.

### Invites

The inviting server makes up `invite_id` and a random `secret` and builds
the link `<hub>/join#<team_id>.<invite_id>.<secret>`. The secret is never
sent to the hub:

- `gate = hex(sha256("peckboard-team-gate-v1\n" + secret))` is what the hub
  stores and what a joiner presents. It shows the joiner holds the link.
- `proof = hex(hmac_sha256(secret, "peckboard-team-join-v1\n" + team_id +
"\n" + joiner_key))` is opaque to the hub. The inviting server checks it:
  the request came from someone holding the link, for exactly this key.
- The verification code both people compare before approval is the first
  four bytes (big-endian) of `hmac_sha256(secret, "peckboard-team-verify-
v1\n" + team_id + "\n" + joiner_key)` mod 1,000,000, shown as `123 456`.
  It depends on the joining key, so someone else who got the link sees a
  different code.

Only the server that created an invite holds its secret, so only it can
check a request's proof and approve it.

### Errors

Additional codes: `not_member` 403 (also the answer for a team that does
not exist), `not_allowed` 403, `invite_invalid` 403, `already_member` 409,
`log_conflict` 409 (another change landed first: reload and retry),
`team_exists` 409, `invite_exists` 409, `bad_record` 422.
