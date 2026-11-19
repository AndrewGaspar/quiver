# Quiver lab: disposable game instances for agentic modding

Status: built 2026-11-18 (`quiver lab`, `quiver_core::lab`, `Request::LaunchMaterial`, lab
sessions in the broker). Not yet run against a real game: section 9 is the first live run.

The Pharos mod (AC Origins DLSS, Reflex, frame generation) was built over a week by an agent
driving the game through a private harness (`ac-dlss/scripts/aco` and its `tools/`):
reflink clones of a "gold" prefix in podman, gamescope headless, NVIDIA through CDI, and the
licence through Quiver's bridge and broker. `quiver lab` makes that a standard, store-agnostic
Quiver capability, with every lesson of that week designed in, so that **starting an agent on
modding a new game is one step**:

```sh
quiver lab brief "<game>" -o brief.md      # paste it into a new agent session
```

## 1. What it is

An **instance** is a disposable, headless copy of a game the user has installed, running in a
container, launched with the user's real licence, that an agent drives from the command line:
launch, wait for a real readiness signal, press keys, move the mouse, take screenshots, edit
config files live, measure, kill, relaunch. Nothing an instance does reaches the user's
accounts, saves, play time or achievements.

```
quiver lab create <game> [--name N] [--display 2560x1600@165[+hdr]] [--proton P] [--mod DIR] [--graphics G] [--sight] [--hdr]
quiver lab up N | down N | rm N | list | show N | edit N key=value…
quiver lab launch N [--mod DIR] [--wait frame]     quiver lab kill N
quiver lab status N | wait N STATE [--stable]       quiver lab logs N [--tail 40]
quiver lab shot N [--settle S] [--crop C] [--metrics]
quiver lab key N KEY… | type N TEXT | mouse N DX DY | click N FX FY [--expect-change]
quiver lab set N FILE:[section]key=value… [--wait] | profile N [NAME]
quiver lab fixture save|install|list … | call N VERB … | where N | teleport N LOC
quiver lab gpu [--for N] -- CMD… | bench N --variant A=… --variant B=… … | host
quiver lab brief <game> | doctor | image build|status | job list|show|wait
quiver lab sight N status [--json]                  (instances with sight = on, section 12)
quiver lab hdr N [--json] | shot N --hdr [OUT]      (instances with display.hdr = on, section 13)
quiver lab … --bg      (any command, as a background job)
```

Game arguments are refs as everywhere in Quiver (`ubisoft:3539`, `epic:<app>`, `steam:<appid>`,
or a name), `--json` where output is data, and `--source` labels the caller in quiverd's
activity log (launch material is logged as "issued lab launch material … session lab-…").

## 2. The model

**Folders** (`$QUIVER_LAB_ROOT`, default `quiver-lab` next to Quiver's prefixes folder, so on
the same filesystem as the installs):

```
gold/<game dir>/{prefix,install,gold.json}     the game as the user plays it, Connect's login removed
instances/<name>/lab.toml                      the manifest
                 prefix/ install/              reflink clones of the gold copy
                 lab/                          mounted at /lab (Z:\lab): the lab channel, runs, shots
                 lab/runs/r<N>/                one folder per launch: game.log, bridge.log, DXVK/vkd3d
                                               logs, host.json, the mod's own log (QUIVER_LAB_RUN_DIR)
                 state.json events.jsonl       the supervisor's view; one event line per change
                 orig/ mod-files.txt           install files a mod replaced, put back before the next launch
fixtures/<game dir>/<name>/                    save fixtures (read-only), locations.txt
profiles/<game dir>/<name>.set                 graphics profiles (config-set lines)
jobs/<id>/                                     background jobs
gpu.lock gpu.holder gpu.log                    the GPU-exclusive lock
```

**The gold copy** is made once per game (`create`, or `create --regold`): `cp -a
--reflink=always` of the game's prefix and install (refused while anything runs in that
prefix; refused on a filesystem without reflinks rather than copying a whole game), then the
Connect guard (section 4). Instances are reflink clones of the gold copy: free, and nothing in
an instance points back at the user's folders.

**The manifest** (`lab.toml`, `quiver_core::lab::manifest`, `deny_unknown_fields`) pins what
a launch runs: the display (`width`, `height`, `refresh`; every tool reads the size from here,
clicks are fractions of it), the Proton build (default: the one the user plays the game with)
with the DXVK, vkd3d-proton and dxvk-nvapi versions it ships, the original install and prefix
paths (the clones are **mounted at those paths** in the container, so paths the game or its
registry recorded keep working), the exe and its arguments, environment and DLL overrides
(the game's Quiver settings when made), the mod folder, the graphics profile, the save folders
fixtures copy, file aliases for `set` (`pharos = "install:pharos.ini"`), `throttle` lines for
the GPU lock, and the mod's `reload_pattern` and `log_file`. Change it with `quiver lab edit`.

**The container** (`quiver_core::lab::podman`, image `localhost/quiver-lab:1` from
`contrib/lab/Containerfile`, built with `quiver lab image build`):

| Choice | Why (Pharos lesson) |
|---|---|
| gamescope `--backend headless` as the display | Xvfb makes NVIDIA's Vulkan present a ~70 ms CPU copy per frame (1). Xvfb isn't in the image. |
| its own network namespace (`--network=pasta:-T,<broker port>`), never `--network host` | with host networking the container shares the host's abstract X socket namespace and once took `:0`, capturing the desktop's clients (2). pasta's `-T` forwards the container's `127.0.0.1:<port>` to the host's broker. Verified: from inside, `:0`, `:1` and the host's other displays are unreachable, and the forward works. |
| `:0`–`:9` reserved inside the container | belt and braces for (2): lock files naming gamescope's pid; the display script refuses a single-digit display. Every `exec` reads the display gamescope reported (`/tmp/display`), never a default. |
| CDI `nvidia.com/gpu=all` plus `/usr/lib32` NVIDIA libraries mounted read-only, lib32 Vulkan in the image | CDI injects only the 64-bit driver (3) |
| `--init` | gamescope doesn't reap orphaned Wine processes (4) |
| `--userns=keep-id`, the host's hostname and `/etc/machine-id` | files keep the user's ownership; the machine looks the same to Ubisoft and Denuvo |
| `/usr/lib/nvidia/wine` mounted | NVIDIA's NGX DLLs (DLSS) |
| per-instance NVIDIA shader cache (`lab/cache/nv`) | relaunches don't recompile |
| ALSA's default is a null sink (`$HOME/.asoundrc`) | there is no sound device; AWR crashed at startup without one (3.2) |
| Epic's names resolve to the instance's EOS proxy, port 443 forwarded to it | EOS saves, achievements and stats never reach Epic (3.1) |

## 3. Licensing, per store

The rule is Quiver's (STORES.md §7): the store's real backend decides, for the user's own
account, through Quiver; no emulators, no entitlement stubs, no second login.
`Request::LaunchMaterial { game, session, … }` (quiverd `lab.rs`; STORES.md §3.6 planned it as
`LaunchMaterial`) mints one launch's licence from quiverd's own session. The instance never
sees a refresh token, a password or a store login.

- **Ubisoft** (`licence = "ubisoft-bridge"`): the bridge's launch context (ownership ticket,
  owned ids, CD keys, the broker's address), the same as `quiver context` makes, with the lab
  session id set. The supervisor writes it 0600 into the run folder, installs the current
  bridge in the instance's prefix (`C:\quiver\quiver-bridge.exe`) and starts it in launcher
  mode (`--run context -- game.exe`), which owns the SDK pipes, starts the game with
  `UPLAY_ARGUMENTS`, and deletes the context once read. Denuvo goes through quiverd's broker
  (`127.0.0.1:47810`, forwarded). One login for everything, held by quiverd. A bridge built
  before lab sessions (no `quiver_ipc::LAB_PROTOCOL_MARKER` in it) is refused: it wouldn't tell
  the broker the session is a lab one.
- **Epic** (`licence = "epic-exchange-code"`): EGL's arguments with a fresh single-use
  exchange code from quiverd (`epic_core::Epic::launch_material`), and the ownership token when
  the game wants one, written 0600 into the run folder (`-epicovt=Z:\lab\runs\rN\<ns><item>.ovt`)
  and deleted at the next kill. The refresh token stays in quiverd's keyring. EOS then talks to
  Epic itself, and the container sends `api.epicgames.dev` to the instance's **EOS proxy**
  (3.1): sign-in, the SDK's config and ownership reach Epic; EOS cloud saves (Player Data
  Storage), achievements, stats, leaderboards, presence and telemetry get a 403 from the proxy
  and never reach Epic. If the game doesn't trust the lab's CA, EOS is offline for the run
  (fail closed). A game with the EOS SDK doesn't launch unless the proxy answers from inside
  its container.
