// Declarative app catalog. Every entry is plain data — adding an app is a
// pure data change, no code change. Every command here is a *static* shell
// script authored by us; nothing here is ever built from user input (see
// tools.ts, which validates any user-supplied app/target id against this
// table before touching a shell at all).
//
// `install`/`remove` carry one recipe per package manager plus an optional
// `vendor` fallback (an upstream curl|sh-style installer) for apps that
// aren't in distro repos. Every recipe that needs root prefixes with
// `sudo -A` — see src/service/askpass.rs in the core repo for the askpass
// bridge convention; a plugin-run `sudo -A` has no askpass wired today, so
// it fails cleanly with sudo's own stderr rather than hanging, and that
// message ends up in the job log tail (see jobs.ts / tools.ts appStatus).

import { PackageManager } from "./distro";

export interface CatalogApp {
  id: string;
  name: string;
  description: string;
  /** Shell script; exit 0 means the app is installed. */
  detect: string;
  /** Shell script; best-effort, prints a version string on success. */
  version: string;
  install: Partial<Record<PackageManager, string>> & { vendor?: string };
  remove: Partial<Record<PackageManager, string>> & { vendor?: string };
  /** The distro package names each PM recipe installs (space-separated,
   * mirroring the recipe's arguments) — lets provenance attribute the app's
   * own package in a snapshot delta (see provenance.ts). Vendor-only apps
   * have none: their binaries never enter the package database. */
  packages?: Partial<Record<PackageManager, string>>;
}

function aptInstall(pkg: string): string {
  return `sudo -A apt-get update && sudo -A apt-get install -y ${pkg}`;
}
function aptRemove(pkg: string): string {
  return `sudo -A apt-get remove -y ${pkg}`;
}
function dnfInstall(pkg: string): string {
  return `sudo -A dnf install -y ${pkg}`;
}
function dnfRemove(pkg: string): string {
  return `sudo -A dnf remove -y ${pkg}`;
}
function pacmanInstall(pkg: string): string {
  return `sudo -A pacman -Sy --noconfirm ${pkg}`;
}
function pacmanRemove(pkg: string): string {
  return `sudo -A pacman -R --noconfirm ${pkg}`;
}
function zypperInstall(pkg: string): string {
  return `sudo -A zypper --non-interactive install ${pkg}`;
}
function zypperRemove(pkg: string): string {
  return `sudo -A zypper --non-interactive remove ${pkg}`;
}

