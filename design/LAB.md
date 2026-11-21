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
quiver lab up N [--owner NAME] | down N | rm N | list | show N | edit N key=value… | whoami   (owners: section 18)
quiver lab launch N [--mod DIR] [--clock virtual:HZ|real] [--wait frame]     quiver lab kill N
quiver lab status N | wait N STATE [--stable]       quiver lab logs N [--tail 40]
quiver lab shot N [--settle S] [--crop C] [--metrics]
quiver lab key N KEY… | type N TEXT | mouse N DX DY | click N FX FY [--expect-change]
quiver lab set N FILE:[section]key=value… [--wait] | profile N [NAME]
quiver lab fixture save|import|install|list … | call N VERB … [--timeout S] | where N [--timeout S]
quiver lab teleport N LOC|X Y Z [YAW [PITCH]] [--no-turn] | turn N YAW [PITCH] [--tolerance DEG] [--moves K]
quiver lab gpu [--for N] -- CMD… | bench N --variant A=… --variant B=… … | host
quiver lab brief <game> | doctor | image build|status | job list|show|wait
quiver lab sight N status [--json]                  (instances with sight = on, section 12)
quiver lab hdr N [--json] | shot N --hdr [OUT]      (instances with display.hdr = on, section 13)
quiver lab limits [show|set PROFILE KEY=VALUE…|reset] (the person's limits on instances, section 15)
quiver lab pad N press|hold|release|stick|trigger|neutral|seq|status [--pad K]   (instances with pads = N, section 16)
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
File references (`log_file`, `files` aliases, `set`) are `install:<rel>`, `lab:<rel>`,
`run:<rel>` (the current run's folder `lab/runs/r<N>/`, where a mod writing to
`QUIVER_LAB_RUN_DIR` puts its log: `log_file=run:pharos.log`; resolved at each use, so after
a relaunch it names the new run's file, and `bench --relaunch` re-resolves it per launch), or
a path relative to the prefix.

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
- Any other is logged as `!!! OUTSIDE`, and added to the run's notes and events, **only in a
  run the proxy watches** (an Epic game or one shipping the EOS SDK, the runs the launch
  checks below apply to; `eos::around`). Every Ubisoft run used to get one for the game's own
  UbiServices HTTPS, which has nothing to do with EOS; other games' runs still get the BYPASS
  check (test `eos::tests::outside_notes_only_where_the_proxy_watches`).

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
| A lab pad is never visible to the host (Steam, the person's games, hyprpad, the desktop) | the udev rule `contrib/lab/72-quiver-lab-pad.rules` (no `uaccess`, no seat, libinput ignores it, `root:quiver-lab-pad 0660`, a gid only containers map); no pad without the rule; a canary device the rule must hide first; every node checked after udev (not openable by the user, no ACL) and re-checked every 5 s, the pads destroyed on any failure; a container gets only its own nodes, never `/dev/input` (section 15; `quiver_pad::rule`, test `podman::tests::pads_only_their_own_nodes`) |

## 5. Nothing is reported

Lab launches carry a **lab session id**, `lab-<instance>-r<run>` (`quiver_api::lab::is_lab_session`).
quiverd decides by that id, never by what a script remembers to pass:

| What | Normal launch | Lab session |
|---|---|---|
| Ubisoft play time (UbiServices `playsession` POST/PUT/DELETE) | the broker's `PlaySession` lease reports it | the bridge passes its context's session with `PlaySession`; the broker holds it as the `lab` holder; `PlaySessions` never starts, beats or ends a session only lab holders hold (`playsession::reportable`). If the user later joins the same game, their share is reported from then. |
| Quiver's play history (`games/<id>/sessions`, `on_finished`) | recorded | not recorded while only lab holders held it (`playsession::finished`) |
| Ubisoft achievements (`AchievementEarned`: local earned set, report journal, UbiServices) | recorded and reported | the bridge passes its session; the broker answers `Ok` and records nothing (`broker::lab_answer`) |
| Mod channel (Options, perf metrics, mod revisions) | served by quiverd | served **inside the instance**, read-only (section 14): its bridge relays to the supervisor's own endpoint, never quiverd's broker. quiverd still refuses lab sessions (`broker::lab_answer`, and `ModRegistry::serve` again); the perf hub never files a lab session's metrics under the user's session (`PerfHub::with_session`). A lab mod is configured through its files (`quiver lab set`). |
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
  `teleport LOCATION [MORE…]` reads `fixtures/<game>/locations.txt` (`name x y z [yaw
  [pitch]] [# comment]`; the comment is dropped, blank and `#` lines skipped, and any further
  arguments, such as Pharos's `hold=S`, follow the row's). This is the generic form of
  Pharos's `where`/`teleport` files: any mod can fill it.
- **Options after arguments.** `call`, `teleport` and `turn` take negative numbers as
  arguments, but anything else starting with `-` is an option, wherever it is:
  `quiver lab call N cfg stereo.depth -1.5 --timeout 60` is the verb `cfg` with two arguments
  and a 60 s timeout (it used to send `--timeout 60` to the mod). For an argument that starts
  with `-` and isn't a number, put it after `--`: `quiver lab call N VERB -- --raw`. `where`
  takes `--timeout S` (default 15; `call` 15, `teleport` 120). Single-number siblings
  (`mouse DX DY`, `pad stick X Y`) already took negatives and options after them.
- **Turning the camera** (`lab::teleport`). A mod can move the player from inside the game but
  often can't turn the camera: under Wine, `SendInput` from inside the game doesn't reach
  AC Origins' raw-input mouse-look, while XTEST motion from the host does. So a mod answers
  `teleport x y z yaw pitch` by moving the player and returning `"want":{"yaw":…,"pitch":…}`,
  and `quiver lab teleport` turns the camera there (unless `--no-turn`), as `quiver lab turn N
  YAW [PITCH]` does on its own: read the camera with `where` (`camera.yaw`/`camera.pitch` in
  degrees, else top-level `yaw`/`pitch`), move the mouse through the XTEST path of `quiver lab
  mouse` (in steps of at most 400 px, which games don't clamp), read again once two readings
  100 ms apart agree (camera smoothing), until both axes are within `--tolerance` (0.5°) or
  `--moves` (10) moves are spent. The gain (degrees per pixel, signed, per axis) is measured
  from the moves: an axis with none yet gets a 100 px probe (400, then 1600 if it didn't
  move), each move updates it (a secant), yaw takes the short way round, so sensitivity,
  inverted axes and the game's yaw convention don't matter. The learned gain is kept in
  `<instance>/camera-gain.json` as the next turn's first guess. A pitch past the game's limit
  stops after two moves that did nothing. The answer is the mod's JSON with `"camera":{"camera",
  "moves","converged","error","gain"}` added; not converging, a mouse that never moves the
  camera, a `where` without a camera, or a game held by `quiver lab clock` is an error (exit
  1). Pharos's `tools/lab/camera` did this with fixed gains (yaw -0.117 °/px, pitch -0.077
  °/px), which the unit tests' simulated camera uses, with smoothing, a pitch limit, a stale
  gain of the wrong sign, the other yaw convention and a dead mouse.
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
| `aco keys` (xdotool) | `quiver lab key` (keydown, hold, keyup; `--down`/`--up` leave keys down across `quiver lab clock` steps, §8.2), `type` |
| `inject_mouse.py` | `quiver lab mouse DX DY` (XTEST relative motion via `xdotool mousemove_relative`); `--repeat --gap --log` runs the timed injector in the container, logging CLOCK_MONOTONIC_RAW send times (Wine's QPC base) for latency work |
| clicks at 2560×1600 pixels | `quiver lab click FX FY`: fractions of the manifest's display; `--expect-change` fails when the screen didn't change (clicks missed silently before) |
| `ini-set` | `quiver lab set FILE:[section]key=value` (INI and XML through `confpatch`, byte for byte except the value; Unreal section names with dots work; `{width}`/`{height}`/`{refresh}`), `--wait` for the reload line |
| `profiles/<p>.ini` | `quiver lab profile` (`profiles/<game>/<name>.set`, applied before every launch when it is the manifest's) |
| `fixture` / `fixture-save` | `quiver lab fixture save|import|install|list` (the manifest's save folders; read-only once saved; the instance's own saves kept once before the first install; refused while the game runs). `import` (below) |
| `where`, `teleport`, `locations.txt`, `tools/lab/camera` | `quiver lab call|where|teleport|turn` over the lab channel; teleport turns the camera to the mod's `want` (section 6) |
| `gpu-exclusive` | `quiver lab gpu [--for N] -- CMD` (a lock others queue on; other running instances throttled through their manifests' `throttle` lines and restored after; host state logged before and after to `gpu.log`) |
| `bench`, `abtest` | `quiver lab bench`: variants `LABEL=assign;assign`, `--rounds` interleaved, `--samples` shots over `--crop` (Laplacian sharpness, temporal noise, mean level), `--log-metric NAME=REGEX` numbers from the log written meanwhile, host state per variant, `--relaunch --ready STATE` for settings that need a restart, `--gpu`; originals restored; `bench.json` and a table |
| `userplaying.sh` | not built as a guard: sharing the machine is the user's call (section 11). The user can set limits instead, enforced quietly on instances only (section 15) |
| `framediff.py`, `perframe.py`, `latency.py` | not built: they read Pharos's own captures. The image metrics (`lab::image`) cover frame differences over crops; a latency analysis needs a mod that logs camera changes per frame (Pharos does) |

Long runs go in the background: `quiver lab … --bg` prints a job id; `quiver lab job wait ID`
returns with the job's output and exit code.

**Importing fixtures** (2026-11-20). `quiver lab fixture import N NAME FROM… [--into FOLDER]
[--note TEXT]` makes a fixture from saves someone has, stored as `fixture save` stores them
(`fixtures/<game>/<NAME>/prefix/<save folder>/…`, a `README.md` with the note, every file
read-only), so `fixture install` and `fixture list` treat it like any other:

- `FROM` is save files (`{1..6}.save`), one folder of them, or one archive (`.zip`, `.tar`,
  `.tar.gz`/`.tgz`, `.tar.xz`, `.tar.zst`, `.tar.bz2`, `.7z`), unpacked with `bsdtar`
  (libarchive refuses absolute paths and `..` and won't write through symlinks; owners,
  permissions, xattrs and ACLs aren't kept);
- the files go into the instance's save folder: the manifest's one `saves` folder as found in
  the instance's prefix (`savegames/*/3539` expanded to the person's user id), or `--into`
  (relative to the prefix, no `..`); with several, `--into` is required;
- a folder or archive holding only one folder is unwrapped (an archive of `3539/`); one
  holding `prefix/` (a fixture copied from another lab) is taken as it is;
- only plain files and folders are imported: a symlink or special file anywhere inside is
  refused (the paths given are followed, nothing inside them is). It is built in a hidden
  folder next to the fixtures and renamed into place, so a failure leaves nothing behind;
  an existing name is refused.

Tests: `lab::fixture::tests` (six save files, then installed; a wrapped folder, a zip and a
tar.gz made by bsdtar; a fixture taken as it is; bad names, a missing file, `--into ../`, a
symlink in a folder and in a tar, two save folders without `--into`, a manifest folder not
made yet).

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
  proc-address lookups. Vtable slots are patched per class, with a registry of originals: one entry per (vtable,
  slot), the hook and what the slot held before it (`vc_vt_plan`/`vc_vt_orig` in `vclock.c`,
  so the host tests run them). **Other modules' hooks stay in the chain** (2026-11-20). A
  vtable is shared by every object of its class, so the hook is asked for again for each new
  factory, device or swap chain. When a registered slot holds something else by then, another
  module hooked it after us; it read the slot then, so the function it saved and calls is
  ours, and it is left in front (it runs, then ours, then the original). When the slot holds
  our original again (someone unhooked), ours goes back, same entry. Until then the DLL hooked
  over the other module with a second entry, but the lookup returned the first entry's
  original, so the other module's hook was skipped: in AC Origins, Pharos's `d3d11.dll` proxy
  hooks the factory's `CreateSwapChain` after the clock hooked it from the game's
  `CreateDXGIFactory` import, the clock then saw the device's factory (same vtable) after the
  patched `D3D11CreateDevice` returned and hooked again, and Pharos never saw the swap chain.
  Chaining onto the other hook instead (ours → theirs → ours) is wrong for a shared vtable:
  with one original per slot it recurses for ever, with a per-call depth it runs ours twice,
  and the two hookers would take the slot back from each other for every new object. The log
  says `… now starts at <addr> in <module>, another module's hook over ours: left in front`.
  A hook that was there before ours is chained onto as before (ours → theirs → original). An
  unregistered copy of a hooked vtable (a per-object vtable) whose slot reached our hook gets
  the original of that hook's entries for the slot when they all agree, else the call fails
  as before.
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

- **Creep.** Once **one thread** has read the clock `clock.creep_after` (4096) times in one
  frame, it can only be spinning on the clock: each further read by that thread moves time on
  by 1/1024 of a frame, so a wait for more time than a Present can give terminates. Reads are
  counted per thread, so many threads that each read a little (a job system, timer threads)
  don't add up to a spin. The test's QPC busy-wait for two frames and its `Sleep(1)` loop on
  timeGetTime (both on the presenting thread), and a worker's busy-wait while the presenting
  thread waits for it, all end this way. Creep depends on how often the game reads, not on the
  machine, so it is still repeatable, but those frames aren't exactly 1/N.
- **The real-time escape.** If a frame takes `clock.stuck_ms` (1000) of **real** time, time
  follows real time from there until the next Present. This catches waits that read the clock
  rarely, and loading screens that never present. **When it fires, that frame's timing tracked
  the machine, not the frame count, and isn't reproducible.** The report counts escapes, and
  `quiver lab logs` marks the run `NOT FULLY DETERMINISTIC`. Ishimura used 250 ms; the lab
  uses 1000 ms so that a loaded machine's slow frames stay on the virtual clock. Raise it with
  `clock.stuck_ms` if loading screens trip it.
- **Never backwards.** A Present sets time to max(grid, now); a frame where creep or the
  escape had already passed the grid is an **overrun**.
- **Sleep** never advances virtual time. Only the **presenting thread's** sleeps get shorter in
  real time (at most `clock.sleep_max_ms`, default 0: a yield), so a sleep-based frame limiter
  spins on the clock and creeps. Every other thread's sleeps stay as asked (`clock.sleeps=all`
  shortens everyone's, `none` no one's). The first live runs showed why: AC Chronicles China
  (x86) and Rabbids Coding (Unity) have background threads that sleep-poll, 200 million sleeps
  of 118 ms on average in AC China's 90 s. With every sleep shortened they became clock-read
  storms (1.9 million reads in one frame in Rabbids), and half the game's frames crept: AC
  China's census had 160000-tick steps only 4385 times against 9.8 million creep steps. The
  test reproduces it with two pollers (`Sleep(1)` and `Sleep(100)`); with `clock.sleeps=all`
  phase A stops being exact.
- **Other processes.** A game's helper (Unity's crash handler, a launcher) that loads the
  proxy too runs the clock, but it writes the report only if it presents. The lab names the
  game's executable (`QUIVER_VCLOCK_EXE`); every log line carries its pid. In Rabbids Coding's
  first run the crash handler's never-engaged report replaced the game's, which had engaged at
  37x: "no Present was hooked" was that, not a missed swap chain.

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
  FILETIME and ms scaling, the report's parsing and verdicts, and the embedded DLLs. The
  vtable registry on fake vtables whose calls are traced: another module's hook over ours
  stays in front and each runs once (`P>H>D`), 50 more objects of the class change nothing,
  a third module over both, unhooking, the original put back, a hook already there chained
  onto (`H>P>D`), vtables told apart, a copied vtable's original, a full registry.
- `cargo test -p quiver-core vclock`: `clock=` edits and the manifest, proxy choice (an
  import, a DLL beside the exe, a shipped DLL skipped, a mod's refused when forced, no way in),
  the DLL placed in the clone and removed at the next launch, never outside the instance, the
  environment (DXVK_CONFIG merged), the report lines and the brief.
- Opt-in, in Wine: `QUIVER_TEST_PROTON=… cargo test -p quiver-vclock --test wine -- --ignored
  --nocapture --test-threads 1`. `tests/wine/clock_test.c` presents through DXVK on a private
  Xvfb (display ≥ :400) with the hook as `version.dll`, for D3D11 x64 fast (with a helper
  process loading the proxy), again, and held to 40 fps, D3D11 with the device's own factory
  (Unity's way), every sleep shortened (the old behaviour), D3D9 x64 and D3D11 x86. Two
  background threads poll the clock with `Sleep(1)` and `Sleep(100)` throughout. Measured
  2026-11-18 on GE-Proton11-12:
  - every one of 1198 steps on QPC is 160000 ticks, and on timeGetTime, GetTickCount and
    GetTickCount64 every step is 16 ms;
  - 19.168 s of game time took 0.56-0.65 s of real time with a fast presenter (30-34x) and
    30.1 s with the 40 fps one (0.64x);
  - the three sequence hashes are identical, as are the creep, overrun and shortened-sleep
    counts (the census isn't compared: the pollers sample it at real-time moments);
  - the busy-waits terminated, and exactly their 70 frames crept (no escapes); the pollers'
    sleeps stayed real and crept nothing. With every sleep shortened, phase A wasn't exact;
  - the helper process didn't replace the game's report;
  - `GetProcAddress("QueryPerformanceCounter")` gives the virtual value;
  - only the executable's 7 clock imports point into the DLL; none of the other 28 modules'
    (Wine's builtins, DXVK's d3d11, dxgi and d3d9) do.

  `another_modules_hooks_stay_in_the_chain` (`d3d11chain`, x64 and x86, 2026-11-20 on
  GE-Proton11-12) replays Origins with Pharos: a factory from `CreateDXGIFactory1` (the clock
  hooks its `CreateSwapChain`), the test exe hooks the same slot over the clock's, then
  `D3D11CreateDevice` (the clock is asked for the device's factory, the same vtable), then
  `CreateSwapChain`, whose hook also hooks the swap chain's `Present` over the clock's. Its
  CreateSwapChain hook ran once and its Present 671 times (every present), the slot still
  held its hook, the clock counted 670 frames after engaging with every step exactly 160000
  ticks. With the old DLL the same run gave `their_create_calls 0 their_presents 0`.

### 8.2 Pause and frame stepping (2026-11-19)

```sh
quiver lab clock N pause              # hold at the end of the frame in flight; prints frame and virtual time
quiver lab clock N step 12            # exactly 12 more frames (each 1/HZ s), then hold again
quiver lab clock N step               # one frame
quiver lab clock N resume             # run free
quiver lab clock N status [--json]    # running/holding/held, frame, virtual time, steps taken
quiver lab clock N step 12 && quiver lab shot N     # the 12th frame, on screen
quiver lab key N --down Right; quiver lab clock N step 12; quiver lab key N --up Right; quiver lab clock N step 1; quiver lab shot N
```

Needs the virtual clock on for the run (`clock=virtual:HZ`, or `launch --clock virtual:HZ`). `pause`
and `step` wait until the game is held there (`--no-wait` returns once the game has the
command; `--timeout S`, default 300, gives up waiting, never the hold). A step from running is a
pause that lets K frames through, counting the one in flight; a step while a step is under way
extends it; a pause during a step stops it at the end of the frame in flight. Right after
`quiver lab launch`, `pause` and `step` wait for the game's process to map the page, and a
pause that arrives before the first Present holds at frame 1: `quiver lab launch N && quiver
lab clock N pause` starts the game held at its first frame.

**Where it holds.** In the DLL's Present hook, after the real Present returned and the clock
moved on its one frame: what the game just presented stays on screen (under DXVK the
translation layer's own present runs on its thread a moment later, and gets there too), and
the game's next frame starts when it's released. Frame `k` (the `k`th Present since engagement)
was rendered at virtual time `(k - 1)/HZ`; the status shows `k` and `k/HZ`, the time the next
frame will see. A second thread presenting while one is held is held before its frame counts,
so time can't move under a hold that way either.

**Time doesn't move while held**, however long: every clock read returns the frozen value. The
valves stand down for it: reads while held don't count towards the creep (a thread polling
the clock while the game is held isn't a spin), and the real-time escape measures a frame from
its release, not across the hold (without that, the first frame after a pause longer than
`clock.stuck_ms` would escape to real time). Other threads' sleeps were never virtual and stay
real. What still moved is counted: `leaked` is the virtual ticks between a hold's start and its
end (0 by construction; non-zero would be a race between a read and the hold) and is in the
report and the verdict; the reads while held (`held_reads`) are counted too. Clocks the DLL
doesn't reach (audio, timers, rdtsc: §8.1 limits) keep running while held, as they always did.

**Determinism.** A Present advances exactly as it does free-running, and a hold adds nothing,
so stepping K at a time gives the same virtual-time sequence as running free: the host test
runs a stand-in game (frames of varying real length, busy-waits that creep, a background
reader) free and then paused and stepped 1, 12, 1, 7, 64, 3, 100, 1, 1, 2, 250 with 2.5 s real
holds (longer than the escape) and 10 000 reads per hold, and requires identical sequences
and identical creep, overrun and escape counts; the Wine test does the same with the real DLL
(the sequence hash equals the free-running one).

**The control channel: a shared page.** The game's process (the one named by
`QUIVER_VCLOCK_EXE`, never a helper) creates `runs/r<N>/vclock.ctl` (4 KiB) and maps it
(`CreateFileMapping`: under Wine a shared mmap of the file, so the host's `mmap` of it sees the
same bytes); `quiver lab clock` maps it on the host. Layout `vc_ctl` in `win/vclock.h`,
mirrored by `quiver_vclock::control`: the controller writes `cmd`, `cmd_arg`,
`cmd_timeout_ms`, then `cmd_seq + 1` (under `flock`, so two controllers serialise); the DLL
applies each command once, publishes its state (a seqlock: frames, virtual ticks, hold target,
the counters, the valves) and then `ack_seq`. Commands are read at every counted Present (one
atomic load), by the DLL's status thread every 50 ms (so a game in a loading screen still
acknowledges), and every millisecond by the held thread, which polls with real 1 ms sleeps (the
DLL's own `Sleep` import). Robustness:

- **No controller, no hold.** Only a command holds; a new process claiming the page ignores
  commands written before it (`ack_seq = cmd_seq` at attach), so nothing from an earlier
  process or run applies.
- **A deliberate pause never times out by default**: the agent controlling it decides. Holding
  costs nothing (a 1 ms poll), and Wine doesn't mind a thread blocked in Present.
  `--auto-resume SECS` on `pause` or `step` is a safety net for a controller that may not come
  back (counted as `auto_resumes`).
- **The game dying** leaves a page that just stops changing: `quiver lab clock` checks the run's
  state while it waits and stops with the exit. `quiver lab kill`, `launch` and `down` work on
  a held game as on any other (a new launch is a new run folder and page).

**Shots while held.** `quiver lab shot` asks gamescope, which keeps compositing the last frame
the game presented: two shots 2 s apart while held are byte-identical. `shot --hdr` comes from
the Sight layer, which captures in the present path, and a held game presents nothing; so while
`quiver lab clock` holds a Sight instance it writes `capture.hold` into the session dir, and
the layer (seeing it within 250 ms) copies every frame it presents into a host buffer and
waits for the copy (only while holding), keeping the latest (`HDR_CAPTURE_HELD` in the status
block). A capture request is then served from that copy once no newer frame has come for 100
ms. `pause` waits (up to 1.5 s) for the layer to say it's keeping frames before it holds, so
the frame the pause stops on is kept; `resume` removes the file. An auto-resume leaves it until
the next `resume` (the copies then cost a frame's readback each).

**Input while held.** `quiver lab key N --down K` presses and leaves keys down, `--up` lets
them go (`quiver lab click N FX FY --down/--up` for buttons). Input sent while the game is held
is processed when it next steps: Wine turns X events into Windows input when the game's window
thread pumps messages, and that thread is blocked in Present. So a key down before `step 12`
is seen in all 12 frames and the frame after, and a plain `quiver lab key` (press and release)
while held lands both in the same frame, which a game that polls key state can miss; the
commands say so. The Wine test checks the three ways games read the keyboard: window messages
(WM_KEYDOWN/UP), raw input (WM_INPUT) and polled state (GetAsyncKeyState) all saw the Right
arrow in exactly the frames from the step after the press to the step after the release.
DirectInput keyboards are built on the same input under Wine. Gamepads are section 15's virtual
pads (`quiver lab pad`), whose scripts step the clock themselves. A game that reads input on a thread that isn't held, or pumps messages on another
thread than the one presenting (Unity's main thread, while its render thread presents), can
see input before the next step, but its simulation still only gets time one frame per step.

**Records.** The DLL logs every command, hold and release to `vclock.log` (`control: step 12 at
frame 9618 …: holds at frame 9630`, `held at frame 9630 (virtual 154.179666 s)`, `released …`),
and `vclock.status` carries the state and counters. `quiver lab logs` prints the last 20 of
those lines and the `pause/step:` line (holds, step frames asked, presents while holding, time
held, reads while held, leaked); `quiver lab status` prints the same line and the live state
from the page. The speed-up leaves time held out.

**Tests.** `cargo test -p quiver-vclock`: the hold state machine in `vclock.c` (a pause holds at
the end of the frame in flight, steps count presents and add up, a pause shortens a step, a
step from running, before the first Present, 10 s held with 200 000 reads frozen with no creep
or escape, the timeout only when asked), the stepped-vs-free sequence above, the control page
(stale commands ignored, applied once, acknowledged with the target, the state block's layout
read back field by field) and the same loop on real threads with a reader hammering the clock.
The Wine test's `stepped` case runs the D3D11 presenter paused from its first frame through the
page, held 3 s (frozen, the pollers reading), stepped 1, 12, 1, 100 (each exactly `k * 160000`
ticks) with the Right arrow held across the first 13 frames, resumed; its phase A sequence
equals the free-running one, the 70 busy-wait frames creep as before, nothing escapes or leaks.

**Live** (2026-11-19, Rabbids Coding in a Sight instance at 62.5 Hz, 3.9x when running free):
paused at frame 9618 (virtual 153.987666 s); 10 s of real time later still frame 9618, the same
ticks, with 4847 reads by Unity's other threads while held; `step 12` -> frame 9630, +0.192000 s;
`step 1` -> 9631, +0.016 s; two SDR shots while held identical; `shot --hdr` of frame 9632
pixel-identical to gamescope's shot of it; a click while held did nothing until the steps,
then the title screen reacted; `step 500 --no-wait` then `pause` stopped at 9698; `step 5
--auto-resume 2` held at 9703 and ran free 2 s later; every frame's QPC step in the census was
160000 ticks; leaked 0. A second run, `launch` then `clock pause` at once: held at frame 1
(2.3 s after the launch), `step 300` -> frame 301, +4.800000 s, its SDR and `--hdr` shots
pixel-identical, no escape in the run (the free-running first run had one, in its loading).

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
- Live Options for a lab mod (`set` from a lab Options page): the lab's mod channel is
  read-only (section 14).
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
   GPU lock (`quiver lab gpu`) only coordinates lab runs with each other. What the user can
   do is set limits on the lab (section 15): theirs, enforced on instances only, never on
   their games, and nothing is refused or warned about.

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

## 14. The mod SDK in an instance (2026-11-19)

A mod in an instance connects through the mod SDK (`quiver_mod.h`) as it does at home, so
what it says reaches Sight in the instance: Torchbearer's `quiver_set_stereo_layout("hsbs")`
puts the HUD and the panel in both eyes with no `sight.stereo` setting. It used to be
refused: quiverd's registry feeds the person's Options, perf history and mod revisions, and a
lab mod must reach none of them. Now the channel ends **in the instance**, read-only.

- **The endpoint**: the supervisor runs a `ModRegistry::lab` (`crates/quiver-cli/src/lab/sdk.rs`)
  on the host's loopback, at a port `up` picks (`mods-port`, the same as last time if free)
  and forwards into the container at the same port (`podman::Host::mods_port`, pasta `-T`). It
  takes only this instance's lab sessions (`lab-<instance>-r<N>`), validated as quiverd
  validates every channel. quiverd's registry still refuses lab sessions, so a channel that
  reaches it anyway (an older bridge) ends there as before.
- **The bridge**: an Epic (or DRM-free) launch runs the bridge in launcher mode with a
  mods-only context (`--run context.json -- game.exe …`: the pipe, the relay to the endpoint,
  the game; no Connect pipes, no `UPLAY_ARGUMENTS`, the licence arguments not logged). A Ubisoft
  launch's context gets `mods_relay` (the endpoint), so its mods' channels go there while
  Denuvo still goes to quiverd's broker. No endpoint (a container from before it: `down`/`up`)
  or a bridge without `quiver_ipc::MODS_ONLY_MARKER`: no SDK this run, and a `mod SDK:` note
  says why. The launch never fails over it.
- **Read-only**: the welcome carries no values (the mod reads its files; `quiver lab set`
  changes them), a mod's own `changed` is logged in `events.jsonl` as "kept in the instance,
  not saved", and nothing sets options or runs actions on it. Its metrics stay in its
  connection's status.
- **What it writes**: `runs/r<N>/sdk.json` (the run's connections: surface, protocol, module,
  caps, metrics, stereo) and, for a Sight instance, `sdk-stereo.json` in the run's session dir
  (`quiver_api::lab::SDK_STEREO_FILE`: layout, surface, connection), removed when the mod
  withdraws its word or its connection goes. The run's lab `quiver-sight` reads it five times
  a second and resolves the layout as quiverd does (the mod's word, else the launch's
  setting, else mono).
- **Status**: `quiver lab sight N status` prints `stereo: hsbs (mod: torchbearer (SDK); setting
  auto) — the HUD and panel are laid out at 1920x1080 and drawn in each eye` and a `mod SDK:`
  line per connection; `--json` has `mod_sdk`. The supervisor's events say `mod SDK: torchbearer
  connected (protocol 3, …)` and `mod SDK: torchbearer says its frames are hsbs`.

Tests: `lab::sdk::tests::lab_channel_is_read_only_and_carries_stereo` (welcome without values,
the word in the session dir and gone with its connection, a change kept in the instance,
other instances' and the person's sessions refused), `modsdk::tests::lab_sessions_only_in_a_lab_registry`,
`podman::tests::epic_names_go_to_the_eos_proxy` (the forward).

## 15. Lab limits (2026-11-20)

The person can cap what lab instances use, so agents can keep working while they play:
"lab may use 20% of the GPU and 6 cores while I'm playing". The limits are the person's
(`config.json` → `lab.limits`, like the download settings), set with `quiver lab limits`,
and enforced quietly on instances only. Quiver never stops, slows, warns about or touches the
person's own games (section 11): nothing is refused, a limit only shapes the lab.

```sh
quiver lab limits                                   # the profiles, and what each running instance enforces now
quiver lab limits set while_playing gpu=20 cpu=6    # the default
quiver lab limits set always memory_max=24G cpu_quota=8
quiver lab limits set always gpu=off                # KEY=off lifts one
quiver lab limits reset [always|while_playing]
```

**Profiles.** `always`, and `while_playing`, which applies on top of `always` (the stricter
value per field) while one of the person's own games runs: a session quiverd holds for Quiver's
launcher (`launcher`) or a Steam launch through `quiver steam-run` (`steam`), as
`Request::Sessions` reports them. Lab runs (`lab`) and other bridge sessions (`bridge`: a
container such as a mod harness) aren't the person playing. The supervisor asks quiverd every
3 s, only when `while_playing` has something in it; without quiverd it says "unknown" and only
`always` applies. Defaults: `always` none; `while_playing` gpu 20%, cpu 6 cores.

Each profile has `cpu` (whole cores), `cpu_quota` (CPUs), `memory_high`, `memory_max` and
`gpu` (percent). Every instance's supervisor applies them to its own container within a few
seconds of a change (a new setting, the person starting or stopping a game), and writes what
it enforces to `<instance>/limits.json`.

### 15.1 CPU and memory: the container's cgroup

The person's systemd delegates `cpu memory pids` (not `cpuset`). Measured with podman 6.1.4
and runc 1.5.2, rootless:

- `podman run/update --cpuset-cpus` fail: `openat2 …/libpod-<id>.scope/cpuset.cpus: no such
  file or directory`. **cpuset isn't available.**
- `podman update --cpus N --memory M` work, but can't lift a limit again: `--cpus 0` and
  `--memory 0` leave the old values, `--cpu-quota=-1` too, and `--memory -1` is refused.
- The container's scope (`/sys/fs/cgroup/<State.CgroupPath>`) is the person's, so its
  `cpu.max`, `memory.high` and `memory.max` can be written directly, both ways.
- runc resets a container's init to every CPU whatever podman was started with, and every
  `podman exec` (each launch, each tool) starts with every CPU too.

So the supervisor (`lab/limits.rs`, `quiver_core::lab::limits`) enforces:

- **cores** as **CPU affinity** of every thread in the container (`cgroup.procs`, then each
  `/proc/<pid>/task`), re-applied every 2 s, plus a **quota of the same number of CPUs** in
  `cpu.max` as the hard ceiling between passes (a game may set its own affinity, and exec'd
  processes start on every CPU). Which cores: whole cores, both SMT threads, the slowest first
  (ACPI CPPC `highest_perf`, else the max frequency), highest-numbered among equals. On the Ryzen
  AI 9 HX 370 (4 Zen 5 cores at CPPC 196–208, 8 Zen 5c at 125), 6 cores are cpus
  `6-11,18-23`: Zen 5c cores sharing one L3, leaving the person's game the four fast cores and
  two compact ones. Measured: all 99 threads of a running AWR instance on `6-11,18-23`,
  `cpu.max` `1200000 100000`.
- **cpu_quota** in `cpu.max` (the smaller of it and the cores' implied quota).
- **memory_high / memory_max** in `memory.high` / `memory.max`.

Lifting a limit writes `max` back and puts every thread on every online CPU.

### 15.2 The GPU

No cgroup controller covers NVIDIA graphics, so the GPU share is enforced from inside the game
and measured from outside:

- **The page.** When any profile has a GPU share, every launch gets a `gpu.ctl` in its run
  folder (`quiver_perf::labgpu`, 4 KiB of atomics) and `QUIVER_LAB_GPU` naming it. An instance
  without Sight gets the Sight layer for it: `up` mounts the host's Sight libraries read-only
  (`podman::Host::layer`, marker `<instance>/gpu-layer`) and the launch names the layer in
  `VK_INSTANCE_LAYERS`. With no `QUIVER_PERF_DIR` the rest of the layer passes everything
  through (no HUD, no ring, no capture). A container from before this needs `down`/`up`.
- **Priority** (`quiver-sight-layer/src/labgpu.rs`). At `vkCreateDevice` the layer adds
  `VkDeviceQueueGlobalPriorityCreateInfoKHR { LOW }` to each queue whose family offers LOW
  (`VK_KHR_global_priority`, else `VK_EXT_global_priority`; the families' levels from
  `VkQueueFamilyGlobalPriorityPropertiesKHR`), and enables the extension. If the driver refuses,
  the app's own create info is used. What happened is in the page and in `quiver lab status`.
  **NVIDIA 620 on the RTX 5070 Laptop offers only MEDIUM** in every queue family (the
  extensions are there, and a LOW device is even created, but there's no level below the
  default), so on this machine the priority does nothing and the layer says `priority: the
  driver offers nothing below the default`. RADV (the Radeon 890M) offers LOW and MEDIUM, and
  gets LOW (test `vulkan_labgpu`).
- **The governor** (`limits::Governor`, in the supervisor, once a second). It measures the
  instance's share with NVML's `nvmlDeviceGetProcessUtilization` (SM busy percent per host pid,
  summed over the container's processes; `quiver_core::lab::nvml`) and holds it to its part of
  the share by pacing presents: the layer waits in `vkQueuePresentKHR` until the page's interval
  since the last present has passed, so the game's next frame waits too. The part is
  `max(share / instances, share − others)`: other lab instances' use counts against the lab's
  share, and an even split is guaranteed. Over its part, the cap drops to the frame rate that
  would have used exactly that (use scales with frames); well under it, the cap rises by at
  most 30% a tick and goes away when the game no longer reaches it. Never below 2 fps. NVML is
  opened only while a run is up under a GPU share (it keeps the dGPU awake).
- **What "percent" means.** Two things learned measuring it on driver 620:
  - NVML keeps **one sample per process**, refreshed every 0.2–0.8 s, each over its own
    period. Reading "samples since the last read" drops a process from every read where its
    sample didn't change, which first gave a median of 0% for a game using 20%. So each read
    takes every process's latest sample from the last 2 s.
  - Utilization is time busy **at the current clock**. Held to a few frames on an otherwise
    idle GPU, the GPU clocks down and each frame counts for more "percent", so the governor
    chased its own tail down to 2 fps (first A/B run below: 20.6% busy at 6 fps). The share is
    now taken at full clock: busy × SM clock ÷ max SM clock (`nvml::Reading::at_full_clock`).
    When the person's game keeps the GPU busy, the clock is high and the two agree; on an idle
    GPU the lab gets more frames for the same busy time, which costs no one anything.

**Measured** (2026-11-20, Alan Wake Remastered in instance `lim-awr`, 1280x720 with gamescope
at 240 Hz so it isn't held to 60, standing in its first gameplay scene; the share toggled
off/20% every 90 s, A B A B; NVML per container once a second, the first 10 s of each phase
dropped). The GPU was never idle: another agent's AWR instance (`awr-rt`, whose supervisor
runs an older Quiver without limits) and a Pharos container (`aco-t1`, Origins) were rendering
throughout, which is the case the limit is for.

| phase | instance's GPU share (at full clock) | busy time | presents/s | governor's part |
|---|---|---|---|---|
| off | 49.9% (p90 80) | 56.6% | 43.0 | - |
| 20% | 10.5% (p90 22) | 14.0% | 11.3 | 10% (awr-rt used ~54%) |
| off | 48.7% (p90 85) | 60.8% | 49.4 | - |
| 20% | 10.3% (p90 19) | 16.1% | 37.3 | 10-20% |

- It holds: the instance went from ~50% to ~10% of the GPU, within its part. Its part was
  10% because the other lab instance used more than the whole share (`max(share / 2,
  share − others)`); that instance isn't governed (old supervisor), so the lab as a whole was
  over 20%. **Only instances whose supervisor has this code are held.**
- Per second it's loose: p90 19-22% against a part of 10-20%, single seconds up to ~50%
  busy. NVML's samples are 0.2-0.8 s each and lag; the governor moves the cap at most 25% a
  second, so a scene change takes a few seconds to settle.
- Earlier runs, before the two fixes in "What percent means": reading only new samples gave a
  median of 0% (the cap never engaged); busy time without the clock correction held the game
  at 20.6% busy but drove it to 6 fps, with the GPU otherwise idle and clocked down.
- What it costs the lab: 43-49 fps became 11-37 fps, so wall-clock numbers in a limited
  instance are the limit's, as `quiver lab` says (15.3).
- CPU, same run: all 99-128 threads of the instance on cpus `6-11,18-23`, `cpu.max` `1200000
  100000`; with `while_playing` and nobody playing (quiverd's real answer, the Pharos
  container's `bridge` session not counted) `max 100000` and every thread on `0-23` within a
  tick.
- The test `vulkan_labgpu` (RADV, the 890M): 60 presents take 0.24 s free and 1.26 s at a
  20 ms interval (59 held), device at LOW. `driver_low_priority` prints each driver's levels.

### 15.3 What agents are told

Under a limit, wall-clock speed in an instance measures the limit. So:

- `quiver lab launch` adds `THROTTLED: lab limits in force (…)`, and says that fixed-step
  virtual-clock results are unaffected (with the clock on) or how to get them (without);
- `quiver lab status` prints the enforcer's line (`lab limits: while playing: Alan Wake
  Remastered is running → cpu 6 cores (cpus 6-11,18-23), gpu ≤20% (priority …, governor
  holding 18% at ≤41 fps)`) and the same note; `--json` has `limits` and `throttled`;
- `quiver lab bench` labels its table and `bench.json` (`"timing": "throttled (lab limits in
  force, wall clock)"`, `"limits"`, `"virtual_clock"`) when a limit was in force without the
  virtual clock; `quiver lab gpu --for N` prints the note before the measurement;
- `quiver lab host` lists every instance's line; `quiver lab limits` shows the profiles, whether
  the person is playing, and each instance;
- the brief has a "Lab limits and the virtual clock" section, and the `quiver` skill says the
  same: the limits are the person's, don't change them, and test on the virtual clock
  (§8.1, §8.2), whose results don't depend on having the GPU.

`QUIVER_LAB_LIMITS=<file.json>` reads and writes the limits there instead of `config.json`
(tests), and `QUIVER_LAB_PRETEND_PLAYING=<titles>` in a supervisor's environment makes it act
as if those games ran (it can only make `while_playing` apply, never lift a limit).

### 15.4 What the platform can't enforce

- **cpuset.** Not delegated; affinity is re-applied every 2 s and a thread can set its own in
  between (the quota still holds). Enabling it would need `Delegate=cpuset` for the person's
  `user@.service` (root), which Quiver doesn't do.
- **A hard GPU share.** Nothing in the kernel or the NVIDIA driver caps a process's GPU time;
  the governor paces presents. Work that doesn't present (loading screens, shader compiles,
  compute) can't be paced, and the status says so when it sees GPU use without presents. The
  measurement is NVML's SM busy share per process, sampled about every 1/6 s, so short spikes
  pass, and a game whose use doesn't scale with frames (a fixed cost per second) isn't held.
- **Priority on NVIDIA.** Offers no level below the default on this driver (above). Nothing in
  a rootless container can raise the person's game's priority instead, and Quiver wouldn't
  touch their game anyway.
- **VRAM.** Nothing limits it. On this 8 GB GPU, other agents' instances filling it made the
  driver fail channel allocations (`NVRM … NV_ERR_STATE_IN_USE` in the kernel log), so new
  devices failed: gamescope's at `up`, a game's at launch ("Your GPU must support DX12").
- **Non-Vulkan rendering.** The layer sees Vulkan only (DXVK, vkd3d-proton, native Vulkan);
  WineD3D/OpenGL games aren't paced.
- **Memory bandwidth, caches, PCIe, power.** Shared, unlimited. A lab at 20% of the GPU still
  heats a laptop that's shared with the game.
## 16. Game controllers in an instance (2026-11-20)

An agent can press pad buttons, move sticks and pull triggers in an instance, and see the game
react: `quiver lab pad`. Each pad is a Linux input device laid out as a wired Xbox 360
controller, made on the host with uinput, **visible only inside that instance's container**.
Windows games find it through Wine's winebus (SDL), so XInput, DirectInput, raw input and
Windows.Gaming.Input all see a controller; native Linux games find it through evdev, SDL or
libudev. Code: `crates/quiver-pad` (the device, the rule's checks, scripts, the Windows probe),
`crates/quiver-cli/src/lab/pad.rs` (the pad host, the commands, status), `podman::PadMounts`.

```sh
quiver lab create GAME --pads          # or: quiver lab edit N pads=1 (2 for player 2), then down/up
quiver lab pad N press a               # press and let go (120 ms; --for 500ms, --for 12f)
quiver lab pad N hold lb               # down until release (release with no button: all)
quiver lab pad N stick left 0 1 --for 2s    # Y up, -1..1; without --for it stays
quiver lab pad N trigger rt 1          # 0..1
quiver lab pad N seq "a 100ms; wait 200ms; lstick=0,1 1s; +rt=1; x; neutral"   # or @FILE; --dry-run
quiver lab pad N --pad 2 press start   # player 2
quiver lab pad N status [--json]       # hidden?, state, rumble, who has it open, what the game sees
```

### 16.1 Keeping it from the host

A uinput device is global to the kernel. Without care, Steam, the person's running game,
hyprpad and the compositor would all see a phantom pad. That must never happen, so:

- **The udev rule** (`contrib/lab/72-quiver-lab-pad.rules`, `quiver lab pad-rule` prints the
  build's copy; installed by the system package to `/usr/lib/udev/rules.d`, with the group from
  `contrib/sysusers/quiver.conf`). It matches every lab pad by name (`Quiver Lab Pad *`)
  and phys (`quiver-lab/*`), on the input device and on its `event` and `js` nodes, and:
  - removes the `uaccess` and `seat` tags. 70-uaccess tags every joystick, and 73-seat-late turns
    the tag into an ACL for the session user, so the rule sits at 72, between them (the window
    `/etc/udev/rules.d/72-hyprpad-puck.rules` uses);
  - sets `LIBINPUT_IGNORE_DEVICE=1` and clears `ID_INPUT`/`ID_INPUT_JOYSTICK` and `ID_SEAT`;
  - sets `OWNER root`, `GROUP quiver-lab-pad`, `MODE 0660` (final assignments).
  The group is gid **105000**, in the user's subordinate gid range. Nobody is a member and no
  host process runs with it. udev needs a group *name* it can resolve: a bare number is ignored
  ("Unknown group"), hence the sysusers file.
- **Why there is no window.** devtmpfs makes the node root 0600, and udev applies the rule
  before it tells listeners (Steam's SDL, libinput, hyprpad) about the device.
- **Reading `udevadm info` for a pad.** `TAGS=:seat:uaccess:` stays: it's udev's sticky list of
  every tag the device ever had. What counts is that there's no `CURRENT_TAGS` with `uaccess`
  (the pad host checks the database's `Q:` lines) and no ACL (`getfacl` shows only
  `user::rw- group::rw- other::---`).
- **The container's gid.** Rootless podman maps the user's gid to 0 of its namespace and the
  subordinate range after it, and `--userns=keep-id` then maps container gid G > 1000 to host
  `100000 + G - 1` (subgid 100000:65536). So host 105000 is container gid **5001**
  (`rule::container_gid`, measured: a file `podman unshare chown 0:5001` shows as 105000 on the
  host and is readable in a keep-id container with `--group-add 5001`, by `podman exec`
  processes too). The container gets `--group-add 5001`. Quiver reads the rule's group and
  maps it, so another gid in the range works.
- **Never without the rule** (`rule::plan`, before any device): the rule file is installed (the
  one udev uses for the name: /etc, /run, /usr/lib) and is ours (`quiver-lab-pad rule v1`), its
  group resolves, has no members, isn't one of the user's groups, and is inside the subgid
  range. Otherwise `up` refuses with the install commands, and no device is made.
- **The canary.** Before the first real pad, the pad host makes a device with the same name
  pattern and phys (`…/canary`) but no keys, axes or rumble and a virtual bus id. It isn't a
  joystick, so no rule grants it and nothing binds it. udev's database entry for it must show
  that the rule ran (`E:QUIVER_LAB_PAD=1`), and its node must be `root:105000 0660` with no ACL.
  Only then is a joystick made. So a rule that doesn't match never meets a real pad.
- **Every node, after udev** (`rule::wait_hidden`): once udev's entry shows the rule ran, the node
  must be root-owned, in the group, mode 0660, with no ACL and no current `uaccess` tag, and
  **this process (the user) must fail to open it**. Any failure destroys every pad at once
  (closing the fds) and `up` refuses. The pad host re-checks every 5 s and destroys the pads if a
  node becomes openable or gets an ACL (a changed rule re-triggered).
- **One container, its own nodes.** The container gets `--device /dev/input/eventN` (and `jsN`)
  for its own pads at the same paths, never `/dev/input` (the person's real devices are ACL'd to
  the same uid that keep-id maps in). Every lab pad is in the shared sysfs, but another
  instance's has no node in this container, and a rootless container can't make one.

### 16.2 The pad host

`up` starts `quiver lab pad-host N` (detached, its own session; log `<instance>/pad/host.log`)
**before** the container, and waits for `<instance>/pad/host.json`: the pads and their nodes,
or why not. The pad host holds the uinput fds, so its pads exist exactly as long as it does:

- it exits, destroying them, when `down` (or `rm`) tells it to, or when the container has been
  gone for two checks 2 s apart (120 s if the container never appears);
- if it crashes or is killed, the kernel destroys the pads with the fds;
- at the next `up`, any lab pad whose instance's container isn't running has its pad host
  stopped (`pad::sweep`, by phys and command line), and so does this instance's old one.

It answers `<instance>/pad/host.sock` (`quiver_pad::host::Request`: get, apply changes to pad K,
quit), writes each change as one evdev frame (only the codes that changed, then SYN_REPORT), and
services force feedback: a game's rumble upload blocks until uinput's owner answers, so uploads
and erases are acknowledged and plays recorded (`status` shows the last strength).

The device is xpad's wired Xbox 360 layout (045e:028e, version 0x0114, bus USB): A B X Y LB RB
Back Start Guide LS RS, both sticks ±32767 on ABS_X/Y/RX/RY (Y flipped as xpad does), triggers
0..255 on ABS_Z/RZ, the d-pad as hat 0, FF_RUMBLE. SDL's mapping database knows it by that id.
Names: `Quiver Lab Pad <instance>`, then `… P2` for the second. The container has no udevd, so
the pad host writes a minimal `/run/udev` for it (`data/c13:<minor>` and `+input:inputN` with
`ID_INPUT_JOYSTICK=1`, mounted read-only), for libudev users. At most four pads per instance.
The socket is `$XDG_RUNTIME_DIR/quiver-lab-pad-<hash of the instance folder>.sock` (a lab root
can be deeper than a unix socket path may be long).

Two details found live, both about matching what a real xpad pad does:

- **Rest values.** xpad reports a centred Y as `!0 = -1`, and Wine's XInput inverts it back to
  0. The kernel starts a uinput axis at 0, which XInput read as `ry -1` until the stick first
  moved, so the device is made with both Y axes at -1.
- **SDL's trigger heuristic.** A trigger rests at SDL's minimum, and until an axis has had a
  "second value" SDL takes a move to |v| < 8192 (raw 96..160 of 0..255, about 0.37-0.63) for the
  real rest position and drops it (`SDL_SendJoystickAxis`). A real trigger passes through lower
  values on its way, so it never hits this; ours jumped straight to 0.5 and XInput never saw it.
  So a trigger moving from rest into that band gets raw 64 first, in the same evdev frame. Under
  the virtual clock the game is held while both arrive, so it can't see the 64.

Measured in the container, without `SDL_JOYSTICK_DISABLE_UDEV`: SDL3 (native) lists the pad as
"Xbox 360 Controller" (type XBOX360) both through its udev path (the minimal database) and
without it; Wine shows exactly one controller per pad (no second copy through winebus's own
udev/evdev backend).

### 16.3 Timing and the virtual clock

Without the virtual clock, durations are wall-clock sleeps. With it (`clock=virtual:HZ`, the run
has a control page, §8.2), **durations are frames**: the command pauses the game if it runs (or
uses the hold it's in), and for each step applies the change, waits `pad.settle_ms` (40 ms real)
while the game is held, so evdev, SDL in winedevice, winebus and XInput's own thread all have it,
then steps exactly `round(ms * HZ / 1000)` frames (at least 1), or `Nf`. At the end a game that
was running is resumed; one that was held stays held (it sees the last change at its next step).
`--real` uses the wall clock anyway. The game reads input at its own frame start, so a change
applied while held at frame k is seen in frame k+1, the same in every run.

Scripts (`quiver_pad::seq`): `a` (down for the step), `+a`/`-a` (stays), `lx=0.5`, `lstick=0,1`,
`rt=1` (for the step, then back where it was), `+rt=1` (stays), `neutral`, `wait`, and one
duration per step (`120ms`, `1.5s`, `12f`). A step's lasting changes go first; its momentary
ones are undone at its end, restoring what was there. `--dry-run` prints the actions and the
total in ms and frames.

**hyprpad's grips.** hyprpad drives a Steam Controller, whose grips (L4, L5, R4, R5) it can map
onto pad buttons per game (`pad_map` in `~/.config/hypr/hyprpad.lua`, e.g. AC Origins'
`r4 = a, r5 = b, l4 = dpad_up, l5 = dpad_down`). `quiver lab edit N pad.grips=r4=a,r5=b,l4=up,l5=down`
(or `--grips`) lets scripts say `r4`, as a hyprpad player would press it. The rest of a Steam
Controller (trackpads, gyro, Steam Input layouts) isn't modelled.

### 16.4 What the game sees

`quiver lab pad N status` prints, per pad: its name and identity, the host and container nodes,
the hidden check (done again now), the state, event frames sent, rumble from the game, and **which
container processes have the node open** (from `/proc/<pid>/fd`; under Wine that's
`winedevice.exe`, the winebus host; a native game shows itself). Then, while the game runs,
**what a Windows game sees**: `quiver-pad-probe.exe` (`crates/quiver-pad/win/probe.c`, mingw,
x64 and x86, embedded) runs under the instance's own Wine in the running game's wineserver
session and reports XInput slots 0-3 (connected, subtype, state), DirectInput game controllers
(name, VID/PID, axes, buttons, state), HID devices (usage page 1, usage 4/5) and raw input
devices. It's a GUI-subsystem program that never shows a window (a console one would take
gamescope's focus), and it refuses to run when the game's wineserver is gone (it would start a
session of its own). `--no-probe` skips it; `--json` has everything.

### 16.5 Tests

- `cargo test -p quiver-pad`: the event frames for state changes (xpad's Y flip, the hat, only
  what changed), XInput's view of a state, names, phys and their parsing, grip maps; the
  uinput structs and ioctl numbers against the kernel's; sysfs nodes; the shipped rule (the tags
  it removes, the matches, final assignments, its group and the sysusers file); the gid mapping
  under keep-id (host 105000 → 5001, the edges, two ranges); group entries; node checks; the
  rule's precedence; scripts (momentary vs lasting, restoring a held button and an axis, frames
  at 62.5 Hz, errors, grips); the probe's JSON; the host protocol on the wire.
- `cargo test -p quiver-core podman::tests::pads_only_their_own_nodes`: the device args, the
  group, `/run/udev`, never `/dev/input` as a whole, nothing without pads.
- `cargo test -p quiver-cli lab::pad`: commands to scripts, the udev database.
- Opt-in, `crates/quiver-cli/tests/lab_pad.rs` (needs the rule): two hand-made DRM-free
  instances of `crates/quiver-pad/tests/wine/pad_game.c` (a D3D11 window logging XInput slots 0
  and 1 each frame and showing slot 0 as colour) with two pads and one. Pads hidden from the host
  (the test process can't open them); the game sees two pads; the probe through XInput,
  DirectInput, HID and raw input; winedevice has them open; buttons, sticks and triggers reach
  the right slot and the screen; evdev and SDL3 in the container; each container has only its
  own nodes; on the virtual clock a script lands on frames 0, 10, 15, 18, 24, the same in two
  runs; a killed pad host's pads go at once; `down` removes them; a stale pad host is stopped by
  the next `up`. The virtual-clock runs come before the second instance, and the first
  instance's game is killed before the second comes up: the GPU is shared, and the NVIDIA driver
  runs out of channels (`NV_ERR_STATE_IN_USE`, gamescope's `vkCreateDevice` -3) with many lab
  containers up.

### 16.6 Verified live (2026-11-20)

With the rule installed (`/etc/udev/rules.d`, group `quiver-lab-pad` 105000), GE-Proton11-12:

- **Hidden from the host.** The canary and every pad: `root:quiver-lab-pad 0660`, `getfacl`
  shows no user entry, no `CURRENT_TAGS`, `LIBINPUT_IGNORE_DEVICE=1`, and open() as the user
  fails with EACCES (the pad host's check and the test's own). Inside the container the same
  node is `65534:5001 0660` and the container user (groups 1000, 5001) opens it.
- **`lab_pad.rs` passes** (56 s): `pad status` printed "the game sees it as XInput slot 0",
  `winedevice.exe` with `/dev/input/event21` and `event22` open, two XInput slots, two
  DirectInput devices "Controller (XBOX 360 For Windows)" 045e:028e (5 axes, 10 buttons, a
  POV), two HID gamepads `VID_045E&PID_028E&IG_00`, two raw input gamepads; A + RT + left stick up
  showed as bright green with the block moved up; pad 2's B on slot 1 only; evdev
  X down and up (`0x133` 1, 0) and SDL3 listing both pads in the container; the script on frames 0, 10, 15,
  18, 24 in two runs (frames 4427… and 4301…); each container listing only its own nodes and the
  other instance failing to open the first's; teardown.
- **XInput values**, through the probe's watch mode in a session that stayed up: A 0x1000 for
  the 300 ms asked, left stick (-1, 0.5) as lx -32768 ly 16384 (Y up survives), Up+Start 0x0011,
  LT 0.5 → 128, RT 0.25 → 64, LT 1 → 255, right stick (0.5, -0.5) → 16384 / -16384, all 0 at
  rest.
- **The pad host's lifetime.** A container removed: its pads gone within 5 s. A pad host
  killed with SIGKILL: its pads gone at once.
- **A real game.** Alan Wake Remastered (Epic) in an instance of its own (`pad-awr`, made with
  `quiver lab create … --pads`, removed after):
  - `pad status`: "the game sees it as XInput slot 0", with `winedevice.exe` holding
    `/dev/input/event21`;
  - `pad press a` during the intro cinematic skipped to the title screen;
  - `press a` again opened the main menu, now with Xbox prompts ("Ⓐ Select");
  - `press down` moved the selection from New Game to Episodes;
  - `stick left 0 -1 --for 250ms` moved it on to Options.
- **Under load.** The first AWR launches showed AWR's "Your GPU must support DX12 with feature
  level 12_0" box: with several lab instances up, the NVIDIA driver had no channel left for
  vkd3d's device. The third relaunch, a few minutes later, got past it. Once, under the same
  load, a `pad` command timed out on the pad host: it checked the container with `podman
  inspect` on the thread that serves requests, and podman was slow. That check now runs on its
  own thread.

### 16.7 Not covered

- **Without the rule, no pads**: installing it is root's (a system package does it; a per-user
  install prints the commands).
- **Root processes on the host** can open any node (root bypasses the group). A root daemon
  that grabs every joystick by udev (InputPlumber, xboxdrv, a root `antimicrox`) would see lab
  pads; the rule clears `ID_INPUT_JOYSTICK` so udev-matching ones don't, but one that scans
  `/dev/input` itself would. None runs on this machine (hyprpad's broker opens only the puck).
- **Hotplug**: pads exist for the container's whole life (podman can't add a device to a
  running container), so a game can't be shown a pad being plugged in or out.
- **Not an Xbox One/Series pad**: no share button, no impulse triggers, no hidraw (uinput makes
  no hidraw node, so SDL's and Wine's HIDAPI drivers and games reading raw HID reports of a real
  pad don't apply). Windows.Gaming.Input wasn't probed (it's built on the same HID device in
  Wine). Rumble is recorded, not felt.
- **Steam Input**: a Steam Controller's trackpads, gyro and Steam's per-game layouts aren't
  modelled; only hyprpad's grip map.
- **Input on a thread the clock doesn't hold** sees changes during the settle, before the next
  step; its simulation still only gets time one frame per step (§8.2).

## 17. Gaps Pharos found moving to the lab (2026-11-20)

Pharos (the AC Origins DLSS/stereo mod) moved its testing from its own harness onto `quiver
lab` and reported these, most important first. Each is fixed and tested where it lives:

1. **The virtual clock dropped another module's DXGI hooks**: re-hooking a factory vtable
   over Pharos's `d3d11.dll` proxy bypassed it (§8.1, "Other modules' hooks stay in the
   chain"). Proved by the registry's unit tests and the Wine test `d3d11chain`, which fails
   with the old DLL; not run live in Origins (that needs Pharos's build in an Origins instance).
2. **`log_file` couldn't name the run folder**: `run:<file>` (§2).
3. **locations.txt's `# comments` reached the mod** as arguments (§6).
4. **teleport couldn't turn the camera**: `teleport` turns it to the mod's `want` in a closed
   loop on `where`, and `quiver lab turn` does it alone (§6). Proved against a simulated camera
   with Origins' gains; not run live.
5. **`where` had a fixed 15 s timeout and options after `call` arguments went to the mod**:
   `--timeout` everywhere, options after arguments, `--` for the rest (§6).
6. **`!!! OUTSIDE …:443` on every Ubisoft run**: only runs the EOS proxy watches get it (§3.1).
7. **No fixture import**: `quiver lab fixture import` (§8).

## 18. Owners (2026-11-21)

Several sessions (Claude Code, Codex, muse, the person in a terminal) share one lab. quiverd once
needed a restart while the instance `origins` held it; the main session asked the Claude
session `pharos` to take it down, and pharos did, but it was a **Codex** thread's
(`pharos-xreal`), and nothing said so. Now every instance and every run records who started it.

**Before taking down (or killing, or relaunching) an instance you didn't start, check its owner
in `quiver lab list` and ask that session or the person.** A `(gone)` owner (its process has
exited) is a strong hint that the instance is abandoned.

**What's recorded** (`quiver_core::lab::owner`): `<instance>/owner.json`, written by `quiver
lab up`, and each run's owner, written by the supervisor from the `launch` request into
`state.json`'s run and `lab/runs/r<N>/owner.json` (runs come from different sessions, and
`state.json` holds only the current one). An owner is the host `agent` (`claude`, `codex`,
`muse`, `tmux`, `kitty`, `terminal`, `unknown`), its `session` name and/or `session_id`, the
host's `pid` with its start time (so a reused pid doesn't pass for it), its `tty`, the
`--source` label when it isn't the default `cli`, an explicit `label`, and when.

**How it's found**: `quiver`'s ancestry in `/proc`, nearest agent host first, then the terminal.
Only argv and a fixed list of this process's own environment keys are read (`owner::ENV_KEYS`:
ids and labels; another process's `environ` is never opened, and nothing secret is recorded).

| Host | What identifies it |
|---|---|
| Claude Code | a `claude` ancestor; its `--resume <name>` is the session name (`--resume` alone or a uuid: no name, the id from `CLAUDE_CODE_SESSION_ID` stands in). Detached: `CLAUDE_PID`, `CLAUDECODE=1`. |
| Codex | commands run under one shared `codex app-server … --managed-daemon` serving every Codex TUI, so the TUI above it is whichever session started the daemon (here the `pharos` thread, for `pharos-xreal`'s commands). The thread is `CODEX_THREAD_ID`, else a TUI's `codex resume <id>` when the command runs right under it; its name is the last `thread_name` for it in `$CODEX_HOME/session_index.jsonl`. The TUI's pid is recorded only if one was resumed on that thread. |
| muse | a `muse resume <id>` ancestor (the id; muse keeps names in a database Quiver doesn't read). |
| terminal | `TMUX_PANE` (the pane's shell is the pid), a kitty window (`KITTY_WINDOW_ID`), else the outermost process on a tty. |

`quiver lab up --owner NAME`, `launch --owner NAME` or `QUIVER_LAB_OWNER=NAME` name it outright.
A `--bg` job hands the foreground's owner to its detached process (`QUIVER_LAB_OWNER_DETECTED`).
`quiver lab whoami [--json]` prints what would be recorded from here.

**Shown as** `codex:pharos-xreal`, `claude:pharos/agent:pharos-lab` (session, then the
`--source`), `tmux:%3`, `terminal`:

- `quiver lab list`: an owner column (the current run's owner while it runs, else who brought
  the instance up), `(gone)` when its process has exited; `--json` has `owner` (with `name` and
  `alive`) and `instance_owner`.
- `quiver lab status N`: the instance's owner and the current run's (`by …`), each with its
  pid and whether it's alive, its tty and when; the earlier runs' owners. `--json`: `owner`,
  `state.run.owner`, `runs`.
- `quiver lab up` on an instance that's already up says whose it is; `down` notes it when it
  isn't yours.
- quiverd's `quiver daemon status`: a lab instance's play session is "waiting on: Assassin's
  Creed® Origins (lab origins, codex:pharos-xreal)", from the lab folder (the instances running
  that game, their runs' owners).

**Status after `down`.** `quiver lab status` printed "container up" after `down` said "origins is
down" and podman listed nothing: `down` stops the supervisor before the container, the
supervisor's quit changed nothing in its state so it never rewrote `state.json`, and its watch
loop (which notices a stopped container) was gone with it; `status` printed the file. Now `down`
waits for the supervisor to stop, removes the container, and writes the state itself
(`container_up: false`, a run still going ends `container stopped`); `status`, `list` and
`show` check the file against podman (`State::reconcile`), and `wait` does whenever no
supervisor holds the instance, so it fails at once instead of waiting out its timeout.

Tests: `lab::owner` (ancestry over fake `/proc` trees for each host, including the shared Codex
app-server; pid reuse; the environment allowlist; `owner.json` and the running-instances word),
`lab::ready` (`a_stale_state_follows_podman`), the CLI's `status_after_down_is_down` and
`owners_in_the_table_and_runs`, the supervisor request's round trip, and quiverd's
`lab_holders_name_the_instance_and_its_owner`.

## 19. Asking lab agents to wrap up for a quiverd update (2026-11-21)

quiverd restarts onto a newly installed build once nothing needs it (design/SERVICE.md), and a
lab run can hold that up for hours while the agent that started it works on. Agents (Claude Code
sessions, Codex threads, muse) can't be sent a message, but they run `quiver` constantly, so the
request rides on their own commands.

**If quiver tells you an update is waiting on your instance, finish your current step, save
what you need, and stop the run (`quiver lab kill <i>`), then launch it again after the
restart; use a time-limited hold only if you truly need longer.** Nothing is ever forced: no
kill, no auto-down.

**What holds the restart.** Only a run's **bridge**: a Ubisoft game's `quiver-bridge` in the
instance keeps a TCP connection to quiverd's broker (its play session, held as `lab`) for as
long as the game runs. The container does not (pasta forwards the broker port per connection,
it opens none of its own), the supervisor does not (it asks quiverd for the licence once per
launch and lets go), a lab `quiver-sight` never talks to quiverd (§12), and Epic and DRM-free
games have no bridge at all. So:

| State | Holds the restart? |
|---|---|
| a Ubisoft run going (any phase before `exited`) | yes |
| paused or stepping on the virtual clock (`lab clock … pause`) | yes: the game, and its bridge, are still there |
| the run killed (`quiver lab kill`), container up | no |
| the instance down | no |
| an Epic or DRM-free run | no |

So the hint asks for `quiver lab kill <i>`, not `down`: the container can stay up and the next
`launch` after the restart is quick. Verified against a scratch daemon (below): with a lab
bridge connected the update waits; when the connection closes quiverd restarts.

**Which instances.** The broker now notes each lab bridge's session (`lab-<instance>-r<run>`,
from its `play_session` request) while its connection lasts (`Broker::lab_sessions`), and
quiverd maps them to instances and their runs' owners (`owner::blocking`: the run's
`owner.json`, else `state.json`'s run, else the instance's). Before, "waiting on" matched lab
play sessions to instances by game and `state.json`, which named instances whose state file was
stale: on 2026-11-21 it listed `xreal-origins` (down, its run exited) next to the
`pharos-aim-verify` run that really held quiverd. A lab play session no connected bridge
explains (an older bridge that doesn't name its session) still falls back to that match.

**What quiverd reports.** `quiver daemon status --json` (and `quiver lab status N --json`) has
`restart_waiting` while a staged update would restart but something is in its way (none when
auto-restart is off, a restart is under way, or the exec onto this build failed):

```json
"restart_waiting": {
  "since": "2026-11-21T19:15:35Z", "waiting_secs": 754, "version": "0.1.0.r622.g5b94e68",
  "waiting_on": ["Assassin's Creed® Origins (lab pharos-content-loader, codex:01a1ff7d/agent:codex-ports)"],
  "lab": [{ "instance": "pharos-content-loader", "game": "Assassin's Creed® Origins", "run": 7,
            "session": "lab-pharos-content-loader-r7", "owner": { "agent": "codex", "session_id": "01a1ff7d-…",
            "source": "agent:codex-ports", "name": "codex:01a1ff7d/agent:codex-ports", "alive": null, … },
            "free_with": "quiver lab kill pharos-content-loader", "asked": "2026-11-21T19:27:02Z" }]
}
```

`since` is when the first of the builds staged since quiverd started was noticed (a newer build
on top doesn't restart the clock). `quiver lab status N --json` adds `holds_restart`, and its
text says "holds up a quiverd update (waiting 12m; run by …): … `quiver lab kill N` frees it".

**The hint.** Every `quiver` command (one hook in `main`, `quiver_core::lab::hint`) whose caller
owns a blocking run prints one line on **stderr**; stdout, and so `--json`, is untouched:

> quiver: a quiverd update is waiting on your lab instance "pharos-content-loader" (12m). At a
> good stopping point, save what you need and stop the run with `quiver lab kill
> pharos-content-loader` (the container can stay up; pausing the clock doesn't free it), then
> launch it again once quiverd has restarted (`quiver daemon status`). Need longer? `quiver
> daemon hold --for 30m --reason "…"` (it holds the restart and stops these reminders while it
> lasts).

After 30 minutes it reads "has been waiting 47m on your lab instance … Please wrap up soon:"; after
60, "has been waiting 1h 12m …, and the person is waiting for it. Unless you're mid-step, please
wrap up now:". Lab commands (only) also get a neutral line about blockers that aren't the
caller's but can't be told apart, or whose owner has exited:

> quiver: a quiverd update is waiting (8m) on lab instances: xreal-origins
> (codex:pharos-xreal/agent:xreal-recorder). If one is yours, stop its run at a good point
> (`quiver lab kill NAME`) and launch it again after the restart; if not, leave it to its owner
> or the person.

> quiver: a quiverd update is waiting (20m) on lab instance "awr-rt", whose owner
> (claude:36696674/agent:torchbearer-rt) has exited; it may be abandoned. Don't stop an instance
> you didn't start without asking: mention it to the person.

- **"Yours"** (`owner::same_session`, the `--source` label aside): an explicit label (`--owner`,
  `QUIVER_LAB_OWNER`) one side gave that the other has or is named by; else different hosts are
  different sessions; else the same session id (Claude's `CLAUDE_CODE_SESSION_ID`, Codex's
  thread, muse's session), else the same host process (pid and start time), else the same
  session name. A Codex command without its thread, or an `unknown` host: unsure, which gets
  the neutral line. A run whose owner is another live (or uncheckable) session gets nothing:
  its own commands get the hint.
- **How often:** once per command, at most once per 5 minutes per session (`Owner::key`;
  `$XDG_RUNTIME_DIR/quiver/hints.json`, which also caches quiverd's answer for 20 s so a burst
  of commands asks once; `lab kill`, `down` and `rm` drop the cache). `QUIVER_NO_HINTS=1` turns
  it off. Never on `quiver daemon …` (it shows all this itself), `--machine`, `steam-run`, the
  supervisor and pad host, or `queue --no-start` (status bars).
- **A hold** taken with `quiver daemon hold` records the caller's owner (`Hold::owner`); while
  it lasts that session isn't reminded. It still holds the restart, so it's for "I need 30 more
  minutes", not a way around stopping. Holds already took `--for` (capped at 24 h).
- **The person's view.** After printing a hint for the caller's own instances, quiver tells
  quiverd (`DaemonRequest::Hinted`; the first time per instance also goes in the activity log:
  "asked codex:01a1ff7d to wrap up lab … so quiverd can restart onto the new build"), and
  `quiver daemon status` says, under "waiting on":

  ```
    lab agents have been asked to wrap up: pharos-content-loader r7, codex:01a1ff7d/agent:codex-ports, asked 3m ago
    (stopping a run frees it: quiver lab kill NAME; the container may stay up)
  ```

  or "lab agents will be asked to wrap up on their next quiver command: …" before any was. The
  app and the Omarchy widget have no "update waiting" display (the app shows only the restart
  itself, `Event::DaemonRestarting`), so this is the person's view for now.

Tests: `lab::hint` (same Codex thread with another `--source`, another thread, the same and
another Claude session, a label naming the thread, an owner gone or unknown, the neutral line,
the wording at 12, 47 and 72 minutes, holds; the rate limiter and its file; owner keys),
`lab::owner::connected_sessions_name_their_instance_and_run_owner`, quiverd's
`restart_waiting_on_lab_instances`, and the CLI's `lab_hint` end to end against a scratch
quiverd and lab folder: a fake bridge under `lab-hinttest-r7`, a build installed over the
daemon, `quiver lab list --json` as the owner (one stderr line, stdout byte for byte the same),
as another thread and with `QUIVER_NO_HINTS=1` (nothing), the rate limit, `asked` and the
activity entry, a hold quieting it, and the bridge closing letting quiverd restart.