- **DRM-free** (`licence = "none"`): nothing to mint. Today that is Steam games without
  Steamworks or SteamStub (below). GOG has no provider in Quiver yet (not built).
- **Steam**: see section 7.

### 3.1 The EOS proxy

A game built on the Epic Online Services SDK talks to Epic itself, with the person's real
licence: it syncs cloud saves, unlocks achievements, sends stats, leaderboard scores and
telemetry. A lab run with test or modded saves could overwrite the person's real cloud saves
and earn achievements. Blocking the host isn't possible: AWR's `EOSSDK-Win64-Shipping.dll`
(EOS SDK 1.7.1) names one host, `api.epicgames.dev`, for everything, sign-in included; the
services differ only by URL path. So the lab filters by path.

**Why a proxy (the first live run).** The first filter was a `secur32.dll` that read each
request's plaintext in schannel's `EncryptMessage`. In AWR it loaded but saw nothing while the
game held five TLS connections to Epic (Cloudflare, 104.18.124.108 and 104.18.125.108): the
SDK statically links **OpenSSL 1.1.1c** under libcurl 7.55.1 ("AES-256 GCM using openssl",
`ossl_static.pdb`, libwebsockets' `tls/openssl/ssl.c`), and imports from Secur32 only
`AcquireCredentialsHandleA`, `InitializeSecurityContextA`, `CompleteAuthToken`,
`DeleteSecurityContext` and `FreeCredentialsHandle` (proxy and NTLM auth). It was removed.
What does work: the SDK carries Unreal's `FSslCertificateManager`, which loads the Windows ROOT
store into OpenSSL (`bUsePlatformProvidedCertificates`, "Unable to open ROOT certificate store",
`CertOpenSystemStoreW`/`CertEnumCertificatesInStore`) next to its bundled roots; pinning is
configuration (`PinnedPublicKeys`, "no pinned key digests found for domain").

**How.** Every instance's container, from `quiver lab up` on:

- resolves `api.epicgames.dev`, and Epic's other names in the SDK (`api-gamedev.ol.epicgames.net`,
  `geo-nat.ol.epicgames.com`, `stun-1-nat.ol.epicgames.com`, `stun-2-nat.ol.epicgames.com`),
  to `127.0.0.1` (`podman --add-host`);
- forwards its loopback port 443 to the instance's **EOS proxy** on the host's loopback
  (`--network=pasta:…,-T,443:<port>`; the port is in `<instance>/eos-proxy-port`). Verified
  rootless: pasta binds the namespace's port 443 itself, no sysctl needed, and two `-T`
  forwards (the broker's and this one) work in one option.

The proxy (`quiver_core::lab::eosproxy`) runs in the instance's supervisor, so it lives exactly
as long as the instance is up, and needs nothing in the image:

- **TLS** for `api.epicgames.dev` only, with a fresh certificate from the instance's own CA.
  The CA (`<instance>/eos-proxy/ca.pem`, key `ca.key` 0600, outside the mounted `lab/` folder so
  nothing in the container can read the key) may sign only `api.epicgames.dev` (a name
  constraint). Any other name (the sinkholed ones) is refused at the handshake and logged.
  ALPN offers `http/1.1` only (an `h2` offer gets http/1.1; an h2-only client gets no
  connection).
- **HTTP/1.1** (hyper: keep-alive, pipelining, chunked bodies). Each request is checked against
  the allowlist. An allowed request is forwarded to the real `api.epicgames.dev` over one
  upstream connection per game connection, verified against the web PKI. The host is resolved
  on the host and never to a local address, so it can't loop back to itself; hop-by-hop
  headers are dropped. Everything else, protocol upgrades (websockets) and unknown paths
  included, is answered `403` with an Epic-shaped error body
  (`errors.com.quiver.lab.eos_blocked`) and never leaves the proxy.
- **Trust** lives in the instance's *cloned* prefix only (`quiver_core::lab::wincert`, at each
  launch, with no wineserver running). The CA is written to `system.reg` as
  `HKLM\Software\Microsoft\SystemCertificates\Root\Certificates\<SHA-1>`, value `Blob` (Wine's
  serialized properties: `CERT_HASH_PROP_ID` then `CERT_CERT_PROP_ID`). That is where Wine's
  ROOT store lives and what `CertOpenSystemStoreW(0, "ROOT")` returns.
  - **The root sync.** Wine's crypt32 deletes, on its first use per session, the roots listed
    under `HKLM\Software\Wine\HostImportedCertificates` that the host no longer has. When that
    key is missing, it first lists every cached root there, ours included
    (`dlls/crypt32/rootstore.c`). 32-bit processes see `Software\Wow6432Node\Wine\…`.
  - **Seen in a Proton 11 prefix:** a 32-bit client deleted a CA the 64-bit one had kept, until
    the Wow6432Node key existed. So both import keys are created if missing, without our hash.
  - **Key usage.** Wine's chain engine reads only the *last* byte of the KeyUsage bit string
    (`CRYPT_KeyUsageValid`), and rcgen writes two bytes. So the CA's KeyUsage is written as a
    one-byte bit string, or Wine rejects the chain with `CERT_E_WRONG_USAGE`.
  - Older lab CAs (from a CA made again) are removed.