export const APPS: CatalogApp[] = [
  {
    id: "git",
    name: "Git",
    description: "Distributed version control system.",
    detect: "command -v git",
    version: "git --version",
    packages: { apt: "git", dnf: "git", pacman: "git", zypper: "git" },
    install: {
      apt: aptInstall("git"),
      dnf: dnfInstall("git"),
      pacman: pacmanInstall("git"),
      zypper: zypperInstall("git"),
    },
    remove: {
      apt: aptRemove("git"),
      dnf: dnfRemove("git"),
      pacman: pacmanRemove("git"),
      zypper: zypperRemove("git"),
    },
  },
  {
    id: "claude",
    name: "Claude Code",
    description:
      "Anthropic's Claude Code CLI. Not packaged by any distro — installed via npm, so Node.js must already be present on the target.",
    detect: "command -v claude",
    version: "claude --version",
    install: { vendor: "npm install -g @anthropic-ai/claude-code" },
    remove: { vendor: "npm uninstall -g @anthropic-ai/claude-code" },
  },
  {
    id: "cursor-agent",
    name: "Cursor Agent",
    description:
      "Cursor's CLI coding agent. Installed via Cursor's official installer script.",
    detect: "command -v cursor-agent",
    version: "cursor-agent --version",
    install: { vendor: "curl https://cursor.com/install -fsS | bash" },
    remove: {
      vendor:
        'rm -f "$HOME/.local/bin/cursor-agent" && rm -rf "$HOME/.local/share/cursor-agent"',
    },
  },
  {
    id: "ollama",
    name: "Ollama",
    description:
      "Local LLM runner. Installed via Ollama's official installer script.",
    detect: "command -v ollama",
    version: "ollama --version",
    install: { vendor: "curl -fsSL https://ollama.com/install.sh | sh" },
    remove: {
      vendor:
        "sudo -A systemctl stop ollama; sudo -A systemctl disable ollama; " +
        "sudo -A rm -f /usr/local/bin/ollama /usr/bin/ollama; " +
        "sudo -A rm -rf /usr/share/ollama /etc/systemd/system/ollama.service; " +
        "sudo -A userdel ollama; true",
    },
  },
  {
    id: "node",
    name: "Node.js",
    description:
      "JavaScript runtime (distro-packaged version, may lag upstream releases).",
    detect: "command -v node",
    version: "node --version",
    packages: {
      apt: "nodejs npm",
      dnf: "nodejs",
      pacman: "nodejs npm",
      zypper: "nodejs npm",
    },
    install: {
      apt: aptInstall("nodejs npm"),
      dnf: dnfInstall("nodejs"),
      pacman: pacmanInstall("nodejs npm"),
      zypper: zypperInstall("nodejs npm"),
    },
    remove: {
      apt: aptRemove("nodejs npm"),
      dnf: dnfRemove("nodejs"),
      pacman: pacmanRemove("nodejs npm"),
      zypper: zypperRemove("nodejs npm"),
    },
  },
  {
    id: "docker",
    name: "Docker",
    description:
      "Container runtime (distro-packaged Docker Engine, not the upstream Docker CE repo).",
    detect: "command -v docker",
    version: "docker --version",
    packages: {
      apt: "docker.io",
      dnf: "docker",
      pacman: "docker",
      zypper: "docker",
    },
    install: {
      apt: aptInstall("docker.io"),
      dnf: dnfInstall("docker"),
      pacman: pacmanInstall("docker"),
      zypper: zypperInstall("docker"),
    },
    remove: {
      apt: aptRemove("docker.io"),
      dnf: dnfRemove("docker"),
      pacman: pacmanRemove("docker"),
      zypper: zypperRemove("docker"),
    },
  },
  {
    id: "ripgrep",
    name: "ripgrep",
    description: "Fast recursive grep replacement (binary name: rg).",
    detect: "command -v rg",
    version: "rg --version",
    packages: {
      apt: "ripgrep",
      dnf: "ripgrep",
      pacman: "ripgrep",
      zypper: "ripgrep",
    },
    install: {
      apt: aptInstall("ripgrep"),
      dnf: dnfInstall("ripgrep"),
      pacman: pacmanInstall("ripgrep"),
      zypper: zypperInstall("ripgrep"),
    },
    remove: {
      apt: aptRemove("ripgrep"),
      dnf: dnfRemove("ripgrep"),
      pacman: pacmanRemove("ripgrep"),
      zypper: zypperRemove("ripgrep"),
    },
  },
];

export function findApp(id: string): CatalogApp | undefined {
  return APPS.find((a) => a.id === id);
}

/** The install recipe for a resolved package manager, falling back to the
 * app's vendor script when there's no PM-specific recipe (or no PM at all). */
export function installRecipeFor(
  app: CatalogApp,
  pm: PackageManager | null,
): string | null {
  if (pm && app.install[pm]) return app.install[pm] as string;
  return app.install.vendor ?? null;
}

export function removeRecipeFor(
  app: CatalogApp,
  pm: PackageManager | null,
): string | null {
  if (pm && app.remove[pm]) return app.remove[pm] as string;
  return app.remove.vendor ?? null;
}

/** The distro package names `installRecipeFor(app, pm)`'s PM recipe would
 * install; empty when the app resolves to its vendor script (or has no
 * recipe at all). Used to attribute the app's own package in an install's
 * snapshot delta — see provenance.ts. */
export function packagesFor(
  app: CatalogApp,
  pm: PackageManager | null,
): string[] {
  if (!pm || !app.install[pm]) return [];
  return (app.packages?.[pm] ?? "").split(/\s+/).filter(Boolean);
}
