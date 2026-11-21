# quiver-netlab

Network conditions for testing remote play on one machine (design/REMOTE.md §8.1). The kernel's
own qdiscs (netem, tbf, fq_codel, cake) shape a link between a server namespace and a client
namespace. There's no root, no sudo, and the host's network is never touched.

```
quiver-netlab list                                   # profiles and scenarios
quiver-netlab verify --profile all                   # measure every profile against its model
quiver-netlab run --profile lte --server '<cmd>' --client '<cmd>' [--duration 30] [--out DIR] [--report r.json]
quiver-netlab run --scenario step-down --fake        # the stand-in stream on both sides
quiver-netlab matrix --profiles all --scenarios all --server '<cmd>' --client '<cmd>' [--duration 20]
quiver-netlab shell --profile wifi-congested [--side server] [-- cmd …]
quiver-netlab set --profile lte | --patch 'to_client.rate_mbit = 20' | --roam | --outage 500 | --show
quiver-netlab exec [--lab PID] --side server -- cmd …
quiver-netlab fake-stream serve [--aimd] | play      # the stand-in workloads
```

## The lab

```
 server ns                 router ns                      client ns
 eth0 10.77.0.2  ──veth──  srv 10.77.0.1 │ cli 10.77.1.1 ──veth──  eth0 10.77.1.2
      fd77::2                  fd77::1     fd77:0:1::1               fd77:0:1::2
                    ◄── to_server shaping   to_client shaping ──►
```

- **Three network namespaces in one user namespace.** The person's uid is root inside, so
  `ip` and `tc` work, but only on the lab's own namespaces. The shaping sits on the router's
  egress interfaces rather than the endpoints'. A qdisc on the sender's own interface holds
  packets that still count against the sending socket, so a UDP sender would block on its
  send buffer instead of seeing loss. The router orphans each packet from its socket.
- **Workloads run as the person.** Each one enters the lab and then unshares a nested user
  namespace that maps the person's uid and gid back to themselves. Wine, Steam and gamescope
  see their real uid, and D-Bus's uid check passes. The workload has no capability over the
  lab, so it can't change the shaping, and it keeps its supplementary groups (`/dev/dri`,
  `/dev/uinput`).
- **Only the network namespace changes.** The mount namespace is the host's, so
  filesystem sockets work: Wayland, PipeWire, the session bus, quiverd, and X11 through
  `/tmp/.X11-unix`. Abstract unix sockets don't cross, and nothing inside has a route out.
  `tests/lab.rs` checks for leaks.
- **Lifetime.** The lab is four fds held by the process that made it, with nothing mounted
  (no `/run/netns`). When that process exits, normally or by crashing, the namespaces go with
  it. Workloads run in their own process groups: they get SIGTERM and then SIGKILL at the
  end, and `PR_SET_PDEATHSIG` covers a crash.
- **GSO is off** on every veth (`gso_max_segs 1`), so netem counts, drops and delays real
  packets, not 64 KiB super-packets. Neighbours are static, so no ARP crosses the shaped link.

`shell` publishes the lab at `$XDG_RUNTIME_DIR/quiver-netlab/<pid>.json`, which `exec` and
`set` find. `ping` works inside.

## Shaping

Each direction is `netem → tbf → (fq_codel)` or `netem → cake`:

