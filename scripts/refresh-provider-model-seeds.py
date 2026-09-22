#!/usr/bin/env python3
"""Refresh (or check) first-party provider seed catalogs from live CLIs.

Runtime discovery already prefers the CLI/HTTP catalog. This script keeps the
compile-time `seed_models()` fallback in each plugin roughly current for
fresh installs and discovery failures.

Usage (repo root):
  scripts/refresh-provider-model-seeds.sh           # probe + print diff
  scripts/refresh-provider-model-seeds.sh --write   # rewrite seed_models()
  scripts/refresh-provider-model-seeds.sh --check   # CI: non-zero on drift

Missing CLIs are skipped (exit 0) so CI without those binaries stays green.
Ollama is skipped by default (local pulls ≠ a universal catalog); pass
`--with-ollama` to include a running daemon.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

ROOT = Path(__file__).resolve().parents[1]


@dataclass
class Entry:
    id: str
    display_name: str
    capabilities: list[str]
    tier: int


def run_cmd(argv: list[str], timeout: float = 30.0) -> str | None:
    try:
        proc = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=timeout,
            cwd=str(ROOT),
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None
    out = (proc.stdout or "") + (("\n" + proc.stderr) if proc.stderr else "")
    return out if out.strip() else None


def which(name: str) -> str | None:
    return shutil.which(name)


def parse_grok(output: str) -> list[Entry]:
    default_id: str | None = None
    models: list[str] = []
    in_list = False
    for raw in output.splitlines():
        line = raw.strip()
        if not line:
            continue
        low = line.lower()
        if low.startswith("default model:"):
            default_id = line.split(":", 1)[1].strip() or None
            continue
        if low in ("available models:", "available models"):
            in_list = True
            continue
        if not in_list:
            continue
        bullet = None
        for prefix in ("*", "-", "•"):
            if line.startswith(prefix):
                bullet = line[len(prefix) :].strip()
                break
        if bullet is None:
            break
        mid = bullet.split()[0].rstrip("(),")
        if mid and mid not in models:
            models.append(mid)
    if default_id:
        if default_id in models:
            models.remove(default_id)
        models.insert(0, default_id)
    out: list[Entry] = []
    for i, mid in enumerate(models):
        name = f"Grok {mid[5:]}" if mid.startswith("grok-") and mid[5:] else mid
        out.append(Entry(mid, name, ["code", "reasoning"], i))
    return out


def parse_cursor(output: str) -> list[Entry]:
    ids: list[str] = []
    # JSON first
    trimmed = output.strip()
    try:
        value = json.loads(trimmed)
        ids = _extract_ids(value)
    except json.JSONDecodeError:
        ids = []
    if not ids:
        for line in trimmed.splitlines():
            line = line.strip()
            if " - " not in line:
                continue
            left, _right = line.split(" - ", 1)
            mid = left.strip()
            if not mid or any(c.isspace() for c in mid):
                continue
            if mid not in ids:
                ids.append(mid)
    out: list[Entry] = []
    for mid in ids:
        caps = ["code", "reasoning"] if "thinking" in mid.lower() else ["code"]
        out.append(Entry(mid, f"{mid} (Cursor)", caps, 0))
    return out


def parse_codex(output: str) -> list[Entry]:
    trimmed = output.strip()
    entries: list[Entry] = []
    try:
        value = json.loads(trimmed)
    except json.JSONDecodeError:
        value = None
        # NDJSON / slug lines
        for line in trimmed.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                if re.fullmatch(r"[A-Za-z0-9._:-]+", line):
                    mid = line.removeprefix("codex:")
                    entries.append(
                        Entry(mid, mid, ["code", "reasoning"], len(entries))
                    )
                continue
            entries.extend(_codex_from_value(obj))
        return _dedup_entries(entries)

    return _dedup_entries(_codex_from_value(value))


def _codex_from_value(value: object) -> list[Entry]:
    out: list[Entry] = []
    if isinstance(value, list):
        items = value
    elif isinstance(value, dict):
        items = value.get("models") or value.get("data") or []
    else:
        return out
    if not isinstance(items, list):
        return out
    for i, item in enumerate(items):
        if isinstance(item, str):
            mid = item.removeprefix("codex:")
            out.append(Entry(mid, mid, ["code", "reasoning"], i))
            continue
        if not isinstance(item, dict):
            continue
        mid = (
            item.get("slug")
            or item.get("id")
            or item.get("name")
            or item.get("model")
            or ""
        )
        mid = str(mid).removeprefix("codex:").strip()
        if not mid:
            continue
        display = str(item.get("display_name") or item.get("displayName") or mid)
        out.append(Entry(mid, display, ["code", "reasoning"], i))
    return out


def parse_kimi(output: str) -> list[Entry]:
    """Best-effort: keep seed as `default` and append provider aliases."""
    trimmed = output.strip()
    try:
        value = json.loads(trimmed)
    except json.JSONDecodeError:
        return []
    items = value if isinstance(value, list) else value.get("providers") or value.get("models") or []
    if not isinstance(items, list):
        return []
    out = [Entry("default", "Default (Kimi config)", ["code"], 0)]
    for item in items:
        if isinstance(item, str):
            mid = item
            name = None
            caps = ["code"]
        elif isinstance(item, dict):
            mid = str(item.get("id") or item.get("alias") or item.get("name") or "").strip()
            name = item.get("display_name") or item.get("displayName") or item.get("name")
            tags = item.get("capabilities") or item.get("tags") or ["code"]
            caps = [str(t) for t in tags] if isinstance(tags, list) else ["code"]
        else:
            continue
        if not mid or mid == "default":
            continue
        display = f"{name} (Kimi)" if name else f"{mid} (Kimi)"
        out.append(Entry(mid, display, caps or ["code"], 0))
    return out


def parse_ollama_tags(output: str) -> list[Entry]:
    # `ollama list` table: NAME ID SIZE MODIFIED
    entries: list[Entry] = []
    for i, line in enumerate(output.splitlines()):
        line = line.strip()
        if not line or line.lower().startswith("name"):
            continue
        name = line.split()[0]
        if not name or name in {e.id for e in entries}:
            continue
        entries.append(Entry(name, f"{name} (Ollama)", ["code"], 0))
    return entries


def _extract_ids(value: object) -> list[str]:
    if isinstance(value, list):
        items = value
    elif isinstance(value, dict):
        items = value.get("models") or value.get("data") or []
    else:
        return []
    ids: list[str] = []
    for item in items:
        if isinstance(item, str):
            mid = item.strip()
        elif isinstance(item, dict):
            mid = str(item.get("id") or item.get("name") or item.get("model") or "").strip()
        else:
            continue
        if mid and mid not in ids:
            ids.append(mid)
    return ids


def _dedup_entries(entries: list[Entry]) -> list[Entry]:
    seen: set[str] = set()
    out: list[Entry] = []
    for e in entries:
        if e.id in seen:
            continue
        seen.add(e.id)
        out.append(e)
    return out


def _seed_array_span(text: str) -> tuple[int, int] | None:
    """Byte span of the top-level `[...]` inside `seed_models()`."""
    m = re.search(r"pub fn seed_models\(\) -> serde_json::Value \{", text)
    if not m:
        return None
    json_m = re.search(r"serde_json::json!\(", text[m.start():])
    if not json_m:
        return None
    i = m.start() + json_m.end()
    while i < len(text) and text[i].isspace():
        i += 1
    if i >= len(text) or text[i] != "[":
        return None
    depth = 0
    j = i
    in_str = False
    escape = False
    while j < len(text):
        ch = text[j]
        if in_str:
            if escape:
                escape = False
            elif ch == "\\":
                escape = True
            elif ch == '"':
                in_str = False
        else:
            if ch == '"':
                in_str = True
            elif ch == "[":
                depth += 1
            elif ch == "]":
                depth -= 1
                if depth == 0:
                    return (i, j + 1)
        j += 1
    return None


def read_seed_ids(models_rs: Path) -> list[str]:
    text = models_rs.read_text()
    span = _seed_array_span(text)
    body = text[span[0]:span[1]] if span else text
    return re.findall(r'"id":\s*"([^"]+)"', body)


def render_seed_array(entries: list[Entry]) -> str:
    lines = ["["]
    for e in entries:
        caps = ", ".join(f'"{c}"' for c in e.capabilities)
        lines.append("        {")
        lines.append(f'            "id": {json.dumps(e.id)},')
        lines.append(f'            "display_name": {json.dumps(e.display_name)},')
        lines.append(f'            "capabilities": [{caps}],')
        lines.append(f'            "tier": {e.tier}')
        lines.append("        },")
    if len(lines) > 1:
        lines[-1] = "        }"
    lines.append("    ]")
    return "\n".join(lines)


def write_seed(models_rs: Path, entries: list[Entry]) -> bool:
    text = models_rs.read_text()
    span = _seed_array_span(text)
    if not span:
        print(f"  ! could not locate seed_models() in {models_rs}", file=sys.stderr)
        return False
    a0, a1 = span
    new_body = render_seed_array(entries)
    new_text = text[:a0] + new_body + text[a1:]
    if new_text == text:
        return False
    models_rs.write_text(new_text)
    return True


@dataclass
class ProviderJob:
    id: str
    models_rs: Path
    binaries: list[str]
    argv: list[str]
    parse: Callable[[str], list[Entry]]
    rewrite: bool = True
    note: str | None = None


def jobs(with_ollama: bool) -> list[ProviderJob]:
    out = [
        ProviderJob(
            "grok",
            ROOT / "peck-plugins/grok/src/models.rs",
            ["grok"],
            ["models"],
            parse_grok,
        ),
        ProviderJob(
            "cursor",
            ROOT / "peck-plugins/cursor/src/models.rs",
            ["cursor-agent", "agent"],
            ["models"],
            parse_cursor,
            # Live list is account-scoped and often 100+ ids. Seed stays a
            # curated popular subset; last-good cache covers offline full lists.
            rewrite=False,
            note="seed is a curated popular subset; discovery + last-good cover the full list",
        ),
        ProviderJob(
            "codex",
            ROOT / "peck-plugins/codex/src/models.rs",
            ["codex"],
            ["debug", "models", "--bundled"],
            parse_codex,
        ),
        ProviderJob(
            "kimi",
            ROOT / "peck-plugins/kimi/src/models.rs",
            ["kimi"],
            ["provider", "list", "--json"],
            parse_kimi,
            # Seed intentionally keeps only `default`; discovery appends aliases.
            # Still useful for --check of the default entry presence.
            rewrite=False,
            note="seed stays `default`; discovery appends aliases at runtime",
        ),
        ProviderJob(
            "claude",
            ROOT / "peck-plugins/claude/src/models.rs",
            ["claude"],
            [],  # special-cased below
            lambda _o: [],
            rewrite=False,
            note=(
                "Claude Code has no `models` subcommand; runtime discovery uses "
                "the stream-json initialize handshake. Seed holds pinned snapshot "
                "ids merged via merge_always_offered — update models.rs by hand "
                "when Anthropic ships a new pin-worthy snapshot."
            ),
        ),
    ]
    if with_ollama:
        out.append(
            ProviderJob(
                "ollama",
                ROOT / "peck-plugins/ollama/src/models.rs",
                ["ollama"],
                ["list"],
                parse_ollama_tags,
                rewrite=True,
            )
        )
    return out


def probe(job: ProviderJob) -> tuple[str | None, list[Entry] | None, str | None]:
    """Returns (binary, entries, skip_reason)."""
    if job.id == "claude":
        return None, None, job.note
    binary = next((b for b in job.binaries if which(b)), None)
    if not binary:
        return None, None, f"no binary on PATH ({', '.join(job.binaries)})"
    out = run_cmd([binary, *job.argv])
    if out is None:
        return binary, None, "command failed or timed out"
    entries = job.parse(out)
    if not entries:
        return binary, None, "parsed empty catalog"
    return binary, entries, None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--write",
        action="store_true",
        help="Rewrite seed_models() for providers whose catalogs can be regenerated",
    )
    ap.add_argument(
        "--check",
        action="store_true",
        help="Exit non-zero when a probed catalog drifts from the seed id set",
    )
    ap.add_argument(
        "--with-ollama",
        action="store_true",
        help="Also probe `ollama list` (local inventory; usually not committed)",
    )
    args = ap.parse_args()

    drifted = False
    any_probed = False

    for job in jobs(args.with_ollama):
        print(f"\n== {job.id} ==")
        seed_ids = read_seed_ids(job.models_rs) if job.models_rs.exists() else []
        binary, entries, skip = probe(job)
        if skip:
            print(f"  skip: {skip}")
            if seed_ids:
                print(f"  seed ({len(seed_ids)}): {', '.join(seed_ids)}")
            continue
        assert entries is not None and binary is not None
        any_probed = True
        live_ids = [e.id for e in entries]
        print(f"  via: {binary} {' '.join(job.argv)}".rstrip())
        print(f"  live ({len(live_ids)}): {', '.join(live_ids)}")
        print(f"  seed ({len(seed_ids)}): {', '.join(seed_ids)}")
        missing = [i for i in live_ids if i not in seed_ids]
        extra = [i for i in seed_ids if i not in live_ids]
        if missing or extra:
            # Curated seeds (rewrite=False) are expected to diverge from the
            # full live list; report the diff but don't fail --check on them.
            # Auth-scoped CLIs can also return a truncated live list — seed
            # supersets of that are fine; only "live ahead of seed" fails CI.
            if job.rewrite and missing:
                drifted = True
            if missing:
                shown = ', '.join(missing[:20]) + ('…' if len(missing) > 20 else '')
                print(f"  + missing from seed ({len(missing)}): {shown}")
            if extra:
                print(f"  - extra in seed: {', '.join(extra)}")
            if not job.rewrite:
                print("  note: curated seed — drift is informational only")
            elif extra and not missing:
                print("  note: seed is a superset of live (likely auth-scoped probe)")
        else:
            print("  ok: seed id set matches live")

        if args.write and job.rewrite:
            # Auth-scoped CLIs can return a tiny subset when unauthenticated;
            # refuse to shrink a larger seed from that.
            if seed_ids and len(entries) < len(seed_ids):
                print(
                    f"  refuse --write: live catalog ({len(entries)}) looks truncated "
                    f"vs seed ({len(seed_ids)}); authenticate and retry"
                )
            elif write_seed(job.models_rs, entries):
                print(f"  wrote {job.models_rs.relative_to(ROOT)}")
            else:
                print("  seed already up to date")
        elif args.write and not job.rewrite and job.note:
            print(f"  note: {job.note}")

    print()
    if args.check:
        if not any_probed:
            print("check: no provider CLIs available — skipping (exit 0)")
            return 0
        if drifted:
            print("check: seed drift detected", file=sys.stderr)
            return 1
        print("check: no drift")
        return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
