# Installing Quiver: the parts that need root (2026-11-21)

Quiver installs per user (`tools/install-user.sh` → `~/.local`) or as a system package
(`tools/package.sh` → a PKGBUILD; `packaging/layout.sh` is shared by both). Most of it needs no
privileges. A growing list of features needs a few system files. Today the person installs each
one by hand from a pasted `sudo` command. This doc replaces that with one reviewed, repeatable
step, and with Quiver saying clearly what's missing and why.

## 1. What needs root

| Piece | Needed by | Kind | Today |
|---|---|---|---|
| group `quiver-lab-pad` (gid 105000) + `72-quiver-lab-pad.rules` | lab controllers (LAB.md §16) | sysusers.d + udev rule | package installs it; per-user install: by hand |
| group `quiver-stream-pad` (gid 105001) + `72-quiver-stream-pad.rules` | stream controllers (REMOTE.md) | sysusers.d + udev rule | by hand |
| `/dev/uinput` access with no seat | pads on a headless server (a seated user gets it through uaccess) | udev rule, group-scoped (§7) | `72-quiver-uinput.rules`, group `quiver-uinput` (105002) |
| firewall: Moonlight (TCP 47984, 47989, 48010; UDP 47998–48000; UDP 5353 mDNS) and Quiver remote (TCP 47990; UDP 47998+) | phones/TVs and other machines reaching a server | ufw application profile / firewalld service | `ufw allow …` by hand |
| lingering | quiverd at boot on a headless server, without a login | `loginctl enable-linger` | not built (may need polkit or root here) |

Everything else stays per-user and needs no root: the binaries, the Sight layer, the user unit
(`~/.config/systemd/user`), and the data under `~/.local/share/quiver`.

**Rules that don't change:**
- Never sudoers entries, polkit rules or setuid helpers.
- Quiver never escalates by itself.
- A person runs the one privileged step, after seeing exactly what it does.
- Agents never run it.

## 2. One source of truth: real system files

Every privileged piece is a static file in `contrib/`, installed to the place a distribution
package would use:

- `/usr/lib/sysusers.d/quiver.conf`: the groups (`quiver-lab-pad` 105000, `quiver-stream-pad`
  105001, `quiver-uinput` 105002), with fixed gids. It replaces the per-feature
  `quiver-lab-pad.sysusers` and `quiver-stream-pad.sysusers`. The gids matter, because they
  map into rootless podman's subgid range (LAB.md §16) and the stream sandbox's.
- `/usr/lib/udev/rules.d/72-quiver-lab-pad.rules` and `72-quiver-stream-pad.rules`, plus a
  headless `uinput` rule if needed. Each carries its marker line ("quiver-…-pad rule vN"), so Quiver
  can recognise installed versions, including copies the person installed by hand under `/etc`.
- `/etc/ufw/applications.d/quiver`: ufw application profiles "Quiver Moonlight" and "Quiver
  Remote". `/usr/lib/firewalld/services/quiver-moonlight.xml` and `quiver-remote.xml` for
  firewalld. The ports live in one place, so a rule is `ufw allow from 192.168.50.0/24 to any app
  "Quiver Moonlight"`, or a checkbox in a firewall GUI.
- The user unit `quiverd.service` (already in `contrib/`), under the system package's
  `/usr/lib/systemd/user` or the per-user `~/.config/systemd/user`.

## 3. Packaged installs need nothing extra

The PKGBUILD installs §2's files. pacman's hooks already run `systemd-sysusers` and reload udev,
so groups and rules are live after `pacman -U`. Firewall rules are not opened by the package:
which networks to allow is the person's call, so the profiles are installed but not enabled
(see §4). The post-install message points to `quiver system setup --firewall`.

## 4. Per-user installs: `quiver system setup`

```
quiver system setup [--check] [--firewall lan,tailnet] [--at-boot] [--undo] [--print]
```

- `--check` (also run by `quiver doctor` and the app) lists each §1 piece as **ok / missing /
  outdated (vN < vM) / not needed here**, with the feature it unlocks.
- With no flags, it writes **one shell script**, which installs exactly the missing or outdated
  pieces. Each line has a comment. The script is idempotent and copies files byte for byte from the
  installed Quiver, never generated on the fly. It shows the script and asks `Run it with sudo?
  [y/N]`, then runs `sudo sh <script>`, so there is one sudo prompt for everything. `--print`
  only prints the script.
