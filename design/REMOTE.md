# Remote play: quiverd on a server, Quiver anywhere (2026-11-21)

The person installs quiverd on a headless machine with a GPU. Quiver on a laptop, a desktop or a
TV box then browses that machine's library, installs games on it, and plays them by streaming.
Input goes up and audio/video comes back, at latency that competes with Steam Remote Play,
Moonlight/Sunshine and Parsec. It should beat them where Quiver can see more than they do: Sight,
profiles, the virtual display sized to the client, HDR, and SBS 3D.

## 1. Goals and bars

| | Bar |
|---|---|
| Latency, LAN, 1440p120 | capture-to-photon median ≤ 12 ms added over local play; p99 ≤ 20 ms |
| Stages (medians) | capture → encode start ≤ 1 ms, encode ≤ 4 ms, network ≈ RTT/2, decode ≤ 2.5 ms, present ≤ 1 refresh (≤ 0 with VRR/tearing) |
| Loss | 2% random loss and no visible artefact (FEC); 10% loss stays playable (reference invalidation, no keyframe storms) |
| Bandwidth | adapts within 1 s to a drop, never sends more than the link (delay-based control, no bufferbloat) |
| Video | H.264 / HEVC / AV1 on NVENC (VAAPI and Vulkan Video next), 4:2:0 and 4:4:4, 8- and 10-bit, HDR10 passthrough with metadata, any size the client asks for, ≥ 240 fps |
| Audio | Opus at 48 kHz, 5 ms frames, stereo/5.1/7.1, jitter buffer with PLC, A/V kept in sync to ±20 ms; microphone up (later) |
| Input | keyboard by evdev position, so the client's compositor remaps carry over (Caps Lock ↔ Esc, project memory); raw relative mouse with pointer lock; high-resolution wheel; up to 4 pads with rumble, gyro and Guide; touch |
| Security | nothing listens unauthenticated; pairing with a short code (PAKE); mutual TLS with pinned keys; media AEAD-encrypted; a store secret never leaves the machine that holds it |

## 2. The pieces

```
 client machine                                     server (headless)
 ┌────────────────────────┐   control plane (TLS)  ┌──────────────────────────────┐
 │ quiver-app ── machines ├────────────────────────┤ quiverd  (quiver-remote)     │
 │   │  (quiver-remote)   │   remote API, pairing, │   │ spawns per stream         │
 │   ▼                    │   StartStream → ticket │   ▼                           │
 │ quiver-stream (client) │◄══ media UDP (AEAD) ══►│ quiver-stream-host            │
 │  SDL3 window + input   │    video/audio down    │  gamescope --backend headless │
 │  ffmpeg hw decode      │    input up            │  capture → NVENC              │
 │  Vulkan present        │   (quiver-stream-net)  │  PipeWire sink → Opus         │
 │  PipeWire/SDL audio    │                        │  uinput kbd/mouse/pads, XTest │
 └────────────────────────┘                        └──────────────────────────────┘
```

