# PeckBoard Skills Hub

The public skills server. PeckBoard instances read the public catalog from
it (`/index.json`), publish `SKILL.md` files to it, fork each other's skills
and send merge requests back — every write signed with the instance's
Ed25519 publisher key. Every publish runs through the same parser and
proprietary-content check PeckBoard itself uses (`peckboard-skill-format`),
so a skill PeckBoard accepts locally is judged the same way here.

Single static binary: Axum, SQLite (bundled, WAL), git storage (vendored
libgit2), TLS 1.3 with a Let's Encrypt certificate obtained over
TLS-ALPN-01 on the same `:443` listener.

## API

[`API.md`](API.md) is the contract between PeckBoard and the hub (handles,
signed headers, skill info / history / diff, forks, merge requests). The
original catalog and publish endpoints:

| Method | Path                                | Notes                                                                                                                                      |
| ------ | ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| GET    | `/index.json`                       | `{schema:1, generated_at, skills:[…], groups:[…]}`, `Cache-Control: max-age=60`; forks are not listed, each skill carries `forks: <count>` |
| GET    | `/skills/<name>/<version>/SKILL.md` | `text/markdown`; also fork versions (`<name>@<handle>`); yanked versions are still served, taken-down ones are not                         |
| GET    | `/groups/<slug>.json`               | `{slug, name, description, skills}`                                                                                                        |
| GET    | `/api/challenge?public_key=<b64>`   | `{nonce, expires_in: 300, pow_difficulty}` — 20 bits for a never-seen key, else 0                                                          |
| POST   | `/api/publish`                      | `{skill_md, public_key, nonce, pow?, signature}` → 201 `{name, version, url}`                                                              |
| POST   | `/api/yank`                         | `{name, version, public_key, nonce, signature}` → 200; owner only                                                                          |

Signatures are Ed25519 over the UTF-8 bytes of:

```text
publish: "peckboard-skills-publish-v1\n" + nonce + "\n" + lowercase hex sha256(skill_md)
yank:    "peckboard-skills-yank-v1\n"    + nonce + "\n" + name + "@" + version
other:   "peckboard-skills-request-v1\n" + METHOD + "\n" + PATH + "\n" + nonce + "\n" + hex sha256(body)
```

Every other write sends that last signature in headers (`X-Peck-Key`,
`X-Peck-Nonce`, `X-Peck-Pow`, `X-Peck-Signature`); `PATH` is the request
path as sent, without the query string. Nonces are single use, bound to the
key they were issued to, valid for 5 minutes, and held in memory only (a
restart invalidates them). Proof of work: `pow` such that
`sha256(nonce + pow)` (the nonce string exactly as issued) has at least
`pow_difficulty` leading zero bits.

Errors are `{error, code, findings?}`:

| HTTP | `code`             | When                                                                           |
| ---- | ------------------ | ------------------------------------------------------------------------------ |
| 400  | `bad_request`      | malformed JSON/key, invalid frontmatter, name, semver or handle                |
| 401  | `nonce_invalid`    | unknown, expired, reused, or other key's nonce                                 |
| 401  | `bad_signature`    | signature doesn't verify, or a signed header is missing                        |
| 403  | `pow_required`     | missing / insufficient proof of work                                           |
| 403  | `banned`           | key banned by an admin                                                         |
| 403  | `not_owner`        | yank / fork publish / sync / merge / comment / close by someone not allowed to |
| 404  | `not_found`        | unknown skill, fork, version, revision, merge request or group                 |
| 409  | `name_taken`       | the name belongs to another key                                                |
| 409  | `version_exists`   | that version was already published (yanked ones included)                      |
| 409  | `handle_taken`     | another key has that handle                                                    |
| 409  | `already_forked`   | this key already forked that skill                                             |
| 409  | `already_open`     | the fork already has an open merge request                                     |
| 409  | `not_mergeable`    | the merge request conflicts, has nothing new, or isn't open                    |
| 413  | `too_large`        | `skill_md` over 512 KiB                                                        |
| 422  | `content_findings` | the content check found credentials / internal details                         |
| 429  | `rate_limited`     | over 20 successful publishes for this key today (UTC)                          |