- `--firewall lan,tailnet` adds firewall lines for the detected firewall (ufw, firewalld, or
  none), allowing the Quiver profiles **from the LAN subnet and `tailscale0` only, never from
  anywhere**. Ports are opened only for features that are on.
- `--at-boot` adds `loginctl enable-linger $USER` where that needs privileges, and enables the
  user unit (no root needed).
- `--undo` removes what setup installed, recognised by marker: the rules, the groups if they are
  empty, and the firewall rules it added (which it tags with a comment).
- The script is saved to `~/.local/state/quiver/system-setup-<date>.sh`, for the record.

Rules already installed by hand (the two pad rules, today) are recognised by their marker and
left alone when they're current.

## 5. Saying what's missing

- **`quiver doctor`** gains a "System setup" section from `--check`.
- **The app:** Settings → About → System setup lists the same, with plain reasons ("Stream
  controllers need a udev rule and a group so games can open them while your desktop can't").
  A **Set up…** button opens a terminal running `quiver system setup`. The app never asks for a
  password itself.
- **At the point of need:**
  - turning Moonlight on with a firewall active and its profile not allowed → an inline notice
    with the command;
  - a stream that wanted pads, without the rule → the host's existing `Notice`, now naming
    `quiver system setup`;
  - `quiver remote enable --at-boot` without linger → the same.
- **Agents** see `--check`'s output in the lab brief and in `quiver doctor --json`. They report
  missing pieces to the person and never run setup themselves.

## 6. Order

1. The static files in `contrib/` (sysusers, the uinput rule if needed, the ufw and firewalld
   profiles), with layout.sh installing them in system packages.
2. `quiver system setup` (check, script, run, undo), plus doctor.
3. The app's System setup page and the inline notices.
4. A headless-server guide in REMOTE.md: install, `quiver system setup --firewall lan,tailnet
   --at-boot`, `quiver remote enable`, `quiver remote pair`.

## 7. `/dev/uinput` with nobody seated

**How a seatless user gets uinput today: they don't.** On this laptop `/dev/uinput` is
`root:root 0660` plus `user:ajg:rw-`. The ACL comes from Steam's `60-steam-input.rules`
(package `steam-devices`: `KERNEL=="uinput", SUBSYSTEM=="misc", TAG+="uaccess",
OPTIONS+="static_node=uinput"`), and logind turns `uaccess` into an ACL for the *active session
on a seat* only. systemd's own rules give uinput no group. A user who is signed in only over
SSH, or a quiverd started at boot with nobody signed in, has no seat session, so no ACL, and
the stream host can't make a single pad (`open /dev/uinput: permission denied`). Hidden pads
need it too: the stream host opens `/dev/uinput` itself, outside the sandbox.

**The rule** (`contrib/udev/72-quiver-uinput.rules`, one line):
`KERNEL=="uinput", SUBSYSTEM=="misc", GROUP="quiver-uinput", MODE="0660", OPTIONS+="static_node=uinput"`.
Nothing else: no `uaccess` (the seat's ACL stays as it is), nothing for "other", no owner.

**Who gets the group: nobody, as with the pad groups.** `quiver-uinput` is gid 105002, inside
the person's subordinate gid range. When opening `/dev/uinput` directly fails with EACCES and
the node has that group, the stream host runs one helper,
`unshare --user --map-users=U:U:1 --map-groups=G:G:1 --map-groups=105002:105002:1
--setgroups=allow --keep-caps quiver-stream-host --uinput-open 105002`, the same newgidmap path
a stream's program uses for its pad group. The helper takes the group, opens `/dev/uinput`,
drops every capability, sends the descriptor back over a socket (SCM_RIGHTS) and exits; the
host makes its pads on that descriptor (`Pad::create_on`).

**Why not a group the person joins.** Membership would give every process of theirs, every
game and script, the power to make keyboards and mice at any time, and it needs a re-login (and
a restart of a lingering user manager) to take effect. With the namespace, nothing holds the
group except for the length of one `open`, and no account changes.

**Why not `quiver-stream-pad`.** A stream's program runs with that group so it can open the
stream's pads. If it also opened `/dev/uinput`, a game could make input devices of its own.
Keeping the groups apart keeps the sandbox's one power to "open these pads".

**Limits.** The person can always map the group themselves (their subgid range allows it),
as they can the pad groups; the point is that nothing has it ambiently. A lab container maps
the whole subgid range, but it never has a `/dev/uinput` node, and a rootless container can't
make one. udev warns that a non-system group owning a device node is deprecated ("not a system
group"); it says the same about the two pad groups, which must stay inside the subgid range,
and would need revisiting together if systemd drops the support. The lab's pad host still
opens `/dev/uinput` directly, so lab pads on a seatless machine aren't covered yet.

## 8. As built (2026-11-21)

**Files.** `contrib/sysusers/quiver.conf`, `contrib/udev/72-quiver-uinput.rules`,
`contrib/ufw/quiver` (profiles "Quiver Moonlight": `47984,47989,48010/tcp|47998:48000/udp|5353/udp`;
"Quiver Remote": `47990/tcp|47998:48061/udp`, the 64 media ports `streams.rs` tries from
`remote::STREAM_PORT`, now `remote::STREAM_PORTS`), `contrib/firewalld/quiver-moonlight.xml`
and `quiver-remote.xml`. Each carries a marker line with a version. `packaging/layout.sh` copies
all of them, the two pad rules included, to `$LIBDIR/contrib/` (`~/.local/lib/quiver/contrib`,
`/usr/lib/quiver/contrib`) on every install, and for packages also to `UDEVDIR`, `SYSUSERSDIR`,
`UFWDIR` (`/etc/ufw/applications.d`) and `FIREWALLDDIR` (`/usr/lib/firewalld/services`).

**The check** (`quiver_api::system::check`, no root, nothing changed). It reads files under an
injectable root (`/etc`, `/usr/lib`, `/run/udev`, `/var/lib`), and the rest comes in `Facts`:
interfaces (`getifaddrs`, virtual by `/sys/class/net`), `systemctl is-active` for firewalld and
nftables, ufw's own `ENABLED=yes`, logind sessions, `/dev/uinput`'s group, polkit's answer for
`set-self-linger` (`pkcheck`, no prompt), whether `quiverd.service` is enabled, and
config.json's `remote` section. Pieces: lab controllers, stream controllers, controllers with
nobody seated, firewall profiles, Moonlight's and remote play's firewall rules, start at boot.
Each is ok, missing, outdated, not needed here, can't tell, or conflict:

- **Rules installed by hand count.** A rule is found the way udev finds it (`/etc`, `/run`,
  `/usr/lib`, `/lib`, first wins) and judged by its marker's version, not its bytes, so this
  laptop's `/etc/udev/rules.d/72-quiver-{lab,stream}-pad.rules` are ok ("installed by hand; its
  comments differ from this build's"). An older version is outdated (replaced where it is in
  `/etc`); a file of that name without the marker is a conflict, left alone. A group must exist,
  have no members, not be one of the person's, and sit in their subgid range.
- **Not needed here.** Stream pieces when neither remote play nor Moonlight is on; the uinput
  rule when the person is seated and quiverd doesn't start at boot; firewall pieces when no
  firewall runs or the server is off; lingering unless `--at-boot` or the unit is enabled.

**Firewalls.** Detected as ufw, firewalld, nftables-only (its own service), or none. Rules are
for the LAN subnets of the up, non-virtual interfaces (IPv4, and IPv6 ULA/global prefixes, not
link-local) and the tailnet (`tailscale0`, both families; firewalld matches Tailscale's ranges
100.64.0.0/10 and fd7a:115c:a1e0::/48), only for servers that are on, and only the tailnet for
a server bound to it alone. Never from anywhere: `--firewall any` is refused.
- **ufw:** `/etc/ufw/user.rules` and `user6.rules` are world-readable on Arch, so the check
  reads ufw's `### tuple ###` lines and asks, port by port, whether an `allow … in` rule covers
  it from the scope (its source contains the subnet, its interface matches or is unset; a
  wider subnet counts, "from anywhere" counts and is named). This laptop's own rules
  (`allow from 192.168.50.0/24 to any port 47984,47989,48010 proto tcp`, the UDP and mDNS ones,
  and `allow in on tailscale0 …` for both families, no comment) make Moonlight ok, so nothing
  is proposed for it; remote play is missing TCP 47990 and UDP 48001–48061 from both. Where the
  files aren't readable, the check says "can't tell without root", and the script asks first:
  `ufw_has SPEC WHERE` reads `ufw status` (as root) for the profile's port groups from that
  subnet or interface before `ufw allow … app 'Quiver Remote' comment quiver`. ufw itself also
  skips a rule identical to one it has.
- **firewalld:** the permanent zone files (`/etc/firewalld/zones`), rich rules naming Quiver's
  services with a source, or the service or ports zone-wide. The script adds
  `rule family=… source address=… service name="quiver-…" accept` to the interface's zone (or the
  default zone), each after `--query-rich-rule`.
- **nftables alone:** Quiver doesn't edit hand-written rulesets. The check and the script say
  which ports to allow from where.
- **Tags.** Every ufw rule setup adds has the comment `quiver`; firewalld rules are Quiver's by
  their service names. Rules the person added (no tag) are never touched by `--undo`.

**The script** (`system::script`). One `sh` script, every step commented and checked first:
`put REL DEST SHA256` verifies the installed copy against the hash this build ships, then
installs it unless the destination already is that file; groups through `systemd-sysusers` on
`/etc/sysusers.d/quiver.conf`, only the missing ones; `udevadm control --reload` when a rule
changed and `udevadm trigger --action=change` on `/sys/devices/virtual/misc/uinput` for the
uinput rule; the firewall lines; `loginctl enable-linger` only where polkit won't let the
person. Everything it installs goes in `/var/lib/quiver/system-setup.list` (`file`, `group`,
`linger` lines). `QUIVER_SETUP_ROOT` points every path at a fake root; without it the script
refuses to run as anyone but root. `quiver system setup` prints it in full, saves it to
`~/.local/state/quiver/system-setup-<date>.sh`, and asks `Run it with sudo? [y/N]`; not on a
terminal, it says to run `sudo sh <path>` instead. `--at-boot` then sends
`Request::RemoteBoot { enabled: true }` (the unit, and lingering where no root is needed).
Agents (`--source` other than the person's) are refused; `--check` and `--print` work for
them.

**Undo** removes the files in the list (only while each still carries its marker), the groups
setup made (unless a remaining rule names one, or it has members), ufw rules tagged `quiver`
(`ufw status numbered`, last first), firewalld rich rules naming Quiver's services, lingering
if setup turned it on, and the list. Pieces installed by hand are named in its header as left
alone.

**Where it shows.** `quiver doctor [--json]` (the build, quiverd, `system_setup`, and the agent
rule); Settings → About → System setup (each piece with a plain reason; **Set up…** opens
`xdg-terminal-exec`, `$TERMINAL -e`, or a known terminal running `quiver system setup`, with
`--firewall lan,tailnet` when a firewall piece is missing, and checks again once it closes);
the Moonlight card and `quiver remote moonlight enable` (one line when the firewall may block
it); the stream host's pad notices and `quiver lab doctor`'s install hint; `quiver remote boot
on` when lingering needs root; the lab brief's "System setup" section and the agent skill.

**Tests.** `cargo test -p quiver-api system::`: shipped files (markers, groups, profiles'
ports against the code's); ufw tuples, coverage (this laptop's rules verbatim, anywhere rules,
other subnets and interfaces, deny and out rules), scopes, firewalld zones; the check against
fake roots (this laptop; outdated, foreign and package copies; members and gids outside the
range; what's needed by feature; unreadable ufw rules); golden scripts
(`src/system/golden/*.sh`, `UPDATE_GOLDEN=1` to accept); the install script run twice against a
fake root with fake `getent`, `systemd-sysusers`, `udevadm`, `ufw`, `groupdel` and `loginctl`
(the second run changes nothing), then undone (the person's rule, group and untagged ufw rule
stay); a group a remaining rule uses is kept; a changed source file stops the script; without
a fake root it wants root. `quiver-stream-host`: the helper's namespace and a descriptor across
the socket. `quiver-app`: terminal choice and the setup command.

**Not done.** The lab's pad host doesn't use the uinput helper yet (lab pads on a seatless
machine). firewalld and the helper weren't run live (no firewalld here; the rule isn't
installed, and nothing here may run as root). Explicit-port firewalld rules for non-default
ports are left to the person (rich rules can't carry a tag). A remote listener bound to one LAN
address still gets rules for every LAN subnet.
