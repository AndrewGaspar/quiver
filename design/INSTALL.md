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
| `/dev/uinput` access with no seat | pads on a headless server (a seated user gets it through uaccess) | udev rule, group-scoped | not built |
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

- `/usr/lib/sysusers.d/quiver.conf`: both groups, with fixed gids. The gids matter, because they
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