Groups named in a skill's `groups:` frontmatter are created on first use,
owned by that first publisher; any publisher's skill may list an existing
group. Fork publishes never create groups.

## Storage

Each skill is a bare git repo, `repos/<name>.git`, and each fork one more,
`repos/forks/<name>@<handle>.git`, holding a single `SKILL.md` on `main`.
Every publish is a commit on `main` (author `<handle>
<<handle>@keys.skills.peckboard.com>`; the signing key is recorded per
commit in SQLite) tagged `v<version>`. A fork starts as a fetch of the
original's `main`, so the two share history; after each write to a fork,
its `main` is fetched into the original as `refs/forks/<fork>`, which is
where ahead/behind, mergeability and merges are computed. Merges (fork
sync, merge requests) are 3-way merges of `SKILL.md` with the frontmatter
`name` and `version` set aside — they always differ between a skill and its
forks — and set by whoever writes the result. Writes to a repo are
serialized. Git is never served over the network.

Yank and takedown are flags in SQLite; history is never rewritten.
`index.json` is built from SQLite and git and cached in memory until the
database changes (including changes made by the admin CLI).

Data dirs from the blob-storage release (`blobs/<sha256>` + version rows)
are migrated on startup (and by any `admin` command): each skill without a
repo gets one, built from its stored versions in published order — one
commit (dated at the original publish time) and tag each. The `blobs/` dir
is left in place; delete it by hand once you're happy.

## Build and Test

```bash
cargo test --manifest-path peckboard-skills-hub/Cargo.toml
cargo run --manifest-path peckboard-skills-hub/Cargo.toml -- \
  --http-only --listen 127.0.0.1:8099 --data-dir "$(mktemp -d)"
```

`--http-only` serves plain HTTP (no ACME) for local testing or behind a
TLS-terminating proxy; returned URLs then default to `http://<listen>`.

## Deploy

```bash
peckboard-skills-hub/deploy.sh user@host              # build static musl, install, restart
peckboard-skills-hub/deploy.sh user@host --no-restart
peckboard-skills-hub/deploy.sh user@host --rollback   # swap back to the previous binary
```

The script builds `x86_64-unknown-linux-musl` (`cargo zigbuild`, or the
repo's `tmp-musl-tools` cross toolchain — SQLite and libgit2 are compiled
from source, so a musl C compiler is needed), refuses a non-static binary,
installs `/opt/peckboard-skills-hub/peckboard-skills-hub` (root-owned; the
previous binary is kept as `.prev`) and the hardened unit
`peckboard-skills-hub.service`, running as the `peckskills` system user.
DNS for the domain must point at the host and `443/tcp` must be open before
the first start, so Let's Encrypt can validate.

### Configuration

`/etc/peckboard-skills-hub.env` (root, 0600):

| Variable                             | Default                          | Meaning                                                                  |
| ------------------------------------ | -------------------------------- | ------------------------------------------------------------------------ |
| `PECKBOARD_SKILLS_DOMAIN`            | `skills.peckboard.com`           | certificate name and host of returned URLs                               |
| `PECKBOARD_SKILLS_ACME_EMAIL`        | —                                | Let's Encrypt account contact                                            |
| `PECKBOARD_SKILLS_ACME_STAGING`      | `false`                          | use the Let's Encrypt staging directory                                  |
| `PECKBOARD_SKILLS_PUBLIC_URL`        | `https://<domain>`               | base of returned URLs                                                    |
| `PECKBOARD_SKILLS_NEW_KEY_POW_BITS`  | `20`                             | PoW bits for a never-seen key                                            |
| `PECKBOARD_SKILLS_PUBLISHES_PER_DAY` | `20`                             | successful publishes per key per UTC day (merges that publish count too) |
| `PECKBOARD_SKILLS_ADMIN_KEY`         | generated `<data-dir>/admin.key` | publisher key (base64) that `admin import` assigns                       |
| `PECKBOARD_SKILLS_HUB_ARGS`          | —                                | extra flags                                                              |
| `RUST_LOG`                           | `info`                           | log filter                                                               |

