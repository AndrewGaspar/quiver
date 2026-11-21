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
  `quiver remote disable` turns it off. On a headless server, `quiver remote enable --at-boot` also
  starts quiverd at boot, before anyone signs in (the `quiverd.service` user unit with
  lingering: design/SERVICE.md).
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
  `source: remote:<peer name>` and are allowed as the app's would be. A client that labels
  itself as something else (an agent) keeps that label: `remote:<peer name>/agent` (§6.1). A few requests are
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
     its game runs. The spike (§6.2) found no sanctioned session-to-session hand-over, so the
     client offers **Sign in on Tower** (point 1): one sign-in per machine, done from
     wherever the person is. If both machines would go
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

### 6.1 As built (phase 3, 2026-11-21)

Nothing in this phase signed in to a store or called one. The Epic hand-over is tested between
two scratch quiverds against a stand-in for Epic on 127.0.0.1; Ubisoft was read about, not
called.

**Protocol** (additive; `quiver_api::accounts`):

| Request | What | Who |
|---|---|---|
| `Accounts(Local)` | This machine's accounts in every store (Ubisoft, Epic), each with the game it is playing. No secrets | anyone; what machines ask each other |
| `Accounts(Presence { wait_ms })` | Every account across this machine, its paired servers (asked now, 3 s each) and its paired clients (what they announced in the last 75 s), merged per account: the machines it is signed in on and the one playing | anyone |
| `Accounts(Handover { store, account, from, to })` | Give machine `to` its own session from machine `from` (`None` is this machine; names as `quiver remote peers` lists them). Epic only | the person |
| `Accounts(Export { store, account })` | A one-time hand-over code from this machine's session (`HandoverCode`) | the person, over the tunnel only |
| `Accounts(Import { store, code })` | Redeem one; adds the account here | the person |
| `Accounts(Announce(MachineAccounts))` | A paired client telling a server what it is signed in to and playing | paired machines only, kept under the name they are paired as |
| `Event::AccountsPresenceChanged` | Reload `Presence` | |

`Handover` is always sent to the app's or CLI's own quiverd. That machine is paired with every
machine the person sees, so it can do either direction: it mints the code locally or asks
`from` for one (`Export`), then redeems it locally or sends it to `to` (`Import`). A server
can't reach a client, so a hand-over from Tower to a laptop is run by the laptop ("add from").
Every machine runs an announcer: at start, whenever its accounts or games change, and every
30 s while a game runs, it sends `Announce` to its servers. A server therefore knows a client
is playing even though it can't ask the client.

**Sources and gates.** Over the tunnel a request's source is `remote:<peer>` when its client
said `cli`, `gui` or `sight`, and `remote:<peer>/<what it said>` otherwise, such as
`remote:laptop/agent` (`remote::remote_source_as`; until now every remote request was
`remote:<peer>`). `remote::is_person` accepts the person here or remote. Signing in and out
(Ubisoft's `Login`, `Login2fa` and `Logout`, Epic's `LoginComplete` and `Logout`), `Handover`
and `Import` require it. `Export` additionally requires the tunnel, since only another
machine has a use for the code. Ubisoft's sign-in had no source gate before; it has the same
one as Epic's now.

**What crosses the tunnel, and redaction.**

- *Ubisoft sign-in on another machine:* the email, password and 2FA code are typed on the
  client and sent in `Login` and `Login2fa` inside the pinned mutual TLS. `Request::Login.password`
  and `Login2fa.code` are now `Secret<String>` (moved from `quiver-core` to
  `quiver_api::secret`, and re-exported there), so a printed request shows `<redacted>`.
  Serde still sends the value. The server stores only its own remember-me ticket, in its own
  keyring.
- *Epic sign-in on another machine:* the authorization code from Epic's page, produced in
  the client's sign-in window or browser, goes in `LoginComplete` as before. `EpicRequest`'s
  `Debug` already hides it.
