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
quiver lab create <game> [--name N] [--display 2560x1600@165] [--proton P] [--mod DIR] [--graphics G]
quiver lab up N | down N | rm N | list | show N | edit N key=value…
quiver lab launch N [--mod DIR] [--clock virtual:HZ|real] [--wait frame]     quiver lab kill N
quiver lab status N | wait N STATE [--stable]       quiver lab logs N [--tail 40]
quiver lab shot N [--settle S] [--crop C] [--metrics]
quiver lab key N KEY… | type N TEXT | mouse N DX DY | click N FX FY [--expect-change]
quiver lab set N FILE:[section]key=value… [--wait] | profile N [NAME]
quiver lab fixture save|install|list … | call N VERB … | where N | teleport N LOC
quiver lab gpu [--for N] -- CMD… | bench N --variant A=… --variant B=… … | host
quiver lab brief <game> | doctor | image build|status | job list|show|wait
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

### 8.1 The virtual clock (fixed step: faster, and repeatable)

```sh
quiver lab edit N clock=virtual:62.5        # every launch; clock=real turns it off
quiver lab launch N --clock virtual:62.5    # one launch (--clock real: one launch without)
quiver lab edit N clock.modules=Engine.dll clock.stuck_ms=2000 clock.proxy=winmm clock.uncap=false
```

With the clock on, the game is told that **every frame took exactly 1/N s**, however long it
really took. Two benefits follow:

- **Faster than real time** when frames are cheap. Presentation is uncapped, so a small or
  old game that renders 800 fps at a 62.5 Hz virtual rate runs its simulation ~13x as fast
  (the Wine test below measured 13-17x with a trivial D3D11 presenter). Agents get through
  repros, menus and waits in a fraction of the time.
- **Repeatable** when the machine is busy. If real frames take longer than 1/N s, the game
  runs slower than real time but behaves exactly as at N Hz: frame k is at k/N s in every
  run. A speed-up below 1.0x is normal and isn't flagged. The test runs the same program
  with a fast presenter, again, and held to 40 fps: the three virtual-time sequences are
  identical, busy-waits included.

Off by default. It changes time only, never licence, ownership or DRM code.

**Credit.** The approach is Ishimura's: the virtual clock of the user's Dead Space project
(`~/code/ds-decomp`, its RIG shim; `docs/17-virtual-clock.md` and `docs/22-frame-delta.md`
there), built so that its physics A/B runs stopped measuring the machine. Its lessons,
corrections included:

- hook the **game's import address tables**, not the kernel32/winmm exports, so Wine's,
  DXVK's and the hook's own clocks stay real (DXVK paces presentation with them);
- advance by exactly one frame per Present, with a reported QPF that makes a frame a whole
  number of ticks;
- design for the deadlock: creep, a real-time escape, never backwards, and a Sleep never
  advances time, each counted;
- speed-up = real fps / virtual rate is an identity, a consistency check rather than evidence
  that the simulation advanced, so the report also carries a **census** of the steps the game
  actually read.

**How** (`crates/quiver-vclock`; the lab side is `quiver_core::lab::vclock`):

- **The DLL** (`win/hook.c`; the arithmetic in `win/vclock.c`, which the crate's unit tests
  run on the host) is built with mingw for x86_64 and i686 by `build.rs` and embedded in
  `quiver`, once per proxy name. Each export of the proxied DLL is a `jmp` through a table
  filled from the real DLL in the system folder, so with no `QUIVER_VCLOCK` it is a plain
  pass-through.
- **Injection: a proxy DLL in the clone.** At each launch the supervisor removes any copy an
  earlier launch left (every build carries a marker), then picks the first of `version`,
  `winmm`, `dinput8` and `dsound` that the executable imports, or a DLL next to it does (a
  Unity `UnityPlayer.dll`), and that neither the game nor the mod ships. It writes the
  hook under that name next to the executable in the **instance's install clone** (refused if
  the folder resolves outside the instance) and sets it `n,b` for that launch only.
  `clock.proxy=` forces one.
