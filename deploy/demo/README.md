# Peckboard Demo Box for App Review

A throwaway Peckboard install for Apple App Review: the **mock** AI provider
only, fictional seeded content, no real data or credentials. It listens on
`127.0.0.1` only; reviewers reach it through Remote Access
(`relay.peckboard.com`). Remote Access dials **out**, so the VM needs no
inbound ports beyond SSH.

| File                     | Purpose                                                           |
| ------------------------ | ----------------------------------------------------------------- |
| `peckboard-demo.service` | Hardened systemd unit: loopback only, mock-only, no auto-start    |
| `seed_demo.py`           | Idempotent seeder: setup, reviewer user, content, pairing, checks |

Layout on the VM (user `peckboard`, home `/opt/peckboard-demo`):

```
/opt/peckboard-demo/bin/peckboard     release binary (read-only to the service)
/opt/peckboard-demo/data/             data dir (DB, keys, reports)
/opt/peckboard-demo/workspaces/       fake project folders the demo points at
/opt/peckboard-demo/seed_demo.py
```

## Deploy

1. Install the binary (0.1.57, linux x86_64) and verify its checksum:

   ```bash
   sudo -u peckboard mkdir -p /opt/peckboard-demo/bin /opt/peckboard-demo/workspaces
   cd /opt/peckboard-demo/bin
   base=https://github.com/PeckBoard/peckboard/releases/download/0.1.57
   sudo -u peckboard curl -fLO "$base/peckboard-linux-x86_64"
   sudo -u peckboard curl -fLO "$base/peckboard-linux-x86_64.sha256"
   sha256sum -c peckboard-linux-x86_64.sha256      # must print: OK
   sudo -u peckboard mv peckboard-linux-x86_64 peckboard
   sudo chmod 0755 peckboard
   ```

2. Install the unit and the seeder, then start:

   ```bash
   sudo install -m 0644 peckboard-demo.service /etc/systemd/system/
   sudo install -o peckboard -g peckboard -m 0755 seed_demo.py /opt/peckboard-demo/
   sudo systemctl daemon-reload
   sudo systemctl enable --now peckboard-demo
   curl -fsS http://127.0.0.1:3344/api/health
   ```

   The first boot creates the data dir, installs **only** the mock provider,
   and prints a random admin password **once** to the journal
   (`journalctl -u peckboard-demo`, the "FIRST-RUN ADMIN ACCOUNT" box). If
   you miss it, mint a new one:

   ```bash
   sudo systemctl stop peckboard-demo
   sudo -u peckboard /opt/peckboard-demo/bin/peckboard \
       --data-dir /opt/peckboard-demo/data --reset-password --user admin
   sudo systemctl start peckboard-demo
   ```

   Keep the admin password to yourself; reviewers never get it.

3. Seed (as `peckboard`, because it writes the fake workspace files):

   ```bash
   read -rs PECKBOARD_ADMIN_PASSWORD && export PECKBOARD_ADMIN_PASSWORD
   sudo -u peckboard --preserve-env=PECKBOARD_ADMIN_PASSWORD \
       python3 /opt/peckboard-demo/seed_demo.py --self-test
   ```

   On its first run, the seeder prints two secrets, **once each**: the
   reviewer password (`appreview`) and the `peckboard://pair/...` link for
   the "App Review" pairing. Paste both straight into App Store Connect →
   App Review Information. Store them nowhere else. Re-running the seeder is
   safe: it skips what already exists, and neither secret can be shown
   again.

   It also completes first-run setup, sets the agent sandbox to `enforce`,
   sets the default model to `mock:happy-path`, and refuses to continue if
   any non-mock model is registered. `--self-test` logs in as the reviewer
   and checks these things:
   - the reviewer is a non-admin, and the Remote Access API returns 403;
   - only mock models are listed;
   - the reviewer sees the 6 sessions, 2 projects, and the report;
   - a live mock reply works;
   - Remote Access is on, with one pairing.

**What the reviewer does:** open the app, then open the pairing link (or
scan its QR code). The app tunnels to this box. Sign in as `appreview` with
the password. They see two folders, two paused projects with cards in
every workflow step, and six sessions. Any message they send gets a
scripted mock reply. The reviewer never sees the first-run wizard, because
it is admin-only and already completed.

## Rotate the Reviewer Password

```bash
sudo -u peckboard --preserve-env=PECKBOARD_ADMIN_PASSWORD \
    python3 /opt/peckboard-demo/seed_demo.py --rotate-reviewer-password
```

This prints the new password once. All of the reviewer's sessions are
signed out.

## Revoke and Recreate the Pairing

```bash
sudo -u peckboard --preserve-env=PECKBOARD_ADMIN_PASSWORD \
    python3 /opt/peckboard-demo/seed_demo.py --new-pairing
```

This revokes every "App Review" pairing, drops its live tunnel, and prints a
new link once. To shut remote access off entirely, for example after review:

```bash
sudo -u peckboard --preserve-env=PECKBOARD_ADMIN_PASSWORD \
    python3 /opt/peckboard-demo/seed_demo.py --disable-remote-access
```

## Wipe and Reseed

> **Warning:** these steps permanently delete everything on the demo box.
> The reviewer password and pairing link both change, so update App Store
> Connect afterwards.

1. Stop the service and delete the data and workspaces:

   ```bash
   sudo systemctl stop peckboard-demo
   sudo rm -rf /opt/peckboard-demo/data /opt/peckboard-demo/workspaces
   sudo -u peckboard mkdir -p /opt/peckboard-demo/workspaces
   ```

2. Start the service. The fresh data dir gets a new admin password (see the
   journal) and mock-only providers:

   ```bash
   sudo systemctl start peckboard-demo
   ```

3. Seed again, as in Deploy step 3.

## Notes

- **Mock-only is fixed at first boot.** `PECKBOARD_PREINSTALL_PLUGINS=mock`
  is only read on an empty data dir, and the installed set is then saved.
  Never install another provider from Settings → Plugins on this box. If
  one is installed anyway, the seeder refuses to run.
- **No auto-started agents.** `PECKBOARD_NO_RESUME=1` turns off the worker
  orchestrator, repeating tasks, restart resume, and the login keep-alive.
  The projects are also paused, so the seeded board never changes by itself.
- **Voice is off.** `PECKBOARD_TTS_DOWNLOAD=0` skips the ~300 MB Kokoro
  model download. Voice playback isn't needed for review.
- **Some mock models run real tools.** `mock:mcp`, `mock:run-command`, and
  `mock:sandbox-probe` run real Peckboard tools as `peckboard`, for example
  `mock:mcp` with an embedded `run_command` block. Treat the reviewer
  credentials as shell access to this VM. The Landlock agent sandbox
  (`enforce`) and the systemd hardening contain it. Keep nothing else on
  the VM.
