# Configuration

Config lives at `<dataDir>/config.json`. Precedence (highest first): CLI args > env vars > config.json > defaults.

## CLI Args

| Arg | Default | Description |
|-----|---------|-------------|
| --port | 3333 | HTTP port |
| --https-port | 3334 | HTTPS port |
| --host | 0.0.0.0 | Bind address |
| --data-dir | ~/.peckboard | Data directory (also PECKBOARD_DATA_DIR env) |
| --no-interactive | - | Skip first-run config bootstrap prompt |
| --reset-password | - | Wipe password + tokens, generate new, print, exit |
| --reset-mdns-name | - | Regenerate mDNS hostname, persist, print, exit |
| --plain / --no-color | - | Disable ANSI colors (also NO_COLOR=1) |
| --json | - | JSON log output (also LOG_FORMAT=json) |
| --log-level | info | Log level floor (also LOG_LEVEL env) |

## Config Properties

| Property | Env Var | Default | Description |
|----------|---------|---------|-------------|
| port | PORT | 3333 | HTTP port |
| httpsPort | HTTPS_PORT | 3334 | HTTPS port |
| host | HOST | 0.0.0.0 | Bind address |
| projectsDir | PROJECTS_DIR | ~/Projects | Root directory for projects |
| defaultSessionModel | CLAUDE_MODEL | (unset) | Default model for plain sessions |
| defaultProjectModel | CLAUDE_MODEL | (unset) | Default model for workers |
| defaultSessionEffort | - | (unset) | Default effort for plain sessions |
| defaultProjectEffort | - | (unset) | Default effort for workers |
| claudeBinary | - | claude | Path to Claude CLI binary |
| permissionMode | - | bypassPermissions | CLI permission mode |
| messageTimeoutMs | - | 3600000 (60min) | Idle timeout per turn |
| messageTurnDeadlineMs | - | 7200000 (120min) | Wall-clock cap per turn (0=disabled) |
| idleProcessTimeoutMs | - | 1800000 (30min) | Idle process sweeper threshold (0=disabled) |
| idleSweepIntervalMs | - | 60000 (1min) | Sweeper cadence |
| tls.certPath | - | (self-signed) | Custom TLS cert path |
| tls.keyPath | - | (self-signed) | Custom TLS key path |
| tls.commonName | - | peckboard | Self-signed cert CN |
| tls.validityDays | - | 365 | Cert validity |
| tls.renewWindowDays | - | 30 | Renewal window |
| mdnsName | - | (auto-generated) | Friendly mDNS hostname |
| keepAwake | - | false | Host sleep blocker toggle |

## Model Resolution

Effective model for a worker spawn (highest wins):
1. `card.model`
2. Workflow step's `model`
3. `project.model`
4. `config.defaultProjectModel`
5. CLI `default` alias (fallback)

Plain sessions use `config.defaultSessionModel` with same fallback.

## Effort Resolution

Same four-tier precedence as model:
1. `card.effort`
2. Workflow step's `effort`
3. `project.effort`
4. `config.defaultProjectEffort` (workers) or `config.defaultSessionEffort` (plain)

Allowed values: low, medium, high, xhigh, max. Invalid values treated as "no override."

The CLI persists effort onto the conversation, so `--resume` without `--effort` reuses the prior turn's value. The spawn path always passes `--effort` when any tier resolves to a value.

## First-Run Bootstrap

On first run, if config.json is missing and stdin is a TTY and `--no-interactive` isn't set, the server prompts for a `projectsDir` and writes config.json before loading config.