- *Epic hand-over:* the exchange code. It is minted on the giving machine, travels once (in
  `Export`'s reply or in `Import`), is a `Secret` in every type (`HandoverCode`, `Import`'s
  `Debug` is written out by hand), and is never logged or put in activity. Epic makes it
  single-use and minutes long. No refresh or access token ever leaves its machine.
- Presence carries account ids, names, states and game names, nothing secret.

**Epic hand-over** (`epic-core`): `Epic::handover_code(account)` asks Epic for an exchange code
with this machine's access token (`GET /account/api/oauth/exchange`), leaving the session as
it was. `Epic::login_exchange(code)` posts `grant_type=exchange_code` and stores the new
session as a sign-in does: in the keyring before the account is listed. Its refresh token
starts a chain of its own, so the two machines rotate independently and signing out on one
leaves the other signed in. Epic launches also record their account in the local session
(`LaunchMaterial.account`), so presence knows who is playing an Epic game.

**"Playing on <machine>".** Before a Ubisoft launch (`Request::Play` of an installed Ubisoft
game), quiverd asks its paired servers (2 s each) and reads its clients' announcements. If
the launch account is playing elsewhere, it refuses: "<account> is playing <game> on Tower.
Ubisoft allows one live session per account, so starting here would end that one: stop it on
Tower first". A machine that can't be reached doesn't hold up a launch. Lab instances count
as playing, since they hold the account's live session too.

**Test isolation.** `QUIVER_SCRATCH_SECRETS=<dir>` makes every keyring entry of that quiverd
a 0600 file in a 0700 directory, through the `keyring` crate's credential-builder hook. It
covers both Ubisoft's `session:` entries and Epic's `epic:` entries, so a scratch daemon never
reaches the desktop's Secret Service. `QUIVER_EPIC_BASE=http://127.0.0.1:<port>` sends every
Epic call to a stand-in; any value that isn't a loopback `http://` address is refused. Before
this, scratch daemons were kept off the keyring only because `env_clear` left them no session
bus.

**Tests.**
- `crates/quiverd/tests/accounts.rs` runs two scratch quiverds, paired, plus an Epic
  stand-in that models refresh chains and single-use exchange codes. It checks:
  - The laptop signs in.
  - An agent is refused, here and through the tunnel (`remote:laptop/agent`). A local
    `Export` and a Ubisoft hand-over are refused too, and nothing is minted for any refused
    request.
  - **Add to tower**: the tower's scratch keyring holds a refresh token from a new chain.
    The laptop's is byte-for-byte the same. The code was minted with the laptop's session
    and redeemed once.
  - Neither log contains a token or code.
  - Presence on both machines, the tower's coming from the laptop's announcement.
  - The laptop signs out and takes a session back **from tower** (`Export` over the
    tunnel). The tower's session is untouched, and its activity log records `remote:laptop`.
  - An announced Ubisoft game shows as "playing on laptop" in the tower's presence.
- `epic-core`: `handover_gives_the_other_machine_its_own_session` (Fake transport) and the
  loopback-only base.
- `quiver-api`: presence merging, `live_elsewhere`, the wire forms and that no secret is
  ever printed, and the remote source rules.
- `quiver-core`: the scratch store's file modes.

**CLI.**
- `quiver account machines [--json]`: every account, its machines, and what is playing.
- `quiver epic add-to <machine> [--account A]` and `quiver epic add-from <machine>`.
- `quiver --machine tower login` and `quiver --machine tower epic login` sign Tower in from
  here. Epic's sign-in window opens on this machine, and the code goes to Tower.

**App.** Settings › Accounts:
- With a paired machine chosen in the switcher, the page is that machine's: "Tower's
  accounts…", **Sign in on Tower** / **Add account on Tower** (Ubisoft's form and Epic's
  window run here, and the session stays on Tower).
- Each Ubisoft and Epic account card has a presence line: "Also signed in on This PC",
  "Only on this machine", or "Playing Assassin's Creed Origins on Tower · its games won't
  start here until that ends", in amber.