Flags: `--domain`, `--data-dir` (default `/var/lib/peckboard-skills-hub`),
`--acme-email`, `--acme-staging`, `--http-only`, `--listen`, `--public-url`,
`--new-key-pow-bits`, `--publishes-per-day`.

### Behind a TLS Proxy (How skills.peckboard.com Runs)

The production VM (`skills-01`) has no public IP: the NPM load balancer
terminates TLS and forwards to the hub over plain HTTP. Its env file:

```sh
PECKBOARD_SKILLS_DOMAIN=skills.peckboard.com
PECKBOARD_SKILLS_HTTP_ONLY=true
PECKBOARD_SKILLS_LISTEN=0.0.0.0:8080
PECKBOARD_SKILLS_PUBLIC_URL=https://skills.peckboard.com
PECKBOARD_SKILLS_HUB_ARGS=
```

The unit's `IPAddressDeny` (no private ranges, an SSRF guard) filters both
directions, so the proxy's LAN address is allowed explicitly with
`IPAddressAllow`. Change that line if the proxy moves. The hub does no
per-IP logic (limits are per signing key), so proxy headers aren't needed.

### Data Dir

```text
/var/lib/peckboard-skills-hub/
  hub.sqlite (+ -wal, -shm)   publishers + handles, skills + forks, versions, groups,
                              merge requests + comments, commit authors, audit_log
  repos/<name>.git            one bare repo per skill
  repos/forks/<fork>.git      one bare repo per fork
  backups/hub-<ts>.sqlite     online SQLite backup every 24 h, newest 7 kept
  backups/hub-<ts>.repos.tar  tarball of repos/ taken right after it, newest 7 kept
  acme/                       Let's Encrypt account + certificate cache
  admin.key                   generated admin signing seed (0600), if no env key
  blobs/<sha256>              (pre-git data dirs only; migrated, then unused)
```

The repos tarball is written with git writes paused, right after the SQLite
backup, so every commit that backup refers to is in it. Off-host backups:
copy the newest `hub-<ts>.sqlite` and `hub-<ts>.repos.tar` pair. To
restore, stop the service, put the SQLite file back as `hub.sqlite` (drop
any `-wal` / `-shm`) and unpack the tarball in the data dir (it holds
`repos/…`).

## Admin Commands

Run on the server as the service user, against the live data dir (safe
while the hub runs):

```bash
sudo -u peckskills /opt/peckboard-skills-hub/peckboard-skills-hub admin list
sudo -u peckskills /opt/peckboard-skills-hub/peckboard-skills-hub admin ban <base64-key>
sudo -u peckskills /opt/peckboard-skills-hub/peckboard-skills-hub admin unban <base64-key>
sudo -u peckskills /opt/peckboard-skills-hub/peckboard-skills-hub admin takedown <name> [version]
sudo -u peckskills /opt/peckboard-skills-hub/peckboard-skills-hub admin import /path/to/skills-checkout
sudo -u peckskills /opt/peckboard-skills-hub/peckboard-skills-hub admin transfer <name> <base64-key>
```

- `takedown` removes the version (or every version) from the index and
  stops serving it (a fully taken-down skill also disappears from the info,
  history, forks and diff reads; a taken-down version can't be diffed); the
  name and version numbers stay reserved and git history is kept.
- `import` reads a PeckBoard/skills checkout
  (`skills/<name>/<version>/SKILL.md` + `groups/*.json`), owned by the admin
  key, committing each new version (oldest first) to the skill's repo.
  Existing versions are left alone; content-check findings are printed as
  warnings (imports are trusted).
- `transfer <name> <base64-key>` hands an original skill to another
  publisher key — e.g. a skill imported under the admin key back to the
  install that wrote it (its key is shown in PeckBoard under Settings →
  Skills → Merge requests).

Every publish, yank, handle change, fork, sync, merge request open / merge /
close, ban, unban, takedown, transfer, import and migration is recorded in
`audit_log`.