**Fail closed.** If the game doesn't trust the lab CA (bundled roots only, or a pinned key),
its TLS to `api.epicgames.dev` fails and EOS is offline for the run. That is acceptable, and
the log says so (`tls failed … the game rejected the lab's certificate (alert UnknownCA) …
fail closed`, or `closed … after the handshake without a request` for a pin checked after the
handshake). Unknown paths always stay blocked.

**The allowlist** (`ALLOWLIST` in `crates/quiver-core/src/lab/eos.rs`, the one place to change
it; each entry has a comment and its source). Everything else on `api.epicgames.dev` gets a 403.
`{}` is one path segment, `{v}` a version (`v1`…`v99`); the query string is ignored; paths
with `%`, `\`, empty, `.` or `..` segments never match.

| Method and path | What | Source |
|---|---|---|
| `GET /sdk/v1/default`, `GET /sdk/v1/product/{}` | the SDK's config (service URLs, flags); without it the SDK never starts | SDK strings (`[SDKConfigService] BaseUrl=https://api.epicgames.dev/sdk`, `/v1/default`, `/v1/product/{productId}`) |
| `POST /epic/oauth/{v}/token` | Epic account sign-in: the launch's exchange code, then its refresh token | SDK (`/epic/oauth/v1/token`); Auth web API (`/epic/oauth/v2/token`) |
| `POST`/`GET /epic/oauth/{v}/tokenInfo` | checks that token | SDK; Auth web API |
| `POST /epic/oauth/{v}/revoke` | the SDK ending its own token at logout (only this launch's session, never quiverd's) | SDK; Auth web API |
| `GET /epic/oauth/{v}/.well-known/jwks.json` | Epic's public keys, to verify ID tokens | Auth web API |
| `GET /epic/id/{v}/accounts` | the signed-in account's display name, read only. AWR's SDK revokes its token and stays signed out when this is refused | seen live (lab run awr r4, 2026-11-19) |
| `POST /auth/v1/oauth/token` | EOS Connect sign-in and the SDK's client credentials | SDK; Connect web API |
| `GET /epic/ecom/{v}/platforms/{}/identities/{}/ownership` | does the account own the game | SDK (`/v1/platforms/{platform}/identities/{identityId}/ownership`); Ecom web API (v3) |
| `POST /epic/ecom/{v}/platforms/{}/identities/{}/ownershipToken` | a signed statement of that | SDK; Ecom web API (v3) |
| `GET /epic/ecom/{v}/identities/{}/entitlements` | the account's entitlements (game, DLC), read only | SDK; Ecom web API (v4) |
| `POST /epic/ecom/{v}/platforms/{}/identities/{}/entitlementToken` | a signed statement of those (newer SDKs) | Ecom web API (v4) |
| the three ecom reads under `/ecom/{v}/…` | the same resources without `/epic` | the EOS OpenAPI mirror on apis.io |

**Blocked on purpose.** Some paths are close to sign-in but write to the account:
`/auth/v1/users` (creates a product user), `/auth/v1/accounts*` (links accounts, makes device
ids), `…/entitlements/redeem` (marks entitlements used), offers and purchases. Others aren't
needed for sign-in: `/epic/oauth/{v}/deviceAuthorization` and other user info (`/user/…`).

**Blocked as reporting.** Player Data Storage and Title Storage,
`/v1/{deployment}/achievements/{puid}/unlock` and the achievement definitions, stats,
leaderboards, presence, friends, sessions, lobbies, and the SDK's telemetry (`[SDKAnalytics]
BaseUrl=https://api.epicgames.dev/telemetry/data/`, `datarouter/api/v1/public/data`).

**Sources.** The SDK's own route strings in AWR's `EOSSDK-Win64-Shipping.dll`, and Epic's web
API reference (dev.epicgames.com/docs/web-api-ref: authentication, connect-web-api,
ecom-web-apis). The proxy only removes reporting: it never answers for Epic on an allowed
path, fakes ownership, or touches what Epic returns.

**The log and the verdict.** Every connection and decision goes to `runs/r<N>/eos-filter.log`:

- `probe ok`: the launch's own probe, from inside the container through `api.epicgames.dev:443`;
- `tls ok` or `tls failed …` (with the reason), and `refused <name>`;
- `allow METHOD path -- rule -> status` and `block METHOD path -- reason (403)`. The path is
  shown with the query's keys only (values replaced by `…`); headers and bodies are never logged;
- `closed conn#N after K request(s)`.

`quiver lab logs <i>` gives the verdict: did the proxy see the game's TLS and requests, did the
game reject our certificate (EOS offline, fail closed), did TLS succeed with no request after
it (a pinned key). Then the counts and each blocked request. "Loaded" proves nothing: the probe
and the `tls ok` lines are the evidence.

**Bypasses.** During a run the supervisor reads the container's TCP table
(`/proc/<pid>/net/tcp{,6}` of an instance process) for `:443` connections that aren't to the
container's loopback.

- One to an address the real `api.epicgames.dev` has (as the proxy resolved it) is logged as
  `!!! BYPASS` and kills the run as a violation, the way a Connect process does.
- Any other is logged as `!!! OUTSIDE`, and added to the run's notes and events.

**At each launch** (`lab::eos`, the supervisor), for an Epic game or any game shipping
`EOSSDK-Win64/Win32-Shipping.dll`, after the kill-all:

1. The proxy's run log starts.
2. The CA is trusted in the clone's registry.
3. A probe from inside the container must reach the proxy through `api.epicgames.dev:443`.

