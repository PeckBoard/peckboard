#!/usr/bin/env bash
# Reusable DEV database fixture for migration testing (CLAUDE.md, migrations rule 5).
#
#   scripts/dev-db.sh seed    <old-binary> <dir>
#       Boot <old-binary> on an empty <dir> and seed realistic rows through the
#       HTTP API: 3 folders (targets in <dir>-projects/), 2 paused projects with
#       18 cards across steps, 20 sessions x 12 mock-provider turns (~2.5k
#       events), 3 queued messages, 2 env vars, a disabled repeating task.
#
#   scripts/dev-db.sh migrate <new-binary> <dir> [--expect-tables a,b]
#                                                [--expect-version N] [--settle SECS]
#       Snapshot row counts of every table + agent-start events + max migration,
#       boot <new-binary> on <dir>, wait --settle (default 10) s, stop it,
#       snapshot again and print PASS/FAIL. Run it on a COPY of a seeded fixture.
#
# Example (0.1.55 -> working tree):
#   python3 tmp-scratch/fetch_release.py 0.1.55 tmp-scratch/bin-0.1.55
#   scripts/dev-db.sh seed tmp-scratch/bin-0.1.55/peckboard tmp-scratch/dev-db/0.1.55
#   rm -rf tmp-scratch/dev-db/work && cp -a tmp-scratch/dev-db/0.1.55 tmp-scratch/dev-db/work
#   scripts/dev-db.sh migrate target/verify/peckboard tmp-scratch/dev-db/work \
#       --expect-tables remote_devices --expect-version 1791002399
#
# Servers always run with PECKBOARD_NO_RESUME=1 PECKBOARD_TTS_DOWNLOAD=0 on a
# random high port; only their own PID is ever signalled. Never point <dir> at
# a real install or a copy of one.
set -euo pipefail
exec python3 "$(dirname "$0")/dev-db.py" "$@"