| field | meaning |
|---|---|
| `delay_ms`, `jitter_ms`, `distribution` | one-way delay; `normal`, `pareto`, `paretonormal` or `uniform` (tc's tables) |
| `loss = { pct, burst }` | mean loss; `burst` > 1 makes it a Gilbert model (`loss gemodel`) with that mean burst length |
| `duplicate_pct`, `reorder_pct`, `corrupt_pct` | netem's |
| `rate_mbit` | the bottleneck (tbf, or cake's shaper); 0 = unshaped |
| `queue_ms` / `queue_packets` | the bottleneck's FIFO, as time at the rate (bufferbloat is a big one) |
| `aqm` | `fifo`, `fq_codel` or `cake` |
| `wander = { min_mbit, max_mbit, every_ms, step_pct }` | a bounded random walk of the rate (cellular) |
| `slot = { min_ms, max_ms, packets }` | netem slotting: packets leave in bursts |

netem always gets a 10 Gbit/s `rate`. That limits nothing, but it makes netem schedule each
packet after the one ahead of it. Jitter then delays the packets behind a late one, as a Wi-Fi
retry does, instead of reordering them. The effective delay therefore depends on the packet
rate: dense traffic sees close to the top of the distribution. Reordering has its own knob.

## Profiles

Each profile lives in `profiles/<name>.toml` (`[both]`, `[to_client]`, `[to_server]`), with
`sources` that say where the numbers come from. "To client" is server → client, the
direction the video goes.

| profile | RTT (delays) | to client | to server | loss | queue |
|---|---|---|---|---|---|
| lan-wired | 0.2 ms | 1000 | 1000 | 0 | 2 ms |
| wifi6-good | 3 ms, paretonormal ±1 | 700 | 450 | 0.1% | 30 ms |
| wifi-far | 5 ms, paretonormal ±3 | 200 | 100 | 0.3%, bursts of 2 | 50 ms |
| wifi-congested | 6 ms, paretonormal ±8 | 80 | 40 | 1.5%, bursts of 3 | 80 ms |
| 5g | 40 ms ±4 | 150 (60–300) | 30 (10–50) | 0.1%, bursts of 2 | 150 ms |
| lte | 64 ms ±8 | 30 (8–50) | 8 (2–15) | 0.3%, bursts of 3 | 300 ms |
| cafe | 24 ms, paretonormal ±6 | 25 | 6 | 0.5%, bursts of 3 | 400 ms |
| tailscale-relay | 36 ms, paretonormal ±6 | 40 | 40 | 0 (TCP underneath) | 100 ms |
| cross-country | 70 ms ±1.5 | 100 | 100 | 0.05% | 60 ms |
| transatlantic | 80 ms ±2 | 100 | 100 | 0.1% | 60 ms |
| bufferbloat-cable | 18 ms ±1 | 20 | 300 | 0 | 600 ms / 100 ms |
| pathological | 120 ms, paretonormal ±25 | 15 | 5 | 5%, bursts of 5; 1% dup, 1% reorder, 0.1% corrupt | 200 ms |

## Verify

`verify` measures what the shaping delivers and compares it with the profile:

- **Idle.** 1000 small UDP packets a second for 3 s, echoed back. The namespaces share
  `CLOCK_MONOTONIC`, so each direction's one-way delay, loss, duplicates and reordering are
  measured separately.
- **Rate.** A paced UDP flood at up to 2× the rate in each direction. The delivered rate is
  the capacity.
- **Loaded RTT.** A light echo probe runs once the bottleneck queue is full. It measures
  bufferbloat.
- **TCP.** One bulk flow as a reference. It isn't checked.

The expected delays come from a model of netem. The model samples the same distribution tables
tc loads and schedules each packet in order, as netem does. The check is that the kernel does
what the model says.

## Scenarios

Each scenario lives in `scenarios/<name>.toml`. It has a base profile, patches on top of it,
and a list of `[[event]]`s:

| scenario | what happens |
|---|---|
| step-down | rate to client 200 → 20 Mbit/s at 10 s, back at 25 s |
| microwave | 2 s of 30% bursty loss both ways |
| outage | 500 ms of 100% loss both ways |
| roam | the client's address changes, with a 150 ms gap at 10 s and none at 20 s |
| latency-creep | 35 → 120 ms each way over 30 s |
| competing-tcp | a bulk TCP flow shares bufferbloat-cable's 20 Mbit/s for 15 s |
| competing-udp | a 150 Mbit/s UDP flood takes most of wifi-far's 200 for 10 s |

Event kinds are `set` (with `for_s` to revert), `restore`, `outage`, `roam`, `ramp` and
`flood`. The `scenario` module docs describe each one.

## Workloads and stats

`run` starts each side as `sh -c '<cmd>'`, the server first and the client 300 ms later. It
sets this environment:

- `QUIVER_NETLAB_SERVER_ADDR` / `_CLIENT_ADDR` (and `_ADDR6`) and `QUIVER_NETLAB_SIDE`;
- `QUIVER_NETLAB_STATS`, a stats file that is also open on fd 3 (`QUIVER_NETLAB_STATS_FD`);
- `QUIVER_NETLAB_DURATION` and `QUIVER_NETLAB_OUT`.

The run ends at the duration or when the client exits.

To get their numbers into the report, workloads append JSON lines to the stats file:

```json
{"t_us": 81234567890, "client": { "...": "quiver_stream_proto::ClientStats" }, "extra": {"frame_gaps": 1, "recv_kbps": 19500}}
{"t_us": 81234568001, "host": { "...": "quiver_stream_proto::HostStats" }}
```

`t_us` is `quiver_stream_proto::now_micros()`. The `extra` keys the report knows are
`frame_gaps`, `max_gap_ms`, `recv_kbps`, `latency_p99_us` and `target_kbps`.
`quiver_netlab::stats::StatsWriter` writes these lines.

## Reports

`<out>/report.json` holds everything, on the run's own clock (ms from its start):

- the shapes applied over time;
- the router's qdisc counters every 250 ms (carried bytes, drops, queue backlog);
- the netlab's own path-RTT probe;
- roams and competing flows;
- the workloads' stats lines;
- a summary: capture → present p50/p99, gaps per minute, mean bitrate against mean capacity,
  stream loss, path RTT, and recovery after each event.

Recovery is the time to three samples in a row back under max(1.5 × the pre-event median,
+10 ms).

`<out>/report.html` is self-contained: inline SVG plus a few lines of inline script for the
hover readout. It draws bitrate against capacity, latency, the bottleneck's queueing delay,
loss, and frame gaps, with the events marked. `matrix` writes `summary.{json,md,html}` with one
row per profile or scenario, linked to each run's report.

## The stand-in stream

`fake-stream serve` sends `fps` bursts of `bitrate / fps` bytes, stamped with the capture
time. The bitrate is fixed, or AIMD on loss and frame latency. The server sends to wherever the
client's feedback came from last, so it follows a roam. `fake-stream play` reassembles frames
and writes `ClientStats` lines. It has no FEC, so a lost packet loses its frame; on lossy
profiles that is far worse than the real transport. `--fake` on `run` or `matrix` puts it on
both sides, sized to the link.

## Using it from tests

```rust
let Some(lab) = quiver_netlab::test_lab() else { return };   // skips without user namespaces
let mut shaper = quiver_netlab::Shaper::new(&lab);
shaper.apply_profile(&quiver_netlab::profile::load("wifi-congested")?)?;
let server = lab.udp(Ns::Server, "0.0.0.0:9000".parse()?)?;   // sockets in the lab, used here
let child = lab.spawn(Side::Client, Command::new("my-client"))?; // or whole processes
```