If any of that fails and the game ships the EOS SDK, **the launch refuses** with the reason,
for example an instance made before the proxy: `quiver lab down <i> && quiver lab up <i>`.
There is no opt-out. The old `secur32.dll` filter is deleted from the clone if it's there.
`quiver lab doctor` checks that a CA can be made and that `api.epicgames.dev` resolves on the
host (DNS only); `quiver lab brief` explains all this to the agent.

**Tests** (`crates/quiver-core/tests/eos_proxy.rs`, a local stand-in upstream with its own CA;
nothing talks to Epic):

- allowed requests forwarded intact (chunked bodies too), blocked ones never reach the upstream;
- keep-alive, pipelining in one write (200, 403, 200 in order), ALPN, sinkholed names,
  websocket upgrades;
- a client that doesn't trust the CA fails closed, and the probe works;
- OpenSSL (`openssl s_client -verify_hostname`) accepts the chain;
- `eos::tests`, `wincert::tests`, `podman::tests::epic_names_go_to_the_eos_proxy`;
- opt-in, `QUIVER_TEST_PROTON=… cargo test -p quiver-core --test eos_proxy -- --ignored`: a
  Windows client (`tests/eos_proxy/client.c`, x86_64 and i686) finds the lab CA in
  `CertOpenSystemStoreW(0, "ROOT")` from the scratch prefix's registry, verifies the proxy's
  chain for `api.epicgames.dev` with `CertGetCertificateChain` and the SSL policy, and gets 200s
  and 403s. Wine's root sync keeps the CA in both architectures; without it the client gets
  `CERT_E_UNTRUSTEDROOT`.

**Not covered.**
- A game that trusts only its bundled roots or pins Epic's key: EOS is offline (fail closed).
- A game with hard-coded Epic addresses: a bypass, so the run is killed; if the address isn't
  one the host resolves, it is only flagged as OUTSIDE.
- Name lookups that skip `/etc/hosts`: the redirect is the container's `/etc/hosts`, which
  `getaddrinfo` reads (AWR's SDK imports Winsock's `getaddrinfo`; no c-ares in it). A lookup
  through `DnsQuery` (the SDK has the string) would go to DNS and show up as a bypass.
- UDP: STUN and P2P go to sinkholed names, and nothing answers.
- HTTP/2-only clients: no connection.

### 3.2 No sound device

The container has no sound device. AWR crashed at startup in r1 and r2 of the first live run,
an access violation (a null read at `Game_f_x64_EOS.exe+0x338489`), before any EOS traffic.
With `pcm.!default { type null }` in `$HOME/.asoundrc` it reached a frame in 4.1 s. The boot
script (`podman::boot_script`) now writes that file in every container (`HOME` is
`/tmp/home`), so ALSA's default device is a null sink.

## 4. Safety invariants

Each is enforced in code, not by convention, and has a test.

| Invariant | Enforced by |
|---|---|
| Connect never runs in an instance; no copied login or ticket | `lab::guard`: credentials (`ConnectSecureStorage.dat`, `user.vdat`) and launch material Quiver's own launches left in the prefix (`quiver-run/context.json`, `*.ovt`) deleted and executables renamed `*.quiver-lab-off` in the gold copy and every instance, re-checked before every launch (`guard::enforce` fails the launch if anything is left); Connect's executables disabled in every launch's `WINEDLLOVERRIDES`; a Connect process seen in an instance kills the run and is recorded under `violations` (`procs::connect_in`). |
| The user's real prefix and install are never mounted or linked | instances are reflink clones; `podman::run_args` mounts only the instance's folders (test `never_host_networking_and_mounts_the_clones`) |
| Instance processes are identified by container, never by name alone | `procs`: the libpod cgroup of each process, then `WINEPREFIX`; kill-all is `wineserver -k`, then SIGKILL of what is left in the container's cgroup. No `pkill -f` anywhere. |
| Kill everything before every launch | the supervisor's launch starts with kill-all (a stale wineserver or bridge hangs the next game before its SDK answers) |
| One live login per account | launch material comes from quiverd; no store client runs in an instance; no credential file is read |
| An EOS game reports nothing to Epic | every container resolves Epic's names to the instance's EOS proxy (3.1), which forwards only the allowlist and answers the rest 403; the CA is trusted only in the instance's cloned prefix; an EOS game's launch is refused unless a probe from inside the container reaches the proxy; a :443 connection to the real host's addresses outside the proxy kills the run (`lab::eos`, `lab::eosproxy`, `lab::wincert`; tests `tests/eos_proxy.rs`, `podman::tests::epic_names_go_to_the_eos_proxy`) |
| **Nothing is reported** (section 5) | quiverd, by session kind |
| Sight in an instance changes none of the person's settings | the lab `quiver-sight` never connects to quiverd; panel actions that would change settings are refused and logged (section 12; `quiver-sight/src/lab.rs`) |

## 5. Nothing is reported

Lab launches carry a **lab session id**, `lab-<instance>-r<run>` (`quiver_api::lab::is_lab_session`).
quiverd decides by that id, never by what a script remembers to pass:

