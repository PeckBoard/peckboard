---
title: Remote Access
nav_order: 12
has_children: true
---

# Remote Access

Remote access lets the PeckBoard app on your phone or another computer reach your box from anywhere, with no port forwarding and no VPN. A _box_ is the machine running your PeckBoard server. The app and the box meet through a relay, then talk directly whenever the network allows; when it doesn't, the relay forwards their traffic, still end-to-end encrypted. This page walks through switching it on, pairing devices, and what to expect once they are connected.

The relay is `relay.peckboard.com` unless you change it. [Official Relay]({{ "/remote-access/official-relay.html" | relative_url }}) explains what it can and cannot see and the one-time registration it asks for; [Running Your Own Relay]({{ "/remote-access/self-hosted-relay.html" | relative_url }}) covers hosting one yourself.

## Turn It On

Remote access is off by default and only admins can change it. Open Settings → Connections → **Remote Access**. The **Relay host** field is already filled in with `relay.peckboard.com`; leave it unless you run your own relay.

Press **On**. The **Turn on remote access?** dialog explains that the box will register with the relay for every paired device, and that anyone holding a device's pairing link can then reach the box from the internet — they still have to sign in. Press **Turn on**.

If the relay wants this box registered and it isn't yet, the relay's registration page opens in a new tab at the same moment. Press **Register** there and wait a few seconds; the [Official Relay]({{ "/remote-access/official-relay.html#register-your-box" | relative_url }}) page covers that step. Back in Settings, the line under the On/Off switch reads **Registered with relay.peckboard.com** once it is done. Registration only matters for the relayed fallback — direct connections work either way — so you can skip it and register later with the **Register** button on that line.

## Pair a Device

## Pair a Device

Each phone or computer gets its own pairing, so you can revoke one without touching the others. Press **+ Pair device**, enter a **Device name** (for example "laptop"), and press **Create pairing**. The next dialog shows a QR code, a **Pairing link** of the form `https://peckboard.com/pair#…`, the **Box fingerprint**, and how long the link is still good for.

**The link is shown only once, works for one device, and expires after an hour.** The first device that opens it enrolls its own key with the box and from then on uses that key; the link itself is spent. Opening the same link on a second device fails with "This pairing link was already used by another device", and the box marks the device row with a warning so you can revoke it if that wasn't you. Hand the link over directly all the same: whoever opens it first gets the pairing.

The link also pins your box's identity key. The app shows the **Box fingerprint** (four groups of four letters) before it pairs; check it matches the one next to the QR code. A different fingerprint means the link doesn't lead to your box — don't continue, and create a new link.

If nobody used a link within the hour, the device row reads **Link expired**; choose **New link** from its menu for a fresh one with the same name. The `peckboard://pair/…` form of the link, for `peckboard-connect` and older apps, is under **For peckboard-connect or an older app** in the same dialog. A box running an older PeckBoard version still hands out `peckboard://` links that stay valid until the device is revoked; the app shows an "Older box" note for those.

