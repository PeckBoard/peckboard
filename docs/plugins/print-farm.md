---
title: Print Farm
parent: Plugins
nav_order: 23
---

# Print Farm

Print Farm runs a fleet of 3D printers from PeckBoard and slices models with the [OrcaSlicer](https://github.com/SoftFever/OrcaSlicer) CLI. A sidebar page shows every printer live — state, progress, layer, time left, temperatures, and a webcam snapshot — with pause, resume, and cancel; a Slice tab uploads STL / 3MF / STEP models, picks OrcaSlicer printer, filament, and process profiles, slices, previews the result in 3D (model view plus a layer-by-layer G-code view), and sends it to a printer. Agents get tools to do the same, except that a print never starts without a human saying so.

![The Print Farm page: printer cards with state, progress, temperatures, and webcam snapshots]({{ "/assets/screenshots/plugins/print-farm.png" | relative_url }})

Supported printers: **Klipper** (Moonraker), **OctoPrint**, **Prusa** (PrusaLink), and **Bambu Lab** in LAN mode through the [print-bridge](https://github.com/PeckBoard/print-bridge) sidecar. Bambu speaks MQTT and FTPS over TLS, which a WASM plugin cannot reach, so print-bridge — a small Rust daemon on `127.0.0.1:7781` — translates them to plain HTTP. Download it from its releases page, run it with a token (`print-bridge --token-file …`, a systemd user unit is in the repo), and put the same token in the plugin's `bridge_token` setting. Newer Bambu firmware only allows third-party LAN control with **LAN-only mode and Developer Mode** enabled on the printer.

OrcaSlicer is driven as a separate process — install it on the PeckBoard host (App Manager has an `orca-slicer` entry, and `xvfb` for headless hosts). The plugin reads Orca's bundled vendor profiles and your own presets, resolves their `inherits` chains, and reports estimated time and filament from the sliced output.

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