| What | Normal launch | Lab session |
|---|---|---|
| Ubisoft play time (UbiServices `playsession` POST/PUT/DELETE) | the broker's `PlaySession` lease reports it | the bridge passes its context's session with `PlaySession`; the broker holds it as the `lab` holder; `PlaySessions` never starts, beats or ends a session only lab holders hold (`playsession::reportable`). If the user later joins the same game, their share is reported from then. |
| Quiver's play history (`games/<id>/sessions`, `on_finished`) | recorded | not recorded while only lab holders held it (`playsession::finished`) |
| Ubisoft achievements (`AchievementEarned`: local earned set, report journal, UbiServices) | recorded and reported | the bridge passes its session; the broker answers `Ok` and records nothing (`broker::lab_answer`) |
| Mod channel (Options, perf metrics, mod revisions) | served | refused for lab sessions (`broker::lab_answer`, and `ModRegistry::serve` again); the perf hub never files a lab session's metrics under the user's session (`PerfHub::with_session`). A lab mod is configured through its files (`quiver lab set`). |
| Ubisoft cloud saves | pulled before, pushed after | never: the bridge never uploads, Connect's `syncsavegames` is forced off, and quiverd's save sync is only around its own launches |
| Epic play time | queued after the session, reported | `Epic::queue_session` refuses a lab session id (never queued, held or sent); lab launches never go through quiverd's Epic session (`epicgame::begin/end`: no save sync, no achievement polling) |
| Epic cloud saves (EGS savesync) | around quiverd's launches | never: lab launches don't go through quiverd's Epic session |
| EOS cloud saves, achievements, stats, leaderboards, presence, telemetry (the game's own EOS SDK) | the game's | 403 from the EOS proxy (3.1), never sent to Epic: only sign-in, the SDK's config, Connect and ownership/entitlement reads reach `api.epicgames.dev`; if the game rejects the lab's CA, nothing reaches Epic at all (EOS offline). Confirmed live with AWR (2026-11-19, runs r4–r6): AWR trusts the lab CA; sign-in and the SDK config pass; telemetry, presence, TURN credentials and the notifications websocket get 403s. One `:443` connection to an AWS address (54.204.14.20) went around the proxy; it's not an `api.epicgames.dev` address and is still unidentified |
| Steam play time, achievements, cloud | Steam's | a lab never talks to Steam (section 7), so nothing reaches it |
| `quiver sessions` | the game, holders `launcher`/`bridge`/`steam` | shown with the holder `lab`, so lab runs are visibly not the user's |

Tests: `playsession::tests::lab_bridges_are_lab_holders` (no report for lab-only holders;
reported once the person joins), `…::sessions_without_a_launcher_are_handed_over` (no history),
`broker::lab_tests` (earns not recorded, mod channels refused, play sessions held as lab),
`perf::tests::lab_metrics_never_land_in_the_persons_session`,
`epic-core phase3_tests::lab_sessions_are_never_queued_or_sent`, and the bridge's
`lab_sessions_travel_with_play_sessions_and_earns`.

## 6. The supervisor and readiness

One supervisor per instance (`quiver lab supervise`, started by `up`, detached, a lock file
so there is never a second; talked to over `supervisor.sock`) owns launches and kills, so
agents never assemble pkill/sleep sequences:

- **launch**: kill-all; the Connect guard; the mod folder's files over
  the install (the previous launch's replaced files put back first); the graphics profile;
  a new run folder with `host.json`; launch material; `podman exec -d … proton run …` (never
  `waitforexitandrun`). The game's environment carries `QUIVER_LAB=Z:\lab`,
  `QUIVER_LAB_RUN=rN`, `QUIVER_LAB_RUN_DIR`, `QUIVER_SESSION`, `QUIVER_GAME`, DXVK and Proton
  log paths into the run folder, and the manifest's env.
- **watch** (every 500 ms): the readiness ladder **process** (the exe runs in the container),
  **window** (gamescope's `GAMESCOPE_FOCUSED_WINDOW` is set), **frame** (a screenshot isn't
  blank), then **exited** (with how: `exited`, `killed`, `violation`, `failed to start`,
  `container stopped`); the mod's state; Connect.
- `state.json` holds the current run, when each rung was reached, the mod's state, pids,
  exit and violations; `events.jsonl` has one line per change, tagged with the run id, so an
  agent never mistakes the previous run's lines for this one's.

`quiver lab wait N STATE` waits for `process|window|frame|exited` or **a state the mod
reports** (`menu`, `loading`, `in-world`…), and fails at once if it can't happen any more (the
game exited). `--stable` then waits until consecutive frames stop changing.

**The lab channel** (store-agnostic: works without the bridge, for Epic and DRM-free games):

- `%QUIVER_LAB%\state`: the mod writes one word (and `key=value` lines) and renames it into
  place. Only writes after the run started count.
- `%QUIVER_LAB%\req\<id>.json` → `%QUIVER_LAB%\rsp\<id>.json`: `quiver lab call N VERB ARGS…`
  (`{"id","verb","args"}` → `{"ok":true,"result":…}`). `where` and `teleport` are calls;
  `teleport LOCATION` reads `fixtures/<game>/locations.txt` (`name x y z [yaw [pitch]]`). This
  is the generic form of Pharos's `where`/`teleport` files: any mod can fill it.
- Live config: the mod re-reads its file; `quiver lab set … --wait` waits for the manifest's
  `reload_pattern` in its `log_file` instead of sleeping.

## 7. Steam

**Findings.** A Steamworks game (`steam_api*.dll`) calls into Proton's `lsteamclient`, which
loads the native `steamclient.so` and talks to the **running Steam client** over per-user IPC:
POSIX shared memory and semaphores in `/dev/shm` (`u<uid>-Shm_*`, `u<uid>-ValveIPCSharedObj-*`),
`~/.steam/steam.pid` and `steam.pipe`, and checks on the calling process. Making that work from
a container means sharing `/dev/shm`, `~/.steam`, the IPC and **PID namespaces** with the host
and mounting Steam's runtime: the container would no longer be isolated. And it would disturb
the user's Steam exactly as playing the game does: the user shows as "Playing X" to friends,
the session counts as play time, stats and achievements go to the account, **Steam Cloud
uploads the instance's saves at exit**, and Steam refuses a second launch of the same app.
SteamStub (`.bind` section) needs the client too. A second Steam login is not acceptable, and
emulators are out (STORES.md §7).

**So, built:** DRM-free Steam games only. `create` reads the install (`lab::facts`): no
`steam_api*.dll`, no import of it, no `.bind` section → `licence = "none"`; Steam doesn't say
which exe to run outside it, so `--exe` is required. The container has no `~/.steam` and a
private `/dev/shm`, so such a game can't reach the user's Steam even if it tries (a game that
calls `SteamAPI_RestartAppIfNecessary` just exits). **Refused:** Steamworks and SteamStub titles,
with this explanation (`quiver lab brief` says so too). Nothing a lab supports counts toward
Steam play time.

**Not built (would need the user):** a Steamworks lab would have to run inside the user's real
Steam session (e.g. `quiver steam-run` launching into a container), accepting play time,
presence, achievements and cloud uploads. That contradicts section 5, so it isn't offered.

## 8. Tools (generalised from the Pharos harness)

| Pharos tool | Lab |
|---|---|
| `aco shot` | `quiver lab shot` (gamescope's own screenshot of what the game presents; `--settle` for TAA/upscaler convergence; `--crop`, `--metrics`) |
| `aco keys` (xdotool) | `quiver lab key` (keydown, hold, keyup), `type` |
| `inject_mouse.py` | `quiver lab mouse DX DY` (XTEST relative motion via `xdotool mousemove_relative`); `--repeat --gap --log` runs the timed injector in the container, logging CLOCK_MONOTONIC_RAW send times (Wine's QPC base) for latency work |
| clicks at 2560×1600 pixels | `quiver lab click FX FY`: fractions of the manifest's display; `--expect-change` fails when the screen didn't change (clicks missed silently before) |
| `ini-set` | `quiver lab set FILE:[section]key=value` (INI and XML through `confpatch`, byte for byte except the value; Unreal section names with dots work; `{width}`/`{height}`/`{refresh}`), `--wait` for the reload line |
| `profiles/<p>.ini` | `quiver lab profile` (`profiles/<game>/<name>.set`, applied before every launch when it is the manifest's) |
| `fixture` / `fixture-save` | `quiver lab fixture save|install|list` (the manifest's save folders; read-only once saved; the instance's own saves kept once before the first install; refused while the game runs) |
| `where`, `teleport`, `locations.txt` | `quiver lab call|where|teleport` over the lab channel |
| `gpu-exclusive` | `quiver lab gpu [--for N] -- CMD` (a lock others queue on; other running instances throttled through their manifests' `throttle` lines and restored after; host state logged before and after to `gpu.log`) |
| `bench`, `abtest` | `quiver lab bench`: variants `LABEL=assign;assign`, `--rounds` interleaved, `--samples` shots over `--crop` (Laplacian sharpness, temporal noise, mean level), `--log-metric NAME=REGEX` numbers from the log written meanwhile, host state per variant, `--relaunch --ready STATE` for settings that need a restart, `--gpu`; originals restored; `bench.json` and a table |
| `userplaying.sh` | not built: sharing the machine between lab instances and the user's own games is the user's call (section 11) |
| `framediff.py`, `perframe.py`, `latency.py` | not built: they read Pharos's own captures. The image metrics (`lab::image`) cover frame differences over crops; a latency analysis needs a mod that logs camera changes per frame (Pharos does) |

Long runs go in the background: `quiver lab … --bg` prints a job id; `quiver lab job wait ID`
returns with the job's output and exit code.

## 9. First live run (for the main session)

Needs the merged build installed (`tools/install-user.sh`: the lab refuses a bridge without
lab sessions) and quiverd restarted onto it (it restarts itself when idle). Then:

```sh
quiver lab doctor                                    # all ok (bridge, image, CDI, reflinks, quiverd)
quiver lab image build                               # if the image is missing (~1.4 GB)
quiver lab create ubisoft:3539 --name origins --display 1920x1080@60   # gold copy + instance (reflinks)
quiver lab up origins
quiver lab launch origins --wait frame --timeout 300
quiver sessions                                      # Origins with holder "lab"; nothing reported
quiver lab shot origins --metrics
quiver lab kill origins && quiver lab down origins
```

Check afterwards: `quiver activity` shows "issued lab launch material … session lab-origins-r1";
quiverd's log has "bridge reports game running holder=lab" and no "play session started";
no new entry in Origins' play history. Then the same for Epic: `quiver lab create "Alan Wake
Remastered" --name awr && quiver lab up awr && quiver lab launch awr --wait frame`. The
launch notes say the EOS proxy's probe from inside the container worked and that the CA is
trusted in the clone. `quiver lab logs awr` then gives the verdict. If AWR trusts the lab CA,
it shows `tls ok` lines, sign-in (`/epic/oauth/v1/token`), the SDK config and the ownership
checks allowed, and what was blocked; if the game stops at sign-in, the blocked lines say
which path it needed (section 3.1 says how to extend the allowlist). If it doesn't trust the
CA, it shows `tls failed … rejected the lab's certificate`: EOS was offline, fail closed. No
`!!! BYPASS` or `!!! OUTSIDE` line may appear.

## 10. Not built yet

- GOG (no provider), EA, Meta: refused with a reason.
- Steamworks titles (section 7).
- A Quiver mod profile (Mods v2 stack) as an instance's mod: today a mod is a folder copied
  over the install; the profile route (overlays, config patches) is the next step.
- The mod SDK channel for lab sessions (refused on purpose; would need a lab-scoped Options
  store so a lab mod's options never reach the user's).
- The app: instances aren't shown in Quiver's app.
- Analysis tools that read a specific mod's captures (frame dumps, per-frame draw counts).

## 11. Open questions for the user

1. Lab sessions no longer report Ubisoft play time (the Pharos containers did, as plain bridge
   sessions). Built as the coordinator asked; Pharos's own `aco` harness is unchanged until it
   moves to `quiver lab`.
2. Should the user's current saves come along in the gold copy (they do: it is a clone of the
   prefix), or should gold copies start without saves and use fixtures only?
3. Decided: resource sharing between lab instances and the user's own games is the user's
   concern, not Quiver's. Quiver has no user-playing check: a launch never refuses, warns
   about or reports the user's games (`quiver lab host` lists GPU users as plain facts). The
   GPU lock (`quiver lab gpu`) only coordinates lab runs with each other.

## 12. Sight in an instance (2026-11-19)

An agent can test Sight (the HUD and the panel drawn in the game, design/SIGHT.md §14) in a
real game without the person. Off by default, so existing instances don't change:
`quiver lab create … --sight`, or `quiver lab edit N sight=on` then `quiver lab down N && quiver
lab up N` (the container's mounts change). Code: `quiver_core::lab::sight`,
`crates/quiver-cli/src/lab/sight.rs`, `quiver-sight --lab` (`crates/quiver-sight/src/lab.rs`).

```sh
quiver lab launch awr --wait frame
quiver lab key awr shift+F2           # the panel, drawn in the game's frames; the game keeps focus
quiver lab sight awr status           # drawn: in the game, capture on, the preload, the layer
quiver lab shot awr                   # the panel on the game's frame
quiver lab key awr Down Return        # drives the panel; the game gets none of it
quiver lab key awr Escape             # closes it; the game has its input again
quiver lab key awr shift+F3           # the HUD
quiver lab logs awr                   # presenter chosen, captures, releases and why
```

**What a launch gets** (`sight = true`):

- **The libraries**, mounted read-only at `up`: the installed Sight libraries
  (`<prefix>/lib/quiver/sight/{x86_64,i686}`, next to the `quiver` that runs `up`) at
  `/opt/quiver/sight`, `quiver-sight` at `/opt/quiver/bin/quiver-sight`, and
  `libxkbcommon-x11.so.0` from the host at `/opt/quiver/lib` (image `quiver-lab:1` lacks it;
  the Containerfile has it now for the next build). `QUIVER_LAB_SIGHT_DIR` and
  `QUIVER_LAB_SIGHT_BIN` point at other builds (the tests).
- **The environment**, the same as quiverd's launches (`quiver_core::sightenv`, shared with
  `quiverd::perf::launch`): `QUIVER_PERF_DIR` (the run's session dir), `QUIVER_SIGHT=1`,
  `VK_ADD_IMPLICIT_LAYER_PATH=/lab/sight/layer` (the layer's manifests, written at each launch,
  pointing at the mounted libraries), `VK_INSTANCE_LAYERS` in quiverd's order, `LD_PRELOAD`
  with `/opt/quiver/sight/$PLATFORM/libquiver_sight_input.so` after the manifest's own, and
  `QUIVER_SIGHT_LOG=1` (the layer's and preload's lines go to `game.log`).
  `env.QUIVER_SIGHT_INPUT=0` drops the preload.
- **A `quiver-sight` for the run, inside the container** (`podman exec -d`, started just
  before the game, killed with every Wine process at kill-all). Inside, it shares the game's
  pid namespace and `/dev/shm`, so the preload's "the panel's owner still exists" failsafe works
  as on the desktop. It renders off-screen with GPUI on the container's GPU (CDI).
- **The session dir** is in a per-instance tmpfs folder on the host,
  `/dev/shm/quiver-lab-<uid>/<instance>/quiver-<uid>/lab-<instance>-r<N>/`, bind-mounted at the
  same path. The supervisor makes it and `hud.image` at each launch (removing the last run's),
  and `quiver lab sight` reads `panel.input` and the layer's ring from the host. It goes at
  `down` and `rm`. The container keeps its own `/dev/shm`, pid and network namespaces.

**Nothing reaches the person's settings.** The lab `quiver-sight` (`--lab`) never connects to
quiverd; none is reachable from the container anyway. Chosen over a read-only view of
quiverd because then there's nothing to get wrong: no request reaches quiverd, so no
setting, profile, Tweak or report can change, and the session stays `lab-<instance>-r<N>`
(holder `lab`).

- The HUD model comes from the layer's ring: frame rate, frame time and 1% low from its
  present records, the graph the layer draws itself, and a `LAB r<N>` tag. It has no GPU or CPU
  sensors (those are quiverd's) and no mod metrics.
- The keys come from the layer's status block, as quiverd's ring thread reads them:
  Shift+F2 (panel), Shift+F3 (HUD), Ctrl+Shift+F3 (preset).
- The panel shows the instance's game and run. Mods, Tweaks and Achievements say they're
  read-only (mod settings go through `quiver lab set`).
- Every change the panel would send (an option, a Tweak, HUD settings, quitting the game) is
  refused, logged in `sight.log` and `sight.json`, and shown as refused in the panel.
  Showing or hiding the HUD only changes this run's `quiver-sight`.

**Logs.** `runs/r<N>/sight.log` (quiver-sight), `runs/r<N>/sight.json` (its status: the
panel's report, the HUD, frames per second, refused actions), and the `quiver-sight-input[…]`
and `quiver-sight-layer:` lines in `game.log`. The preload now says why it gave input back
(`input back to the game (panel N): the panel closed | failsafe: quiver-sight is gone |
failsafe: quiver-sight stopped renewing the panel's deadline | failsafe: the panel isn't in the
game's frames`). `quiver lab logs` summarises per panel across the prefix's processes: input
hooked (X11 or Wayland), the presenter chosen and why, the layer drawing it, captures,
releases by reason, failsafe releases, and refused actions. `quiver lab sight N status
[--json]` shows the live state: quiver-sight alive, the HUD, the panel (open, drawn where,
backdrop), capture, the preload's and layer's words, the cursor.

**Doctor and brief.** `quiver lab doctor` checks that the Sight libraries and `quiver-sight`
are installed next to `quiver` (and whether the i686 build and `libxkbcommon-x11` are there).
`quiver lab brief` has a "Sight in the instance" section with the commands above.

**Verified.** Unit tests:
- the mounts only for `sight = true` instances, and no host pid, ipc or network namespace;
- the environment (merged with the manifest's own `LD_PRELOAD`, layer path and
  `VK_INSTANCE_LAYERS`; the opt-out);
- the layer manifests (the same as the layer's own template);
- the `quiver-sight` command line;
- the log summary over interleaved lines from several processes;
- the lab HUD's frame statistics and models, the read-only panel data.

End to end, `crates/quiver-cli/tests/lab_sight.rs`. It runs the real `quiver lab up/launch/key/
shot/sight/logs` on a hand-made DRM-free instance of the Sight layer's D3D12 test program
(vkd3d-proton under GE-Proton11-12), in the real lab image with gamescope headless and the GPU:
- keys reach the program;
- Shift+F2 opens the panel drawn in the game (the screenshot's middle changes, mean difference
  ~61/255);
- keys don't reach the program while it's open; Escape closes it, and its Escape never reaches
  the program;
- Shift+F3 shows the HUD (top-left changes);
- with the panel open, killing the run's `quiver-sight` gives the input back (~1 s, counting
  the CLI's polling);
- `quiver lab logs` reports the presenter and the failsafe.

Not run: a real game in an instance (AWR, Origins), winewayland in an instance (gamescope
headless has no Wayland input), a 32-bit game.

**Limits.** A debug build of `quiver-sight` can't run in the container: rust-embed reads its
fonts from the source tree in debug builds, so the installed (release) one is what's mounted.
Test programs built as console programs get Wine's console window, which takes gamescope's
focus (games are GUI programs). The lab HUD has no sensors.

## 13. HDR in an instance (2026-11-19)

`quiver lab create GAME --hdr` (or `--display 1920x1080@60+hdr`), or `quiver lab edit N
display.hdr=on` and then down/up, makes an HDR instance. `display.hdr` turns `sight` on too,
because measuring reads the swapchain through the Sight layer.

```
quiver lab create awr --hdr                  # or: quiver lab edit awr display.hdr=on; down; up
quiver lab launch awr --wait frame           # then turn the game's own HDR setting on
quiver lab hdr awr [--json]                  # swapchain format and colour space, the metadata, gamescope's view
quiver lab shot awr --hdr [OUT.png]          # OUT.raw (exact), OUT.png (16-bit PQ), OUT.json (nits)
```

**The display.** gamescope's headless backend has no real output, so HDR is forced:
`--hdr-enabled --hdr-debug-force-support --hdr-debug-force-output` (gamescope 3.17 in
`quiver-lab:1`) make its one output HDR10 PQ. The boot script passes them through
`QUIVER_LAB_GAMESCOPE_ARGS`. gamescope then sets `GAMESCOPE_HDR_OUTPUT_FEEDBACK=1` on the
root window, though `GAMESCOPE_DISPLAY_SUPPORTS_HDR` stays 0 headless.

**The game's environment.** `ENABLE_GAMESCOPE_WSI=1` and `PROTON_ENABLE_HDR=1` go before the
manifest's env, so the manifest's env can override them. `podman exec` doesn't inherit
gamescope's environment, so the exec prologue adds `GAMESCOPE_WAYLAND_DISPLAY` (written by the
boot script to `/tmp/gamescope-wayland`) when the WSI layer is enabled. Measured in throwaway
containers with the D3D12 test program:
- **Without gamescope's WSI layer** the surface offers only `SRGB_NONLINEAR`, and
  vkd3d-proton makes a `B8G8R8A8` SDR swapchain.
- **With it** you get HDR10 (`A2B10G10R10` + `HDR10_ST2084`) and scRGB
  (`R16G16B16A16_SFLOAT` + `EXTENDED_SRGB_LINEAR`), and gamescope sets
  `GAMESCOPE_COLOR_APP_WANTS_HDR_FEEDBACK=1`.
- **`DXVK_HDR`** wasn't needed with GE-Proton11-12: its DXGI detects HDR from gamescope's
  feedback. `PROTON_ENABLE_HDR=1` (which sets `DXVK_HDR=1`) is there for builds that still
  gate on it.
- **winewayland** (`PROTON_ENABLE_WAYLAND=1`, `WAYLAND_DISPLAY=gamescope-0`) made no swapchain
  in 45 s in the container: X11 plus the WSI layer is the HDR path.

**Layer order.** gamescope's WSI layer turns the game's HDR swapchain into an sRGB one for the
driver, and keeps `vkSetHdrMetadataEXT` to itself. A layer below it sees an SDR `MAILBOX`
swapchain and no metadata. It's an implicit layer, and the loader puts implicit layers above
every `VK_INSTANCE_LAYERS` layer unless they're named there too. So when the launch has
`ENABLE_GAMESCOPE_WSI=1`, `sightenv::layer_order` names it below Sight
(`VK_LAYER_QUIVER_sight:VK_LAYER_FROG_gamescope_wsi_x86_64:…_i686`). This applies to quiverd's
launches too.

**`quiver lab hdr`.** From the layer's status block, which gained appended fields (`hdr_*`,
`capture_done`; older layers read as zero):
- the target swapchain's format and colour space;
- the last `VkHdrMetadataEXT` the game set: max CLL and FALL, mastering max and min, primaries
  and white point, and the number of calls;
- whether a capture is possible.

From gamescope's root properties:
- whether its output is HDR;
- whether it sees the game as HDR;
- the metadata's EOTF. Only the first byte of `GAMESCOPE_COLOR_APP_HDR_METADATA_FEEDBACK` is
  read: gamescope writes a byte array as 32-bit items through Xlib's `long`s, so the rest is
  unreliable.

**`quiver lab shot --hdr`.** In lab launches only (`QUIVER_SIGHT_CAPTURE=1`), the Sight layer
adds `TRANSFER_SRC` usage to swapchains when the surface allows it. A swapchain the HUD can't
draw on still gets a compositor for captures. The capture works like this:
1. The CLI writes `n` into the session dir's `capture.req`.
2. The layer's heartbeat sees it.
3. The next present on the target swapchain copies the presented image, before the
   blur/HUD/panel, into a host buffer in the compositor's submit.
4. A later present takes the bytes once that submit's fence is done.
5. The heartbeat writes `capture-<n>.raw` and `.json` into the session dir, then
   `capture_done = n` in the status block.

The CLI then measures the capture, writes its files and removes the session-dir copies, since
tmpfs isn't the place for full frames. It writes three files:
- **`OUT.raw`:** the swapchain's bytes, exact.
- **`OUT.png`:** 16-bit RGB. For HDR10 these are the PQ code values themselves (lossless). For
  scRGB they're converted to BT.2020 PQ (derived).
- **`OUT.json`:** the format, colour space, the game's metadata, and `stats`:
  - max, average and p99 nits, and the brightest channel;
  - the fraction of pixels above 203 nits (SDR reference white) and above 1000 nits.

Luminance is computed per format:

| Format | Luminance |
|---|---|
| PQ | BT.2020 weights |
| scRGB | ×80 nits, BT.709 weights |
| 8-bit sRGB | 1.0 = 203 nits |

**gamescope's own screenshot** is an 8-bit PNG even in HDR: 203 nits came out at 169/255, and
1000 nits clips to 255. A `.avif` path (`gamescopectl screenshot x.avif`) writes nothing
headless. So plain `quiver lab shot` is for layout, and `--hdr` is for highlights.

**Sight on an HDR swapchain** (recorded by the e2e test):
- **Panel:** the layer can't draw the panel or the HUD on HDR yet (`DRAW_ERR_FORMAT`). On the
  desktop the panel falls back to its layer-shell window. In an instance there's no
  layer-shell, so Shift+F2 doesn't open it: the report says "can't draw it in the game (the
  Sight layer can't draw on this swapchain …) and no layer-shell here". The game keeps its
  input: the preload never captures for a panel the layer isn't drawing.
- **HUD:** Shift+F3 sets the lab HUD on (`hud_on`), but nothing is drawn.

**Monitors.** gamescope's headless backend has exactly one output (`--prefer-output` and
`--virtual-connector-strategy` are for the DRM backend and Steam's virtual connectors). A
second monitor would need a second gamescope, and a game sees only the one it was started on.
Not built: one output per instance.

**Verified.** Unit tests:
- PQ both ways, and the test program's code values;
- half floats;
- HDR10, scRGB and SDR stats; the lossless 16-bit PNG;
- gamescope's properties;
- `+hdr` display parsing and edits (on turns Sight on; a new size keeps HDR);
- the gamescope args in `run_args`, and the exec prologue;
- the WSI layer below Sight;
- the layer's sidecar JSON and status fields, the status block sizes in Rust and C, and an
  older layer's shorter block.

End to end, `crates/quiver-cli/tests/lab_hdr.rs` runs a hand-made DRM-free instance with
`display.hdr` on, in the real image with gamescope headless and the GPU. The program draws 203,
1000 and 4000 nits and sets max CLL 4000, max FALL 600, mastering 0.005–1000 and BT.2020
primaries:
- **HDR10** (`--arg 32`): `quiver lab hdr` shows `A2B10G10R10` HDR10 and the metadata exactly,
  and gamescope reports output HDR and the game as HDR (EOTF PQ). `shot --hdr` measures max
  3988 nits, avg 789, 52.8% above 203 and 5.6% above 1000 (the 4000-nit band).
- **scRGB** (`--arg 64`): `R16G16B16A16_SFLOAT` scRGB, max 4000 nits, avg 790.
- **Panel and HUD:** as above.

Not run: a real game in HDR (AWR, Origins), D3D11 through DXVK in HDR, a 32-bit game.