| Crate | Kind | Owns |
|---|---|---|
| `quiver-stream-proto` | lib | **The contract.** Control messages, `EncodedFrame`, `AudioPacket`, `InputEvent`, caps and config. The main session owns it; change it on purpose and bump `PROTOCOL_VERSION` when a change breaks the other side. |
| `quiver-stream-net` | lib | The media transport: UDP, packetising, FEC, AEAD, pacing, the bitrate controller, input redundancy, the clock sync, stats, and a loss/latency/jitter simulator for tests. |
| `quiver-stream-host` | bin + lib | Server side of a stream: the virtual display (headless gamescope at the client's size and rate), capture, encode, audio capture and encode, input injection, rumble back. |
| `quiver-stream` | bin + lib | The client: window, decode, present, frame pacing, input capture, audio playback, stats overlay, the stream menu. |
| `quiver-remote` | lib | The control plane: machine identity, pairing, mutual TLS, discovery, the remote API tunnel, stream tickets. It's used by quiverd, quiver-api and quiver-app. |

### 2.1 Why these choices

- **The media goes over its own UDP, not over QUIC streams.** Real-time video wants loss handled
  by FEC and reference invalidation, not retransmission, and it wants its own pacing and
  congestion control. QUIC's controller would fight ours. Sunshine, Parsec and Steam all do the
  same. The keys come from the control plane, so the UDP side has no handshake of its own. A
  quinn-datagram transport stays possible behind the same API if measurements say so.
- **The host is a process per stream,** spawned by quiverd. An encoder crash or a GPU hang then
  doesn't take quiverd down, and quiverd can still restart onto a new build while a stream runs.
  quiverd passes the keys and settings on an fd; the client connects straight to the host's port.
- **Headless gamescope is the virtual display.** The lab already proves it on NVIDIA (LAB.md §1,
  §12, §13), it does HDR headless, and it runs at any size and rate. Capture is gamescope's
  PipeWire stream (dmabuf), or the Sight layer's swapchain copy where that is cheaper; the host
  measures both. The display matches the client exactly (an XReal in SBS gets 3840×1080), so
  nothing is scaled.
- **The client is SDL3 + Vulkan + ffmpeg, not a GPUI window.** It needs keyboard grab
  (`zwp_keyboard_shortcuts_inhibit`, so Super and Alt-Tab reach the game when captured),
  pointer lock with relative motion, pads with rumble and gyro, and present modes GPUI doesn't
  expose. quiver-app launches it, like Moonlight's UI launches its stream, and the stream menu
  is drawn in-window.

## 3. Ports, identity, pairing

- **Identity.** Each quiverd has a machine key: Ed25519, with a self-signed X.509 certificate for
  rustls (rcgen). It lives at `<data>/remote/machine.key`, 0600. Each machine also has a name
  (the hostname by default) and a stable id (the key's hash). Paired peers are listed in
  `<data>/remote/peers.json` (id, name, certificate pin, role, last seen).
- **Off by default.** `quiver remote enable [--listen ADDR]` turns the listener on. The default
  is TCP 47990 for the control plane, with media on a UDP port per stream from 47998 up. It binds
  on all addresses, because a Tailscale or WireGuard address is the expected way in from outside;
  the docs say not to port-forward. `quiver remote status` shows the state, and
  `quiver remote disable` turns it off.
- **Pairing works when the server has no screen.** On the server, `quiver remote pair` prints a
  6-digit code and the machine's name, and waits 2 minutes. The person types the code into Quiver on
  the client (Settings → Machines → Add). The two sides run SPAKE2 over the code on a TLS
  connection whose certificates aren't pinned yet, then confirm each other's certificate hashes
  inside the PAKE's key, and each pins the other. A wrong code fails. Three wrong codes end the
  pairing window. A client already paired can pair others from its own UI.
- **Discovery.** mDNS `_quiver._tcp` on the LAN advertises the name and id, never the library.
  An address can also be added by hand (a Tailscale name). Unpaired machines show as
  "Pair…".
- **Authorisation.** A paired peer is the person, so their requests carry
  `source: remote:<peer name>` and are allowed as the app's would be. A few requests are
  local-only, such as anything that names a local path to read or open a file picker on. The
  daemon lists them, and refuses them over the tunnel with code `local_only`. Revoking a peer
  (`quiver remote revoke`, or from the UI) drops its connections at once.

## 4. The remote API

The quiverd protocol (newline-delimited JSON, `quiver-api`) goes unchanged over the TLS
connection. `quiver_api::Client` gains a transport: Unix socket (today) or remote (`Machine`).
Everything the app does works against a remote quiverd: the library, downloads, settings,
profiles, mods, tweaks, Sight settings, play time, achievements and activity. Art and other big
blobs travel as they do today over the socket, and the client caches them by machine id.

Things that are about *this* screen stay local even when the library is remote: the app's own
settings, the stereo UI, the gamepad navigation, the theme.

## 5. Multiple machines: the UX

- **Machines.** The top bar gets a machine switcher next to the logo. It shows `This PC` and each
  paired machine, with a status dot (online, asleep, offline), and what is downloading on each.
  The default is a merged view.
- **One library, several machines.** The library shows each title once. A badge says where it is
  installed (This PC, Tower, or both) and where it would play best (the server's GPU). The game
  page's Play button splits: **Play on This PC** and **Stream from Tower**, with the last choice
  remembered per game. A title owned but installed nowhere offers **Install on…**.
- **Downloads per machine.** The Downloads page groups by machine. Each machine's queue is its
  own. The Omarchy widget shows the active machine's ring, plus a small letter badge when it isn't
  this one.
- **Settings scoped clearly.** A game's settings page names its machine ("Tower · Alan Wake
  Remastered · Torchbearer"). Profiles, mods and tweaks belong to the machine the game is
  installed on, and the "copy to…" action copies a profile between machines.
- **Streaming feels like playing.** A launch splash, then the stream window full-screen on the
  same output. The first time, a small "Ctrl+Alt+Shift+Q for the stream menu; Guide opens Sight
  as usual" hint appears. Closing the window asks whether to leave the game running on the
  server, so it can be resumed later. The server keeps the session for 10 minutes after a
  disconnect. A reconnect resumes it, and the library card shows **Resume on Tower**.
- **Wake.** A machine known to be asleep gets Wake-on-LAN from the switcher (its MAC is learned
  at pairing).
- **Sight on a stream.** Sight runs on the server inside the game, so it is in the video, and
  Guide/Shift+F2 reach it through the forwarded pad and keys. The client adds its own small stats
  chip (bitrate, RTT, loss, the latency split) and the stream menu: quality, codec, pacing,
  capture mouse, disconnect, quit game.

## 6. Licensing without signing in back and forth

The game always runs on the machine it is installed on, so its licence check is that
machine's: the same account, Denuvo or activation slot, and machine identity (STORES.md §7.4:
true and stable). A stream adds no machine. What has to work:

1. **Signing in to a headless server from the client.** The store sign-in flows
   (`SignInStep`, STORES.md §3.2) already render generically. Over the tunnel the client renders
   them. `OpenUrl` opens the *client's* browser or webview, and the pasted code or redirect goes
   to the server. The secrets stay on the server. This is still a person-only gate: agents never
   do it.
2. **Each machine its own session, from one sign-in.** For playing locally on two machines:
   - **Epic:** signed in on one machine, **Add to Tower** asks that machine's Epic session for an
     exchange code (`/account/api/oauth/exchange`), sends it over the paired link, and the other
     machine redeems it for its *own* refresh-token chain. That is how Epic's own clients hand a
     session over. The two chains rotate independently, so nothing fights.
   - **Ubisoft:** one live demux connection per account (PROPOSAL.md). Each machine holds its
     own remember-me session (UbiServices allows several devices) and connects demux only while
     its game runs. Getting the second session without the password and 2FA again needs a
     spike: is there a sanctioned session-to-session handoff, or must the person sign in once
     per machine? Until that is known, the client offers **Sign in on Tower** (point 1), which
     is one sign-in per machine, done from wherever the person is. If both machines would go
     live at once, the second refuses with "playing on <machine>". That matches Ubisoft's own
     rule.
   - **Steam:** Valve's client signs in on each machine itself (person-only). On a headless
     server, Steam runs in its own headless gamescope, and its sign-in window is streamed to the
     client like a game. Steam allows one playing session per account, and Quiver shows Steam's
     refusal as it is.
3. **Account presence.** Settings → Accounts shows, per account, which machines are signed in and
   which one is playing. Signing out on one machine never signs another out.

Save data follows the stores' cloud saves, plus Quiver's save stores (STORES.md §3.7). A switch
between machines therefore uploads on exit and downloads on launch, as between two PCs today.
The play-time and achievement reporters run where the game runs.

## 7. Phases

| Phase | What | Who |
|---|---|---|
| 0 | This doc and `quiver-stream-proto` | main session |
| 1 (parallel) | **net**: `quiver-stream-net` with the simulator and benchmarks. **host**: `quiver-stream-host` standalone (`--target game|lab|test-pattern`, writing frames to a file or a channel). **client**: `quiver-stream` standalone (plays an Annex-B/OBU file or a channel, with input capture shown in the overlay). **remote**: `quiver-remote`, `quiver remote enable/pair/…`, the remote API tunnel, the app's machine switcher and merged library. | four agents |
| 2 | **Integration**: `StartStream` in quiverd spawns the host with a ticket, and the app's Stream button runs `quiver-stream`. End-to-end over loopback on this laptop, streaming a lab instance and then a real game. Latency measured with the stage stamps, plus a photodiode-free check (a frame counter drawn by a test pattern, read back from the client's swapchain). | one agent |
| 3 | **Accounts**: remote sign-in, Epic exchange-code hand-over, the Ubisoft spike, the account presence UI. **Steam on headless**. | agents, with the person for every sign-in |
| 1b | **Moonlight compatibility**: `quiver-gamestream`, a GameStream-protocol front end over the same host (pairing, app list, RTSP, the control stream, RTP video and audio with Moonlight's FEC). It lets TVs and phones connect early. The person decided this on 2026-11-21; Quiver's own client stays the priority. Off by default, with its own switch. Clean-room: Sunshine and moonlight-common-c are GPL-3.0. | one agent |
| 4 | VAAPI and Vulkan Video encode, microphone, touch, Wake-on-LAN, a TV-friendly client mode, and a remote desktop target. | later |

## 8. Testing on one machine

The person's call (2026-11-21): test purely on this laptop for now. Network conditions come from
`quiver-netlab`: two rootless network namespaces joined by a veth, shaped with the kernel's
netem and tbf. No root is needed, and the real network is never touched.

This laptop is both ends: a scratch quiverd (with `QUIVER_SHM_ROOT`, a scratch data dir and a
non-default port) acts as the server, and the client runs on the desktop. Tests use
headless gamescope (never X displays :0–:9), the test-pattern target, and the net simulator
(loss, delay, jitter, reordering, bandwidth caps, all in userspace, with no `tc`, which needs
root). Agents stream lab instances, never the person's running game. They don't stop or slow
the person's games, and the lab limits apply to streams of lab instances too.

## 9. Not decided (the person's call)

- Reachability: are the LAN and Tailscale enough, or is NAT traversal or a relay wanted?