- **What becomes virtual.** QueryPerformanceCounter/Frequency, GetTickCount(64), timeGetTime,
  GetSystemTimeAsFileTime (and Precise), Sleep and SleepEx, from kernel32, kernelbase, the
  `api-ms-win-core-*` sets and winmm, matched by name in the import tables (and delay-load
  tables) of the executable and of the modules in `clock.modules` (`UnityPlayer.dll` is added
  by itself when it's there); GetProcAddress in the same modules answers those names with the
  hooks too. Nothing else is patched: the Wine test checks that the executable is the only
  module whose clock imports point into the DLL.
- **The frame boundary is the game's own Present, hooked in process.** The DLL hooks the
  creation functions in the imports of the game's modules and of the other modules in its
  folder (minus graphics layers such as an app-local DXVK or a mod's `dxgi.dll`): Direct3D 8
  and 9 (`CreateDevice`, then the device's and swap chain's `Present`/`PresentEx`), DXGI
  factories and D3D10/11 device-and-swap-chain (then `CreateSwapChain*`, then the swap chain's
  `Present`/`Present1`; a device made without a factory has its adapter's factory hooked),
  OpenGL `SwapBuffers`/`wglSwapBuffers`, and Vulkan `vkQueuePresentKHR` through its
  proc-address lookups. Vtable slots are patched per class, with a registry of originals.
  Nested presents on one thread count once; when two paths present, a D3D Present outranks
  OpenGL, which outranks Vulkan.
  **Why not the Sight layer's present count:** under DXVK and vkd3d-proton,
  `vkQueuePresentKHR` runs on the translation layer's submission thread, a frame or more after
  the game's Present returned. A clock driven by it would hand the game 0 or 2 frames on some
  frames, which is exactly the jitter this exists to remove. The in-process hook advances the
  clock on the game's thread when its Present returns. It also needs no layer in the instance
  and covers OpenGL.
- **Uncapped presentation** (unless `clock.uncap=false`): `DXVK_CONFIG` gets
  `dxgi.syncInterval = 0; d3d9.presentInterval = 0` and no frame-rate caps (after the
  manifest's own `DXVK_CONFIG`); `DXVK_FRAME_RATE=0`, `VKD3D_FRAME_RATE=0`,
  `__GL_SYNC_TO_VBLANK=0`, `vblank_mode=0`. The DXGI Present hook passes sync interval 0 (for
  vkd3d-proton's swap chains too) and D3D9 devices are created with
  `D3DPRESENT_INTERVAL_IMMEDIATE`. The container's gamescope boots at 1000 Hz
  (`lab::vclock::boot_refresh`; a launch says so when the instance was booted lower). A
  mod's own limiter, such as Pharos's `fps_limit`, is the user's to turn off.

**The clock.** At 10 MHz (Wine's QPC), `ticks_per_frame = round(QPF / N)` and the reported
QPF is `ticks_per_frame * N`: 62.5 Hz is 160000 ticks at 10000000, and 60 Hz is 166667 ticks
at 10000020, a few ppm off. Pick a rate whose period is a whole number of milliseconds (125,
100, 62.5, 50, 40 Hz): games that time frames with timeGetTime or GetTickCount then see the
same step every frame; a launch notes it otherwise. Until the first Present the clock passes
real time through, so start-up is unchanged. The first Present moves it on to the next whole
virtual second, so every run has the same phase from then on (a millisecond clock floors, and
start-up takes a different real time each run).

**The safety valves**, each counted in the report:

- **Creep.** After `clock.creep_after` (4096) clock reads in one frame, the frame can only be
  spinning on the clock: each further read moves time on by 1/1024 of a frame, so a frame
  limiter that waits for more time than a Present can give terminates. The test's QPC
  busy-wait for two frames and its `Sleep(1)` loop on timeGetTime both end this way. Creep
  depends on how often the game reads, not on the machine, so it is still repeatable, but
  those frames aren't exactly 1/N.
- **The real-time escape.** If a frame takes `clock.stuck_ms` (1000) of **real** time, time
  follows real time from there until the next Present. This catches waits that read the clock
  rarely, and loading screens that never present. **When it fires, that frame's timing tracked
  the machine, not the frame count, and isn't reproducible.** The report counts escapes, and
  `quiver lab logs` marks the run `NOT FULLY DETERMINISTIC`. Ishimura used 250 ms; the lab
  uses 1000 ms so that a loaded machine's slow frames stay on the virtual clock. Raise it with
  `clock.stuck_ms` if loading screens trip it.
- **Never backwards.** A Present sets time to max(grid, now); a frame where creep or the
  escape had already passed the grid is an **overrun**.
- **Sleep** never advances virtual time; it only gets shorter in real time (at most
  `clock.sleep_max_ms`, default 0: a yield), so a sleep-based limiter spins on the clock and
  creeps.

**The report.** The DLL writes `runs/r<N>/vclock.log` (each patched import slot and vtable
hook, the present path, engagement, a line every 10 s) and `vclock.status` (key=value,
rewritten every half second and at exit). `quiver lab status` shows engaged or not, frames,
virtual and real seconds, the speed-up, and the counters; `--json` adds the parsed report as
`clock`. `quiver lab logs` adds the **census**, the distinct steps between successive values
the game read from the performance counter and from the millisecond clocks. "160000 ticks
x1199 (100%)" means exactly 1/62.5 s every frame; creep shows as small steps, an escape as odd
ones. A long gap where the game didn't read a clock shows as one large step. The census is
followed by whether every step was a frame, and the determinism verdict.

**Limits** (in the brief too):

- **Audio drifts.** The mixer runs on a clock this can't reach (Ishimura: "Audio drifts"), so
  sound desyncs or stutters away from 1.0x. This is a testing tool.
- **Other clocks keep real time.** Games that pace on audio, waitable timers, `timeSetEvent`,
  `WaitForSingleObject` timeouts, `GetSystemTime`/`GetLocalTime`, `rdtsc`, or that read the
  clock from a module not listed in `clock.modules`, see real time there.
- **Multiplayer and DRM timing checks** see a clock that disagrees with the world. Denuvo and
  other DRM may react badly to time hooks: use it on DRM-free, offline, small games first.
- **Two present paths, or two swap chains presenting per frame,** can still count a frame twice
  (the latching handles a translation layer's own present; a second window doesn't).
- **The engine's own accumulator.** Ishimura found Dead Space's frame timer quantised the
  exact delta it was given into whole milliseconds, on another thread than Present. A game
  that does that still varies inside; the census shows what it was handed, not what it made of
  it.
- **Not yet:** DirectDraw-only games (no present hook), a present source for games that
  present from a process the proxy isn't loaded in.

**Tests:**

- `cargo test -p quiver-vclock`: the clock's arithmetic in the host build of `vclock.c`. Whole
  ticks per frame and the reported QPF, exactly one period per frame whatever the real frame
  took, a busy-wait that terminates by creeping (with the overrun after it), the escape (once
  per frame, then back on the grid), never backwards under random interleavings, sleeps,
  FILETIME and ms scaling, the report's parsing and verdicts, and the embedded DLLs.
- `cargo test -p quiver-core vclock`: `clock=` edits and the manifest, proxy choice (an
  import, a DLL beside the exe, a shipped DLL skipped, a mod's refused when forced, no way in),
  the DLL placed in the clone and removed at the next launch, never outside the instance, the
  environment (DXVK_CONFIG merged), the report lines and the brief.
- Opt-in, in Wine: `QUIVER_TEST_PROTON=… cargo test -p quiver-vclock --test wine -- --ignored
  --nocapture --test-threads 1`. `tests/wine/clock_test.c` presents through DXVK on a private
  Xvfb (display ≥ :400) with the hook as `version.dll`, for D3D11 x64 fast, again, and held to
  40 fps, then D3D9 x64 and D3D11 x86. Measured 2026-11-18 on GE-Proton11-12:
  - every one of 1198 steps on QPC is 160000 ticks, and on timeGetTime, GetTickCount and
    GetTickCount64 every step is 16 ms;
  - 19.168 s of game time took 1.1-1.4 s of real time with a fast presenter (13-17x) and
    30.1 s with the 40 fps one (0.64x);
  - the three sequence hashes are identical, as are the DLL's census, creep and sleep counts;
  - the busy-wait and the Sleep loop both terminated (60 creeping frames, no escapes);
  - `GetProcAddress("QueryPerformanceCounter")` gives the virtual value;
  - only the executable's 7 clock imports point into the DLL; none of the other 28 modules'
    (Wine's builtins, DXVK's d3d11, dxgi and d3d9) do.

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

**The virtual clock (8.1)**, on a small DRM-free game `G` (instance `g`), not run live yet:

```sh
quiver lab doctor                                  # "virtual clock DLL: x64 and x86, built in"
quiver lab edit g clock=virtual:62.5
quiver lab down g && quiver lab up g               # gamescope at 1000 Hz (tail lab/boot.log: "@1000")
quiver lab launch g --wait frame                   # notes: "virtual clock: 62.5 Hz … through <proxy>.dll"
quiver lab status g                                # "clock: virtual clock engaged … = N.NNx"
quiver lab logs g                                  # census, exact?, deterministic?
quiver lab launch g --clock real --wait frame      # the same game on real time: the DLL is gone
quiver lab edit g clock=real && quiver lab down g && quiver lab up g
```

What to check: the clock engaged (a present path was hooked), the speed-up is above 1 for a
cheap game, and the performance-counter census is dominated by 160000-tick steps. If it never
engages, `vclock.log` lists what was hooked. Check also that gamescope doesn't pace at 1000 Hz
(the real fps in the report is frames / real seconds), and whether the game's audio or menus
misbehave.

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
