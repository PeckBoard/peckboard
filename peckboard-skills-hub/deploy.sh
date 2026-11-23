#!/usr/bin/env bash
# Build a static musl peckboard-skills-hub and install it on HOST over SSH.
#
#   peckboard-skills-hub/deploy.sh user@host [--no-restart | --rollback]
#
# Requires: the x86_64-unknown-linux-musl Rust target, plus either
# `cargo zigbuild`, the repo's musl cross toolchain under
# tmp-musl-tools/x86_64-linux-musl-cross, or CC_x86_64_unknown_linux_musl
# (SQLite and libgit2 are vendored C, so a musl C compiler is needed). The remote user
# needs sudo. Extra ssh/scp options go in $SSH_OPTS. Firewall: open 443/tcp.
#
# Installs /opt/peckboard-skills-hub/peckboard-skills-hub (previous binary
# kept as peckboard-skills-hub.prev; root:peckskills, not writable by the
# service user) and the systemd unit. Configuration lives in
# /etc/peckboard-skills-hub.env (root, 0600) — see README.md. Data lives in
# /var/lib/peckboard-skills-hub (systemd StateDirectory, 0700).
set -euo pipefail

HOST="${1:?usage: deploy.sh user@host [--no-restart | --rollback]}"
MODE="${2:-install}"
# shellcheck disable=SC2206
SSH_OPTS=(${SSH_OPTS:-})

cd "$(dirname "$0")"

if [[ "$MODE" == "--rollback" ]]; then
  ssh "${SSH_OPTS[@]}" "$HOST" "sudo bash -s" <<'REMOTE'
set -euo pipefail
cd /opt/peckboard-skills-hub
[[ -f peckboard-skills-hub.prev ]] || { echo "no previous binary" >&2; exit 1; }
mv -f peckboard-skills-hub.prev peckboard-skills-hub.tmp
[[ -f peckboard-skills-hub ]] && mv -f peckboard-skills-hub peckboard-skills-hub.prev
mv -f peckboard-skills-hub.tmp peckboard-skills-hub
systemctl restart peckboard-skills-hub
sleep 1
systemctl --no-pager --lines=5 status peckboard-skills-hub || true
REMOTE
  exit 0
fi

RESTART=1
[[ "$MODE" == "--no-restart" ]] && RESTART=0

TARGET="${TARGET:-x86_64-unknown-linux-musl}"
MUSL_BIN="$(cd .. && pwd)/tmp-musl-tools/x86_64-linux-musl-cross/bin"
if cargo zigbuild --version >/dev/null 2>&1; then
  cargo zigbuild --release --locked --target "$TARGET"
else
  if [[ -z "${CC_x86_64_unknown_linux_musl:-}" && -x "$MUSL_BIN/x86_64-linux-musl-gcc" ]]; then
    export CC_x86_64_unknown_linux_musl="$MUSL_BIN/x86_64-linux-musl-gcc"
    export AR_x86_64_unknown_linux_musl="$MUSL_BIN/x86_64-linux-musl-ar"
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$MUSL_BIN/x86_64-linux-musl-gcc"
  fi
  cargo build --release --locked --target "$TARGET"
fi
BIN="target/$TARGET/release/peckboard-skills-hub"

if command -v file >/dev/null 2>&1; then
  case "$(file -b "$BIN")" in
    *"statically linked"* | *"static-pie linked"*) ;;
    *) echo "refusing to deploy: $BIN is not statically linked" >&2; exit 1 ;;
  esac
fi

echo "==> uploading to $HOST"
scp -q "${SSH_OPTS[@]}" "$BIN" deploy/peckboard-skills-hub.service "$HOST:/tmp/"

echo "==> installing"
ssh "${SSH_OPTS[@]}" "$HOST" "sudo RESTART=$RESTART bash -s" <<'REMOTE'
set -euo pipefail
if ! id peckskills >/dev/null 2>&1; then
  useradd --system --no-create-home --home-dir /var/lib/peckboard-skills-hub \
    --shell /usr/sbin/nologin peckskills
fi
# Root owns the install dir and everything in it; the service user can read
# and execute but never replace its own binary. Its only writable place is
# the systemd-managed StateDirectory=/var/lib/peckboard-skills-hub.
install -d -m 0750 -o root -g peckskills /opt/peckboard-skills-hub
chown root:peckskills /opt/peckboard-skills-hub
chmod 0750 /opt/peckboard-skills-hub
cd /opt/peckboard-skills-hub
[[ -f peckboard-skills-hub ]] && cp -p peckboard-skills-hub peckboard-skills-hub.prev
install -m 0750 -o root -g peckskills /tmp/peckboard-skills-hub peckboard-skills-hub.new
mv -f peckboard-skills-hub.new peckboard-skills-hub
chown -R root:peckskills /opt/peckboard-skills-hub
chmod -R g-w,o-rwx /opt/peckboard-skills-hub
install -m 0644 -o root -g root /tmp/peckboard-skills-hub.service \
  /etc/systemd/system/peckboard-skills-hub.service
[[ -f /etc/peckboard-skills-hub.env ]] || install -m 0600 -o root -g root /dev/null /etc/peckboard-skills-hub.env
rm -f /tmp/peckboard-skills-hub /tmp/peckboard-skills-hub.service
systemctl daemon-reload
systemctl enable peckboard-skills-hub >/dev/null
if [[ "$RESTART" == 1 ]]; then
  systemctl restart peckboard-skills-hub
  sleep 1
  systemctl --no-pager --lines=5 status peckboard-skills-hub || true
fi
REMOTE
echo "==> done"