- Epic cards get **Add to <machine>** for each machine that is up and doesn't have the
  account. A machine without Epic gets **Add <account> from <machine>**.
- The game page's account stat reads "demo · playing on Tower", and the overview gets a
  *Live session: In use on Tower now* row.

### 6.2 Ubisoft: a second machine's session (spike, desk research only)

The question: can machine B get its own UbiServices session from machine A's, without the
password and 2FA again? The research read public write-ups and open-source projects'
documentation and issues. No code was copied. Quiver made no request to any Ubisoft domain.

| Question | Finding |
|---|---|
| A sanctioned hand-over, device-code or QR login | **None found.** No OAuth device grant, "sign in with a code", or phone-approves-PC flow is documented for UbiServices. The QR codes in Ubisoft's help are for setting up an authenticator app. GeForce NOW's TV QR code opens Ubisoft's ordinary login page on the phone. The Connect mobile companion app reportedly shut down in April 2025 (unconfirmed). Community API notes describe a "ticket exchange": re-posting `/v3/profiles/sessions` with `ubi_v1 t=<ticket>` and another `Ubi-AppId` returns a ticket scoped to that app (wnc-replay/ubi-docs, `guide/ticket-exchange.md`). Nothing says it gives another *device* an independent session or a remember-me ticket. A `delegation-ubiservices` host appears in reverse-engineering notes without details. Using either would be our own invention, not a sanctioned flow |
| `rememberDevice` | Real, and **per device**. Completing 2FA with `trustedDevice: {id, friendlyName}` returns a `rememberDeviceTicket`. Later logins send `Ubi-RememberDeviceTicket` and skip the 2FA prompt (UplayDB/UbiServices, MIT, `V3/Login.cs`). Each trusted device is a named entry in the account's Trusted Devices list, which has a cap, and "untrust all" signs every one out. Copying A's device ticket to B would present B as A |
| One remember-me ticket on two machines | **No.** Ubisoft retires the previous remember-me ticket at each renewal (unifideck's measurements and issue #435; the GOG Galaxy plugin keeps whatever each `rm_v1` renewal returns; Ubisoft-Connect-RE saw 401s after a failed renewal). Two holders would sign each other out, as Epic refresh tokens would |
| What other launchers do | Nothing across devices. The GOG Galaxy plugin (MIT) signs in through Ubisoft's web login in an embedded browser, one install at a time. Its retry loops once sent repeated 2FA emails and got accounts suspended (issue #33). ubisoft-demux-node, Lutris, GameNative, DroidDeck and unifideck sign in once per install. Playnite reads the Connect client's files |

**Recommendation: one sign-in per machine, done from wherever the person is** (point 1, now
built). The client's form collects the email, password and 2FA code and sends them over the
pinned tunnel. Machine B makes its own `POST /v3/profiles/sessions` and keeps its own
remember-me ticket. The credentials are used once and never stored. Not allowed (STORES.md
§7, items 4, 7 and 8):
- copying A's remember-me or device ticket to B;
- deriving B's session from A's ticket through ticket exchange or `delegation-ubiservices`;
- using the mobile app's AppId;
- making up or rotating device ids to get past the trusted-device cap.

Two follow-ups for the Ubisoft sign-in code (not built here):
- Send an honest `trustedDevice` (a stable id per machine, named "Quiver on <host>") when
  completing 2FA, so each machine is one named trusted device and stops asking for codes.
  That is what Connect does.
- Never let two processes share one remember-me ticket. unifideck's #435 is per-game prefixes
  signing each other out, which is relevant to Quiver's bridge if it ever renews tickets
  outside quiverd.

A Steam-ticket login (accounts linked to Steam) is a legitimate per-machine alternative,
outside this phase.

### 6.3 Steam on a headless server (design; not built)

Valve's client signs in on each machine itself, and Quiver never sees a Steam password
(STORES.md §7, item 9). On a headless server the person still has to see Steam's own sign-in
window, so Quiver streams it.

1. **Steam in its own headless gamescope.** When a Steam game is wanted on Tower and Tower's
   Steam isn't running, quiverd starts `steam -silent` inside a headless gamescope of its own
   (the same `gamescope --backend headless` the stream host uses, LAB.md §1), sized for the
   client, under the person's uid and Steam root. That gamescope is long-lived: Steam stays
   up across games, as it does on a desktop.
2. **Signing in is a stream of that display.** Settings › Accounts › Steam on Tower shows
   Steam's state (STORE-STEAM's local mode: the client's most recent user, `ClientManaged`).
   When it says Steam needs a sign-in, **Sign in to Steam on Tower** asks for
   `StartStream { target: SteamClient }`, a new `StreamTarget` that names the Steam display
   instead of a game. The client opens the stream window on Steam's login dialog. The person
   uses Steam's QR code (scanned with their phone) or types the password and Steam Guard
   code into Steam itself, through the forwarded keyboard. Quiver relays pixels and keys
   only and sees no credential. Closing the window ends the stream, and Steam keeps running.
3. **Playing.** A Steam game launched on Tower runs under that Steam, through steam-run's
   hook as today (STORE-STEAM.md §9). The stream host captures the game's gamescope.
   Whether the game nests in Steam's gamescope or gets its own is the integration's
   choice; its own keeps the stream at the game's size and rate. Steam's own "playing
   elsewhere" refusal (one playing session per account) is shown as Steam words it, and
   Quiver doesn't add a check of its own.
4. **Presence.** `Local` reports Steam's most recent user as a `ClientManaged` account (from
   `loginusers.vdf`'s non-secret fields, as `Stores` does today), and presence shows which
   machines have Steam signed in. Playing comes from steam-run sessions.

**Needed from the integration work** (StartStream, the stream host and the client):
- a `StreamTarget::SteamClient` (or a generic "named display") in `quiver-stream-proto`;
- the host able to attach to an existing gamescope's PipeWire stream instead of spawning one,
  and to keep it alive after the client leaves;
- keyboard text input and pointer clicks in the stream reaching Steam's login UI (uinput or
  XTest in the headless gamescope);
