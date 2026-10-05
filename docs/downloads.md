---
title: Downloads
nav_order: 3
---

# Downloads

Everything PeckBoard ships is a standalone binary attached to a GitHub
release, each with a `.sha256` checksum file next to it. There are two release
streams:

- **Server and remote agent** — tags like `0.1.65`, on every release. The
  links below always fetch the newest one.
- **Phone and desktop apps** — tags like `mobile-0.1.3`. Direct links below
  point at the current app release; newer ones appear on the
  [releases page](https://github.com/PeckBoard/peckboard/releases) under
  `mobile-*` tags.

## The Server

The PeckBoard server is one binary: web UI, database, and TLS are all inside.
Download, make it executable, run it — see
[Getting Started]({{ "/getting-started.html" | relative_url }}).

| Platform             | Download                                                                                                                         | Checksum                                                                                                        | Notes                                                       |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------- |
| Linux x86_64         | [peckboard-linux-x86_64](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-linux-x86_64)                 | [sha256](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-linux-x86_64.sha256)         | Server/CLI, no desktop deps                                 |
| Linux x86_64 desktop | [peckboard-linux-x86_64-desktop](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-linux-x86_64-desktop) | [sha256](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-linux-x86_64-desktop.sha256) | Adds `--desktop` native window; needs `libwebkit2gtk-4.1-0` |
| macOS Apple Silicon  | [peckboard-macos-arm64](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-macos-arm64)                   | [sha256](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-macos-arm64.sha256)          | Server + desktop window                                     |
| Windows x86_64       | [peckboard-windows-x86_64.exe](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-windows-x86_64.exe)     | [sha256](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-windows-x86_64.exe.sha256)   | Server + desktop window (WebView2)                          |

On Linux and macOS, mark the download executable before the first run:

```bash
chmod +x peckboard-linux-x86_64
./peckboard-linux-x86_64
```

## The Remote Agent

`peckboard-agent` runs on another machine and dials home to your server over
an outbound WebSocket, so sessions can drive that machine. Setup:
[Remote Agent]({{ "/remote-agent.html" | relative_url }}).

| Platform            | Download                                                                                                                                 | Checksum                                                                                                            | Notes                                             |
| ------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------- |
| Linux x86_64        | [peckboard-agent-linux-x86_64](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-agent-linux-x86_64)             | [sha256](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-agent-linux-x86_64.sha256)       | Needs `libxcb1` (preinstalled on desktop distros) |
| Linux arm64         | [peckboard-agent-linux-arm64](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-agent-linux-arm64)               | [sha256](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-agent-linux-arm64.sha256)        | Needs `libxcb1`                                   |
| macOS Apple Silicon | [peckboard-agent-macos-arm64](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-agent-macos-arm64)               | [sha256](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-agent-macos-arm64.sha256)        |                                                   |
| macOS Intel         | [peckboard-agent-macos-x86_64](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-agent-macos-x86_64)             | [sha256](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-agent-macos-x86_64.sha256)       |                                                   |
| Windows x86_64      | [peckboard-agent-windows-x86_64.exe](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-agent-windows-x86_64.exe) | [sha256](https://github.com/PeckBoard/peckboard/releases/latest/download/peckboard-agent-windows-x86_64.exe.sha256) |                                                   |

## The App (Phone and Desktop Remote Client)

The PeckBoard app pairs with your server once and then reaches it from
anywhere — setup in
[Remote Access]({{ "/remote-access.html" | relative_url }}). Current app
release: **mobile-0.1.3**.

| Platform              | Download                                                                                                                                             | Checksum                                                                                                                     | Notes                                                                            |
| --------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| macOS (universal)     | [PeckBoard-0.1.3-macos-universal.dmg](https://github.com/PeckBoard/peckboard/releases/download/mobile-0.1.3/PeckBoard-0.1.3-macos-universal.dmg)     | [sha256](https://github.com/PeckBoard/peckboard/releases/download/mobile-0.1.3/PeckBoard-0.1.3-macos-universal.dmg.sha256)   | Signed and notarized; Apple Silicon + Intel                                      |
| Windows x64 installer | [PeckBoard-0.1.3-windows-x64-setup.exe](https://github.com/PeckBoard/peckboard/releases/download/mobile-0.1.3/PeckBoard-0.1.3-windows-x64-setup.exe) | [sha256](https://github.com/PeckBoard/peckboard/releases/download/mobile-0.1.3/PeckBoard-0.1.3-windows-x64-setup.exe.sha256) | Unsigned: SmartScreen warns — More info → Run anyway. Per-user install, no admin |
| Windows x64 MSI       | [PeckBoard-0.1.3-windows-x64.msi](https://github.com/PeckBoard/peckboard/releases/download/mobile-0.1.3/PeckBoard-0.1.3-windows-x64.msi)             | [sha256](https://github.com/PeckBoard/peckboard/releases/download/mobile-0.1.3/PeckBoard-0.1.3-windows-x64.msi.sha256)       | Same app, MSI packaging                                                          |
| iPhone                | App Store — in review                                                                                                                                | —                                                                                                                            | TestFlight today; the App Store listing is being reviewed                        |
| Android               | Not yet published                                                                                                                                    | —                                                                                                                            | Ships as a signed APK in a future app release                                    |

## Verifying a Download

Every file's `.sha256` holds the expected checksum. Compare it to the file you
downloaded:

```bash
# Linux
sha256sum -c peckboard-linux-x86_64.sha256

# macOS (from the same directory as both files)
shasum -a 256 -c peckboard-macos-arm64.sha256
```

```powershell
# Windows: print both and compare
certutil -hashfile peckboard-windows-x86_64.exe SHA256
type peckboard-windows-x86_64.exe.sha256
```

The checksum files are generated by the same CI run that builds the binaries
and are attached to the release together.
