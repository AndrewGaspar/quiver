# quiver-gamestream

A GameStream-protocol server, the protocol Moonlight clients speak, as a second front end
over Quiver's remote-play host (design/REMOTE.md, phase 1b). The crate docs (`src/lib.rs`)
cover the API, the ports and the security model.

## Ports (for firewalls and Tailscale ACLs)

| Protocol | Port | What |
|---|---|---|
| TCP | 47989 | HTTP: server info, pairing |
| TCP | 47984 | HTTPS (client certificates): app list, box art, launch, resume, quit |
| TCP | 48010 | RTSP (encrypted) |
| UDP | 47998 | video |
| UDP | 47999 | control and input (ENet) |
| UDP | 48000 | audio |

`Ports::from_http(p)` moves them all together (HTTPS p−5, video p+9, control p+10, audio
p+11, RTSP p+21); Moonlight is told the others, so only the HTTP port has to be entered. No
mDNS: Moonlight adds the server by address or name (a Tailscale IP or MagicDNS name).

## Clean-room note

Sunshine and moonlight-common-c are GPL-3.0; this crate is MIT. No code from them was copied
or paraphrased. The wire formats were learned by reading those projects and Moonlight's
behaviour, then written fresh here, and checked against the real `moonlight` client
(`tests/moonlight_e2e.rs`). What informed each part:

| Part | Sources read (for formats and behaviour only) |
|---|---|
| Pairing (`pairing.rs`, `nvhttp.rs`) | moonlight-qt `nvpairingmanager.cpp`, `nvhttp.cpp`, `nvcomputer.cpp`, `identitymanager.cpp` (the client's side of each stage, the XML fields read); Sunshine `nvhttp.cpp` (the fields `/serverinfo` and `/launch` return) |
| HTTP endpoints, app list, launch parameters | moonlight-qt `nvhttp.cpp`; Sunshine `nvhttp.cpp`; moonlight-common-c `Limelight.h` (codec mode flags, audio configuration) |
| RTSP and SDP (`rtsp.rs`, `sdp.rs`) | moonlight-common-c `RtspConnection.c`, `SdpGenerator.c`, `RtspParser.c` (requests, encrypted framing, the attributes sent and parsed); Sunshine `rtsp.cpp` (the DESCRIBE and SETUP answers) |
| Control stream (`control.rs`, `enet.rs`) | moonlight-common-c `ControlStream.c` (message types, V2 encryption, IVs, payload layouts), `Limelight-internal.h` (channels); ENet itself is `rusty_enet`, a port of the MIT-licensed ENet |
| Input (`input.rs`) | moonlight-common-c `Input.h`, `InputStream.c`, `Limelight.h` (packet layouts, byte orders, flags); the virtual-key to evdev table is written from the public Windows and Linux key code lists |
| Video (`video.rs`, `fec.rs`) | moonlight-common-c `Video.h`, `VideoStream.c`, `RtpVideoQueue.c`, `VideoDepacketizer.c` (NV header, FEC info, frame header, encryption); Sunshine `stream.cpp` (block splitting, which fields are filled after parity); nanors (MIT) for the Cauchy matrix and field polynomial |
| Audio (`audio.rs`) | moonlight-common-c `AudioStream.c`, `RtpAudioQueue.c` (FEC header, the fixed audio parity matrix, CBC IV); Sunshine `stream.cpp` (FEC packet sequence numbers) |
| Packet size and MTU | moonlight-qt `session.cpp`, moonlight-common-c `Connection.c`, `PlatformSockets.c` (how Moonlight picks its packet size and what it treats as local) |

## Testing

`cargo test -p quiver-gamestream` runs the unit tests (pairing crypto against vectors from
`openssl` and `sha256sum`, RTSP and SDP parsing, packetisation and FEC recovery, input
decoding). The end-to-end test with the real Moonlight client is opt-in:

```text
QUIVER_E2E_MOONLIGHT=1 cargo test -p quiver-gamestream --test moonlight_e2e -- --nocapture
QUIVER_E2E_MOONLIGHT=1 cargo test -p quiver-gamestream --features stream-host --test moonlight_e2e -- --nocapture
```

The second streams `quiver-stream-host`'s NVENC test pattern instead of an encoded file.
`tests/moonlight-e2e.sh` says how the run is kept away from the desktop session.