- quiverd owning that gamescope's lifetime (start on demand, stop on `quiver steam stop` or
  idle, never while a game runs); and the lab's GPU and CPU limits leaving it alone, since
  it's the person's Steam, not a lab instance.

"Restart Steam" stays Ask for agents (STORES.md §7, item 10), on a server too.

## 7. Phases

| Phase | What | Who |
|---|---|---|
| 0 | This doc and `quiver-stream-proto` | main session |
| 1 (parallel) | **net**: `quiver-stream-net` with the simulator and benchmarks. **host**: `quiver-stream-host` standalone (`--target game|lab|test-pattern`, writing frames to a file or a channel). **client**: `quiver-stream` standalone (plays an Annex-B/OBU file or a channel, with input capture shown in the overlay). **remote**: `quiver-remote`, `quiver remote enable/pair/…`, the remote API tunnel, the app's machine switcher and merged library. | four agents |
| 2 | **Integration**: `StartStream` in quiverd spawns the host with a ticket, and the app's Stream button runs `quiver-stream`. End-to-end over loopback on this laptop, streaming a lab instance and then a real game. Latency measured with the stage stamps, plus a photodiode-free check (a frame counter drawn by a test pattern, read back from the client's swapchain). | one agent |
| 3 | **Accounts**: remote sign-in, Epic exchange-code hand-over, the Ubisoft spike, the account presence UI (built: §6.1, §6.2). **Steam on headless** (designed: §6.3). | agents, with the person for every sign-in |
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

### 8.1 Network conditions (`quiver-netlab`)

The net simulator covers unit tests. For real processes over a real link, `quiver-netlab`
shapes the link with the kernel's own qdiscs (netem, tbf, fq_codel, cake). That needs no root
after all: the netlab makes a user namespace the person owns and puts three network namespaces
in it (server, router, client). The shaping sits on the router's interfaces, and the host's
network is never touched. Workloads run as the person's own uid in a nested user namespace.
Only the network namespace changes, so the GPU, PipeWire and the Wayland socket work as usual.
The crate's README covers the details.

