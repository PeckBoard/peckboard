---
title: Print Farm
parent: Plugins
nav_order: 23
---

# Print Farm

Print Farm runs a fleet of 3D printers from PeckBoard and slices models with the [OrcaSlicer](https://github.com/SoftFever/OrcaSlicer) CLI. A sidebar page shows every printer live — state, progress, layer, time left, temperatures, and a webcam snapshot — with pause, resume, and cancel; a Slice tab uploads STL / 3MF / STEP models, picks OrcaSlicer printer, filament, and process profiles, slices, previews the result in 3D (model view plus a layer-by-layer G-code view), and sends it to a printer. Agents get tools to do the same, except that a print never starts without a human saying so.

![The Print Farm page: printer cards with state, progress, temperatures, and webcam snapshots]({{ "/assets/screenshots/plugins/print-farm.png" | relative_url }})

Supported printers: **Klipper** (Moonraker), **OctoPrint**, **Prusa** (PrusaLink), and **Bambu Lab** in LAN mode through the [print-bridge](https://github.com/PeckBoard/print-bridge) helper. Bambu speaks MQTT and FTPS over TLS, which a WASM plugin cannot reach, so print-bridge — a small native daemon on `127.0.0.1:7781`, not a registry plugin — translates them to plain HTTP. Newer Bambu firmware only allows third-party LAN control with **LAN-only mode and Developer Mode** enabled on the printer.

Setup is automatic. With nothing to fill in, Print Farm:

- installs OrcaSlicer itself if none is found: it extracts the AppImage into `~/.local/share/orca-slicer` without root, and fetches libraries the host lacks (for example `libGLU`). On a host with no display it wraps the slicer in `xvfb-run` when that is installed.
- installs or upgrades print-bridge as a systemd user service once a Bambu printer exists, and reads its token from `~/.config/print-bridge/token`.
- picks the printer's own OrcaSlicer machine preset, and that preset's default process and filament.
- corrects a mistyped Bambu serial from the printer's certificate.

The plugin reads Orca's bundled vendor profiles and your own presets and resolves their `inherits` chains within the preset's own vendor. It slices a Bambu Studio project 3MF by keeping its geometry and applying the chosen profiles, and reports estimated time and filament from the sliced output.

The Bambu camera stays connected while the page is open and shows the latest frame. On weak printer Wi-Fi a frame can take a minute to arrive. The connection drops after a minute without viewers, because the printer allows only one camera client.

| Tool             | What it does                                                                                     |
| ---------------- | ------------------------------------------------------------------------------------------------ |
| `printer_list`   | Lists configured printers with credentials redacted                                              |
| `printer_status` | Live status of one printer or all: state, progress, layer, times, temperatures                   |
| `profile_list`   | OrcaSlicer printer / filament / process profiles, filterable by vendor and name                  |
| `slice_model`    | Slices a project file or uploaded file id into G-code (or `.gcode.3mf` for Bambu); returns a job |
| `slice_status`   | Polls a slice job: log, output file, estimated time and filament                                 |
| `print_upload`   | Uploads a sliced file to a printer without starting it                                           |
| `print_start`    | Queues a print and asks you in the agent's chat; it starts only on your "Start print"            |
| `print_control`  | Pauses or cancels a print (resume is page-only — it moves hardware)                              |
| `print_snapshot` | Saves a webcam snapshot                                                                          |

If the agent cannot ask you (no live chat), the print waits on the page with a **Start print** button.

<details markdown="1">
<summary>Hooks, permissions, settings, and storage</summary>

Hooks: `mcp.tool.invoke`, `http.request.before`, `http.request.authed`. Permissions: `http_request`, `process_exec_any`, `data_store`, `ask_user`, `provide_mcp_tools`, `user_authority`, `contribute_sidebar`.

`process_exec_any` runs `curl` (binary uploads, multipart forms, self-signed printer TLS), `orca-slicer`, and file housekeeping. While a `curl` request runs, the printer's API key or password is visible on its command line to other processes of the same user.

Settings: `bridge_url` (default `http://127.0.0.1:7781`), `bridge_token`, `orca_path` (default `orca-slicer`), `orca_profiles_dir` (auto-detected), `orca_headless` (`auto` wraps the slicer in `xvfb-run -a` when there is no display), and `storage_dir` (default `~/.local/share/peckboard-print-farm`). Models, G-code, and snapshots are stored on disk there; printer records and job metadata live in the plugin data store.

</details>
