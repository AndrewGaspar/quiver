# Verifying achievement reads and reports live (for the orchestrator)

Everything here goes through the **user's running quiverd** (its demux login). Don't use
`quiver dev …`: dev commands sign in on their own and would replace quiverd's login. Deploy
first (`tools/install-user.sh`, then restart quiverd per the deploy routine), because the
`ach_frontend` code lives in quiverd.

Ground truth: AC Origins, product **3539**, config `uplay.achievements_sync_id: ACE`.
Achievement **59 "Namaste"** was unlocked through Connect on 2026-11-17.

## 1. Raw read (read-only)

```sh
quiver achievements 3539 probe            # ACE / PC, raw framing
```

This prints the request sent (minus the token), the decoded blob and `product.earned`.

- **Pass:** `product.found: true` and `earned` contains `59` with a 2026-11-17 timestamp.
  Note `product.platform_id` and `ts_unit`: quiverd learns them and uses them for writes.
- **`error: … timed out`:** the server ignored the frame. In this order, try:
  - `quiver achievements 3539 probe --framing wrapped`. This sends `Upstream{request}`
    instead of a bare `Req`.
  - `quiver achievements 3539 probe --all-products`. Without a product filter, the reply
    shows the product and platform ids Ubisoft really uses.
  - `quiver achievements 3539 probe --platform <id>`.
- **`ach_frontend refused the orbit token` / AuthenticateReq timed out:** the service wants
  a different token than the UbiServices ticket. That's a finding: stop and report it.
- If only `--framing wrapped` works, the framing needs changing. Either restart quiverd with
  `QUIVER_ACH_FRAMING=wrapped`, or flip the default (`Framing::default` in
  `crates/quiver-core/src/achievements.rs`).

## 2. Merged status (what the CLI, app and bridge see)

```sh
quiver achievements 3539 --refresh
```

- **Pass:** the header reads `N/67 unlocked (from ach_frontend…)` and line 59 shows `✓` with
  its date.
- `legacyPlayerAchievements` (REST) is a second source. Its URL prefix is a guess
  (`/v1/profiles/{userId}/legacyPlayerAchievements?spaceId=`). If it 404s, it only shows
  under "unavailable:" while `ach_frontend` works.
- The game page (Saves & progress) marks 59 as earned.

## 3. Report path, without sending anything

1. Make reports dry runs for the game: `quiver set 3539 achievements=dry-run`.
2. Play Origins through Quiver and load a save. The game re-sends its unlocked set.
   - quiverd's log should show no `achievements dry run: would send` line for already
     earned ones (they dedupe as `AlreadyEarned`).
   - The bridge log shows `achievement 59 earned again (already earned)`.
3. A genuinely new unlock logs `achievements dry run: would send` with the request and its
   bytes, and the journal has it as `dry run (not sent)`. Check with
   `quiver achievements 3539`.
4. Any journaled one can be rebuilt without sending:
   `quiver achievements 3539 report <id> --dry-run`.
   - Check the blob: `product_id: ACE`, the learned platform, and your id with a timestamp
     in the learned unit.

## 4. A real report (only when the user agrees)

1. `quiver set 3539 achievements=` (back to on).
2. `quiver achievements 3539 report <id>` sends a journaled one.
   - Look for `a read-back shows it unlocked`: success.
   - `read-back does NOT show it` means `ach_frontend` accepted the write but ignores it for
     this title. The REST POST path with `Ubi-Signature` would then be needed (see
     `achievements-report-path.md`).
3. To turn reporting off if it misbehaves: `quiver set 3539 achievements=off`.

## Where things are

- Journal: `~/.local/share/quiver/achievements/<user-id>/<product>.json`. It holds Ubisoft's
  last read set, entries with their state, attempts, the last error and `confirmed`.
- Pending reports retry every 10 minutes. They also retry after every successful read and
  every new earn. Attempts that reached Ubisoft count; after 8 the entry becomes `failed`.
  Offline attempts don't count.