- `quiver-netlab list` shows twelve profiles with their sources. They run from `lan-wired`
  through `wifi6-good`, `wifi-far`, `wifi-congested`, `5g`, `lte`, `cafe`,
  `tailscale-relay`, `cross-country` and `transatlantic` to `bufferbloat-cable` and
  `pathological`. Seven scenarios change a profile over time: `step-down`, `microwave`,
  `outage`, `roam` (the client's address changes), `latency-creep`, `competing-tcp` and
  `competing-udp`.
- `quiver-netlab verify --profile all` measures each profile (one-way delays, RTT, loss, rate
  and loaded RTT) against a model of netem. The crate's tests do this for every profile.
- `quiver-netlab run --scenario step-down --server '<host cmd>' --client '<client cmd>'` runs
  the two sides. They get `QUIVER_NETLAB_SERVER_ADDR` and `QUIVER_NETLAB_CLIENT_ADDR`, and a
  stats file (`QUIVER_NETLAB_STATS`, also on fd 3) for timestamped `ClientStats` and
  `HostStats` JSON lines. The run writes `report.json` and a self-contained `report.html`:
  bitrate against capacity, latency, queueing, loss and frame gaps, with the events marked.
- `quiver-netlab matrix --profiles all --scenarios all --server … --client …` writes one
  summary table: median and p99 latency, gaps per minute, bitrate against capacity, and
  recovery after a step-down. Until the host and client exist, `--fake` runs a stand-in UDP
  stream (fixed or AIMD bitrate) on both sides.
- `quiver-netlab shell --profile lte` gives a shaped shell for poking around.
  `quiver-netlab set` changes the shaping from another terminal while the shell runs.

Integration (phase 2) should make `quiver-stream-host` and `quiver-stream` write the stats
lines when `QUIVER_NETLAB_STATS` is set, and run the matrix before each transport change
lands.

## 9. Not decided (the person's call)

- Reachability: are the LAN and Tailscale enough, or is NAT traversal or a relay wanted?

## 10. Phase 2 as built (2026-11-21)

**The pieces, end to end.** `StartStream` (quiverd `streams.rs`) binds a media port (from
47998 up, or `remote.stream_port`), makes fresh keys for every request, and starts
`quiver-stream-host serve` in its own scope (`systemd-run --user --scope`), the port as fd 3
and the spec, keys included, as the first line on its stdin (`quiver_stream_proto::serve`).
The reply's ticket names the address the client's control connection came in on. The client
(`quiver-stream connect`) reads the ticket on its stdin and runs `ClientSession` under the
stream window (`net.rs`, as `link.rs` describes). `quiver-stream caps --json` gives the caps
the app and `quiver stream` send.

**Sessions.** One stream per target. A client that leaves (Disconnect, closing the window,
silence) leaves the session kept for 10 minutes: the game runs on, and the next `StartStream`
for it resumes with new keys on the same port, the game untouched (the test pattern stops
meanwhile and starts again). Another machine asking while one streams is refused. Quit game
(`Bye{reason: "quit-game"}`) ends the stream and quiverd stops the game; the game exiting
ends the stream. Streams show in `quiver status`, `Request::Streams` and `StreamsChanged`
("streaming to Laptop", "kept for 8 min (until 14:05), resume from Quiver on Laptop");
`quiver stream --list|--stop`. The keep window is `remote.keep_secs` (600).

**Moonlight sessions** keep the game the same way (`quiver_gamestream::keep`). Moonlight
going (the app swiped away or backgrounded, the network gone, the control stream timing out)
is a disconnect, never a quit: the game, its headless gamescope, sink and pads run on for
`remote.keep_secs`, and `/serverinfo` reports it as `currentgame` with a `_SERVER_BUSY`
state, so Moonlight shows it running with Resume and Quit. `/resume`, or `/launch` of the
same app, attaches the new RTSP/ENet/RTP session to the running host with the new launch's
keys: frames queued meanwhile are dropped, the host is asked for a keyframe at once, a
different codec or colour goes to the encoder, and a different size or rate is logged and
kept (a game's display can't resize; Moonlight scales). Launching another app while one runs
gets GameStream's (and Sunshine's) 400 "An app is already running on this host", so Moonlight
asks the person to quit it first. Quit app (`/cancel`), `quiver stream --stop`, the game
exiting, or the window ending stops the game. The kept game shows in `Request::Streams` as
"kept for 8 min (until 14:05), resume from Moonlight (roth)". **One stream per game across
both front ends:** Moonlight's host is in quiverd and Quiver's transport's is a `serve`
process, so a session can't move between them; each refuses the other's game, saying where
to resume it.