The PeckBoard app ships on its own releases, tagged `mobile-X.Y.Z` on the [releases page](https://github.com/PeckBoard/peckboard/releases), separately from the server:

| Platform | How to get it                                                                        |
| -------- | ------------------------------------------------------------------------------------ |
| iPhone   | TestFlight                                                                           |
| Android  | `peckboard-android-X.Y.Z.apk` from the release (sideloaded)                          |
| macOS    | `PeckBoard-X.Y.Z-macos-universal.dmg` — universal, macOS 11 or later, notarized      |
| Windows  | `PeckBoard-X.Y.Z-windows-x64-setup.exe` (or the `.msi`) — per-user install, unsigned |

**On a phone**, open the app and tap **Pair a box** (or **Add box** once one is paired), then **Scan pairing QR code** and point the camera at the code in Settings. Scanning the code with the system camera instead opens the app on a **Pair a box?** screen. Either way, give the box a name if you like and tap **Pair**.

**On a computer**, copy the pairing link, choose **Pair a box** in the app, paste the link under **Paste the pairing link**, and press **Pair**. Opening a `peckboard://pair/…` link while the app is installed also lands on the **Pair a box?** screen, which shows the relay the link points at; nothing is paired until you press **Pair**.

The app connects straight away and opens your box's normal web interface. Sign in once; the app remembers the login for that box. A **Boxes** button in the top-left corner of every box page returns to the list of paired boxes.

<details markdown="1">
<summary>Installer warnings on Android and Windows</summary>

Android asks you to allow your browser to install unknown apps the first time you open the APK. To check the download first, run `sha256sum -c peckboard-android-X.Y.Z.apk.sha256`.

The Windows installer isn't code-signed yet. SmartScreen shows "Windows protected your PC" — choose **More info**, then **Run anyway**. If Windows instead shows a security warning naming an **Unknown Publisher**, press **Run**. The macOS disk image is signed and notarized, so it opens without a warning.

</details>
## Where the Pairing Secret Lives

The app stores the device's key and pairing credential in the platform's secure storage, never in a plain file:

| Platform | Storage                                                                              |
| -------- | ------------------------------------------------------------------------------------ |
| iOS      | Keychain, readable only on this device after first unlock, never synced to iCloud    |
| Android  | Encrypted with a key held in the Android Keystore; app backups and transfers are off |
| macOS    | Keychain                                                                             |
| Windows  | Credential Manager                                                                   |

On the box, each device's secrets are stored encrypted in PeckBoard's database and never shown again after pairing. Once a device has connected with its own key, the link secret it was paired with is retired on the box: a copy of the link, wherever it ended up, no longer gets anyone in.
On the box, each device's secret is stored encrypted in PeckBoard's database and never shown again after pairing.

## Direct and Relayed Connections

The relay only introduces the two ends. It tells each side where to find the other, both send packets at the same moment so their routers let the replies in — _UDP hole punching_ — and from then on traffic flows straight between them. When both networks block that, typically a strict corporate NAT on one side and a mobile carrier on the other, the relay forwards the traffic instead.

Either way the connection is end-to-end encrypted with keys derived from the pairing secret, which the relay never receives, so the relay cannot read it. A relayed connection shows a **Relayed** badge next to the device in Settings and "Relayed via relay.peckboard.com · end-to-end encrypted" on the app's connect screen. The box keeps trying to upgrade a relayed connection to a direct one in the background. [Security and Encryption]({{ "/remote-access/security.html" | relative_url }}) details the encryption and the metadata the relay can still see.

**You never need to forward a port.** The **Direct Connection (Optional)** fields in Settings — **UDP port**, **Range size**, **Public address** — are only for setups that already pin UDP ports on the box; leave them empty otherwise.

## Network Changes and Reconnecting

The app keeps its connection only while it is in the foreground. When you come back to it, it reconnects and returns to the page you were on.

With box 0.1.65 or later and app mobile-0.1.4 or later, a phone that switches between Wi-Fi and cellular on a direct connection drops the old path and reconnects within seconds, without reloading the page. Relayed connections, and older versions, notice a dead path by timeout instead, which takes around 15 seconds. On a computer, waking from sleep triggers the same immediate reconnect.

While it is connecting, the app shows one of these states:

| State               | Meaning                                                                                 |
| ------------------- | --------------------------------------------------------------------------------------- |
| _Finding your box…_ | Meeting the box at the relay                                                            |
| _Reconnecting…_     | The connection dropped; a new one is on its way                                         |
| _Box offline_       | The box isn't at the relay: it is off, remote access is off, or the pairing was revoked |
| _Can't connect_     | Neither a direct path nor the relayed fallback worked                                   |
| _Connection failed_ | Another error, such as the relay being unreachable                                      |

## Rename and Revoke Devices

Every paired device is listed in the Remote Access section with its state — **connected**, **waiting for device**, **offline**, or **error — retrying** — when it last connected, and how its pairing stands:

| Badge                                                          | Meaning                                                                                                                                                    |
| -------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Waiting for first connection · expires in 42 min**           | The link was created and nobody has opened it yet                                                                                                          |
| **Link expired**                                               | The hour passed unused; **New link** in the row menu issues another                                                                                        |
| **Enrolled · iPhone · 14:03 from 203.0.113.7**                 | A device enrolled its key with this link (the name the device reported, when, and from where); "first connection pending" until it has used that key       |
| **Not enrolled (legacy)**                                      | Paired with a link from an older PeckBoard; the link is still its only credential. Updating the app on that device secures the pairing on its next connect |
| ⚠ **Link already used by another device — tried 14:05 from …** | Someone opened this device's link after it was used. If that wasn't you, revoke the device                                                                 |

Its menu offers **Rename**, **New link** (for an unused or expired link), and **Revoke**. Revoking destroys the device's secrets and its key, drops any open connection at once, and signs out logins made through it; pair it again to restore access. Removing a box from the app (**Remove box** on its manage screen) only makes that device forget its credential — revoke it on the box as well.

The box's own identity key lives in the `remote_access_identity` file in its data directory. Every enrolled device pins it, so PeckBoard never replaces a missing file on its own: if it is gone, enrolled devices show **Box identity key missing — re-pair this device** and no new link can be created until the file is restored or those devices are revoked.

Turning remote access **Off** stops the box from meeting any device at the relay; pairings are kept and work again when you turn it back on. Changing the **Relay host** applies to new pairings: links already handed out name the relay they were created for, so pair those devices again after a change.

This is separate from the [Remote Agent]({{ "/remote-agent.html" | relative_url }}), which lets sessions on your box control another machine.
