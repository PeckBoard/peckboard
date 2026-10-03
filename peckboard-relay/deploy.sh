#!/usr/bin/env bash
# Build a static musl peckboard-relay and install it on HOST over SSH.
#
#   peckboard-relay/deploy.sh user@host [--no-restart | --rollback]
#
# Requires: the x86_64-unknown-linux-musl Rust target, plus either
# `cargo zigbuild`, the repo's musl cross toolchain under
# tmp-musl-tools/x86_64-linux-musl-cross, or CC_x86_64_unknown_linux_musl.
# The remote user needs sudo. Extra ssh/scp options go in $SSH_OPTS (e.g. a
# pinned UserKnownHostsFile). Firewall is managed separately.
#
# Installs /opt/peckboard-relay/peckboard-relay (previous binary kept as
# peckboard-relay.prev) and the systemd unit. Runtime flags such as
# --acme-staging go in /etc/peckboard-relay.env as PECKRELAY_ARGS=...
set -euo pipefail

HOST="${1:?usage: deploy.sh user@host [--no-restart | --rollback]}"
MODE="${2:-install}"
# shellcheck disable=SC2206
SSH_OPTS=(${SSH_OPTS:-})

cd "$(dirname "$0")"

if [[ "$MODE" == "--rollback" ]]; then
  ssh "${SSH_OPTS[@]}" "$HOST" "sudo bash -s" <<'REMOTE'
set -euo pipefail
cd /opt/peckboard-relay
[[ -f peckboard-relay.prev ]] || { echo "no previous binary" >&2; exit 1; }
mv -f peckboard-relay.prev peckboard-relay.tmp
[[ -f peckboard-relay ]] && mv -f peckboard-relay peckboard-relay.prev
mv -f peckboard-relay.tmp peckboard-relay
systemctl restart peckboard-relay
sleep 1
systemctl --no-pager --lines=5 status peckboard-relay || true
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
BIN="target/$TARGET/release/peckboard-relay"

if command -v file >/dev/null 2>&1; then
  case "$(file -b "$BIN")" in
    *"statically linked"* | *"static-pie linked"*) ;;
    *) echo "refusing to deploy: $BIN is not statically linked" >&2; exit 1 ;;
  esac
fi

echo "==> uploading to $HOST"
scp -q "${SSH_OPTS[@]}" "$BIN" deploy/peckboard-relay.service "$HOST:/tmp/"

echo "==> installing"
ssh "${SSH_OPTS[@]}" "$HOST" "sudo RESTART=$RESTART bash -s" <<'REMOTE'
set -euo pipefail
if ! id peckrelay >/dev/null 2>&1; then
  useradd --system --no-create-home --home-dir /var/lib/peckrelay \
    --shell /usr/sbin/nologin peckrelay
fi
install -d -m 0750 -o peckrelay -g peckrelay /opt/peckboard-relay
cd /opt/peckboard-relay
[[ -f peckboard-relay ]] && cp -p peckboard-relay peckboard-relay.prev
install -m 0750 -o root -g peckrelay /tmp/peckboard-relay peckboard-relay.new
mv -f peckboard-relay.new peckboard-relay
install -m 0644 -o root -g root /tmp/peckboard-relay.service \
  /etc/systemd/system/peckboard-relay.service
rm -f /tmp/peckboard-relay /tmp/peckboard-relay.service
systemctl daemon-reload
systemctl enable peckboard-relay >/dev/null
if [[ "$RESTART" == 1 ]]; then
  systemctl restart peckboard-relay
  sleep 1
  systemctl --no-pager --lines=5 status peckboard-relay || true
fi
REMOTE
echo "==> done"