**A still picture.** gamescope paints only on damage, so a paused game sends no frames. The
capture sources now report a quiet 100 ms (`Next::Idle`), and a keyframe asked for meanwhile
(a session connecting or resuming, a loss) is made by encoding the last frame again (a new
encoder session for another codec takes the last frame along). A resumed stream of a still
game gets its picture at once, natively and from Moonlight.

**Restarts.** The host isn't quiverd's: an exec onto a new build leaves it running, and the
new build re-adopts it from `$XDG_RUNTIME_DIR/quiver/streams/<id>/` (its record and the
host's `control.sock`). Measured: a kept test-pattern stream re-adopted after a restart and
resumed from the client with new keys, same host pid.

**Games.** `StreamTarget::Game` runs the host's headless gamescope with a placeholder
(`Target::Display`), then `Play` exactly as today (launch plan, profile, bridge, Sight,
tweaks) with the host's `LaunchEnv` in the plan: its `DISPLAY`, its sink, the Sight layer's
capture socket, and the stream's pad group (`pad_gid`). The launch then runs every process
(bridge, Proton, wineserver, Wine's controller driver, the game) in one user namespace of its
own, with mods mounted if the profile has them and the pad group mapped in with
newuidmap/newgidmap and given to each process (`overlay::Namespace::create_with`). A
pad group that can't be mapped, a wineserver of the prefix left outside, or a process found
without the group 8 s in turns the stream's pads off with a notice and an activity line,
never a pad nobody can open. (Until 2026-11-21 a modded launch's namespace mapped only the
person's own ids, so a Moonlight stream of Origins with its Pharos profile had a pad the
game couldn't open; `quiverd/tests/stream_pads.rs` covers both.) Tested with a Wine test program registered in a scratch daemon
(`QUIVER_TEST_GAMES`), launched through `play_store_game` as an Epic game is: keys and clicks
reached it, and Quit game from the stream menu stopped it. Not tested with a store game (the
person's call). Moonlight launches go through the same display and launch
(`streams::MoonlightGames`, in-process).

**Not built:** `LabInstance` (the instance's gamescope is inside its container, with no
PipeWire for the screencast and its X display in the container's network namespace: it needs
a capture and an injector inside the container, under the lab limits) and `Desktop`; resizing
a running game's display; a stream moving between Moonlight and Quiver's own client;
a Moonlight game surviving a quiverd restart (its host is in quiverd, unlike `serve`).

**Measured on this laptop** (RTX 5070 Laptop; host and client on one GPU, the client in a
nested headless gamescope at 2560×1440@120, HEVC, the test pattern's stamp read back from
every decoded frame, present time from `VK_KHR_present_wait`):

| | capture → present p50 / p99 | decode p50 | stamps right |
|---|---|---|---|
| loopback, Vulkan Video decode | 4.8 / 7.7 ms | 1.0 ms | 2356 / 2356 |
| loopback, Vulkan Video, a game running on the same GPU | 5.3 / 7.0 ms | 1.2 ms | 1128 / 1128 |
| loopback, NVDEC, a game running | 7.7 / 9.9 ms | 3.5 ms | 2338 / 2338 |
| two scratch quiverds, `quiver stream --target test-pattern` (debug builds) | 14.8 / 26.3 ms | 1.1 ms | 876 / 876 |

**Under the netlab** (`quiver-netlab matrix`, all profiles and scenarios, 15 s a profile;
the real host and client, NVDEC; the pattern's bottom 6 % noise so the encoder spends what
it's given, `QUIVER_STREAM_PATTERN_NOISE`): the commit "Remote play tuning from the netlab"
has the table before and after. After tuning, p50 / p99 ms: lan-wired 7.7 / 33, wifi6-good
11.6 / 15, wifi-far 19.2 / 24, 5g 39 / 138, lte 47 / 169, tailscale-relay 42 / 76,
cross-country 47 / 135, transatlantic 51 / 91, bufferbloat-cable 20 / 155. The tuning:
overuse needs real queueing (8 ms, or 4× the delay noise, above the 2 s minimum), the
client's wait for an overtaken frame follows the jitter, and the FEC margin is 2.5.

**Open problems found:**
- libav's Vulkan Video decode path sometimes ends in `VK_ERROR_DEVICE_LOST` (netlab
  `wifi-congested` and `microwave`, and once on loopback with a game on the same GPU); the
  stream now ends with that error instead of freezing. NVDEC never did. Not root-caused: it
  may be the one-GPU test setup (the host's game and the client share it).
- The pattern's noise can't go below ~12 Mbit/s at the encoder's highest QP, so the
  `pathological` profile (15 Mbit/s, 5 % bursty loss) measures the workload, not the link.

## 11. A headless server, start to finish (2026-11-21)

A machine with a GPU and no one at it (a tower in a closet), streaming to your laptop, TV and
phone. Every step but one runs as you; the one root step is a script you read first
(design/INSTALL.md).

1. **Install Quiver** as yourself: `tools/install-user.sh` from a checkout (or the Arch
   package, `tools/package.sh`, which also installs the udev rules, groups and firewall
   profiles). Sign in to your stores once (`quiver login`, `quiver epic login`), over SSH if you
   like.
2. **The root step:**

   ```sh
   quiver system setup --firewall lan,tailnet --at-boot
   ```

   It checks the machine without root, then prints one script with exactly what's missing:
   the groups and udev rules for stream (and lab) controllers, the `/dev/uinput` rule a
   machine with nobody seated needs, Quiver's firewall profiles, firewall rules from your LAN
   subnet(s) and `tailscale0` only (never from anywhere; only for Moonlight and remote play
   once they're on, so run it again after turning one on), and lingering where polkit doesn't
   let you turn it on yourself (it doesn't over SSH on most systems). It saves the script to
   `~/.local/state/quiver/system-setup-<date>.sh`, asks `Run it with sudo? [y/N]`, and
   `--at-boot` then runs `quiver remote boot on`, so quiverd starts at boot before anyone signs
   in. `quiver system setup --check` shows where each piece stands; `--print` only prints the
   script; `--undo` removes what it installed.
3. **Turn remote play on:** `quiver remote enable` (TCP 47990, and a UDP media port per
   stream from 47998). For Moonlight clients too: `quiver remote moonlight enable`, then
   `quiver system setup --firewall lan,tailnet` again if it says the firewall doesn't allow
   Moonlight yet (`quiver remote moonlight enable` prints that line when it applies).
4. **Pair your machines:** `quiver remote pair` on the server prints a 6-digit code; type it on
   the client (Settings → Machines → Add, or `quiver remote add <server> <code>`). Moonlight
   clients pair with the PIN they show: `quiver remote moonlight pin 1234`.

`quiver doctor` shows the build, quiverd, and the same system setup list at any time. Pads
in streams work with nobody seated because the stream host opens `/dev/uinput` through a
short-lived helper that alone gets the `quiver-uinput` group (INSTALL.md §7); without the
rule, a stream's notice says to run `quiver system setup`.
