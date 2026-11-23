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
