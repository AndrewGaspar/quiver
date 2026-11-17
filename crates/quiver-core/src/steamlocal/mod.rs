//! Read-only parsers for the Steam client's local files, for showing Steam games in the
//! library without signing in to Steam:
//!
//! | file | module | what |
//! |---|---|---|
//! | `appcache/appinfo.vdf` | [`appinfo`] | name, type, OS list, developer, publisher, release date, DLC, review score |
//! | `appcache/stats/UserGameStatsSchema_<appid>.bin`, `UserGameStats_<accountid>_<appid>.bin` | [`stats`] | achievements and the user's unlocks |
//! | `userdata/<accountid>/config/localconfig.vdf` | [`playtime`] | total and two-week play time, last played |
//! | `userdata/<accountid>/<appid>/remotecache.vdf` | [`remotecache`] | Steam Cloud files and their sync state |
//!
//! Everything here only reads, opening files read-only and never locking them. These are
//! the client's caches: any of them may be missing (an app never started here, a fresh
//! install), stale, or in a format a newer client changed. Nothing panics on bad input;
//! parse failures become errors for the binary formats and `None`/empty for the text
//! ones, and callers should treat either as "no data". `account_id` arguments are the
//! `userdata/` directory names (32-bit account ids, as in
//! [`crate::exports::steam::Steam::users`]); anything that isn't all digits reads as
//! missing.

pub mod appinfo;
mod kv;
pub mod playtime;
pub mod remotecache;
pub mod stats;

pub use appinfo::AppInfo;

/// Steamworks Common Redistributables.
const REDIST: u32 = 228980;
/// Steam Linux Runtime 1.0 (scout), 2.0 (soldier), 3.0 (sniper), 4.0.
const RUNTIMES: [u32; 4] = [1070560, 1391110, 1628350, 4183110];
/// Proton: Experimental, 6.3, 7.0, Hotfix, 8.0, 9.0, 10.0, 11.0 (7.0 and 8.0 not
/// installed on the machine these were checked on; the rest match their app manifests).
const PROTONS: [u32; 8] = [1493710, 1580130, 1887720, 2180100, 2348590, 2805730, 3658110, 4628710];

/// Whether an installed app is plumbing rather than something to show in a library:
/// appinfo type `tool` (Proton, Steam Linux Runtime, Steamworks Common Redistributables,
/// SteamVR, dedicated servers…) or `config`. Type `application` (Wallpaper Engine,
/// Blender, …) stays visible, as do games, demos, music and video. The known ids below
/// always count, and without a type (no appinfo cached yet) names decide too.
pub fn is_tool(app_id: u32, info: Option<&AppInfo>, name: &str) -> bool {
    if app_id == REDIST || RUNTIMES.contains(&app_id) || PROTONS.contains(&app_id) {
        return true;
    }
    if let Some(kind) = info.and_then(|i| i.kind.as_deref()) {
        return matches!(kind, "tool" | "config");
    }
    let name = name.trim();
    name.starts_with("Steam Linux Runtime")
        || name.starts_with("Proton ")
        || name == "Proton"
        || name.starts_with("Steamworks Common Redistributables")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(kind: &str) -> AppInfo {
        AppInfo { kind: Some(kind.into()), ..Default::default() }
    }

    #[test]
    fn tools() {
        assert!(is_tool(228980, None, "Steamworks Common Redistributables"));
        assert!(is_tool(1628350, Some(&info("game")), "anything"));
        assert!(is_tool(250820, Some(&info("tool")), "SteamVR"));
        assert!(is_tool(1, Some(&info("config")), "x"));
        assert!(!is_tool(431960, Some(&info("application")), "Wallpaper Engine"));
        assert!(!is_tool(292030, Some(&info("game")), "Proton Something"));
        assert!(!is_tool(292030, None, "The Witcher 3: Wild Hunt"));
        // No appinfo: names.
        assert!(is_tool(9_999_001, None, "Proton 12.0"));
        assert!(is_tool(9_999_002, None, "Proton Experimental"));
        assert!(is_tool(9_999_003, Some(&AppInfo::default()), "Steam Linux Runtime 5.0"));
        assert!(!is_tool(9_999_004, None, "Protonic Rush"));
    }

    /// Runs every reader over this machine's Steam install and prints aggregates only
    /// (never names, ids of accounts, or values from the user's files):
    /// `cargo test -p quiver-core steamlocal::tests::real_files -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_files() {
        use crate::exports::{steam::Steam, steamgames};
        use std::time::Instant;
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());
        let Some(steam) = Steam::find(&home) else { return println!("no Steam") };
        let root = &steam.root;

        let t = Instant::now();
        let buf = std::fs::read(root.join("appcache/appinfo.vdf")).unwrap();
        let read_ms = t.elapsed().as_secs_f64() * 1e3;
        println!("appinfo.vdf: {} bytes, magic {:08x}, read {read_ms:.1} ms", buf.len(), u32::from_le_bytes(buf[..4].try_into().unwrap()));
        let t = Instant::now();
        let all = appinfo::parse(&buf, |_| true).unwrap();
        println!("appinfo all: {} apps in {:.1} ms", all.len(), t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        let none = appinfo::parse(&buf, |_| false).unwrap();
        println!("appinfo skip all: {} apps in {:.2} ms", none.len(), t.elapsed().as_secs_f64() * 1e3);
        let count = |f: &dyn Fn(&AppInfo) -> bool| all.values().filter(|a| f(a)).count();
        println!(
            "  names {} kinds {} oslist {} developer {} publisher {} release {} dlc-lists {} reviews {}",
            count(&|a| a.name.is_some()),
            count(&|a| a.kind.is_some()),
            count(&|a| !a.oslist.is_empty()),
            count(&|a| a.developer.is_some()),
            count(&|a| a.publisher.is_some()),
            count(&|a| a.release.is_some()),
            count(&|a| !a.dlc.is_empty()),
            count(&|a| a.review_percentage.is_some()),
        );
        let mut kinds = std::collections::BTreeMap::new();
        for a in all.values() {
            *kinds.entry(a.kind.clone().unwrap_or_default()).or_insert(0) += 1;
        }
        println!("  kinds {kinds:?}");
        // A ~100 MB file: the app entries repeated, the string table after them.
        let off = i64::from_le_bytes(buf[8..16].try_into().unwrap()) as usize;
        let reps = (100 << 20) / off + 1;
        let mut big = buf[..16].to_vec();
        for _ in 0..reps {
            big.extend_from_slice(&buf[16..off - 4]);
        }
        big.extend_from_slice(&[0; 4]);
        let big_off = big.len() as i64;
        big[8..16].copy_from_slice(&big_off.to_le_bytes());
        big.extend_from_slice(&buf[off..]);
        let some: std::collections::HashSet<u32> = all.keys().copied().take(300).collect();
        let t = Instant::now();
        let n = appinfo::parse(&big, |id| some.contains(&id)).unwrap().len();
        println!("synthetic {} MB ({reps}x): {n} distinct of 300 wanted apps (decoded {reps}x each) in {:.1} ms", big.len() >> 20, t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        appinfo::parse(&big, |_| false).unwrap();
        println!("synthetic, skipping everything: {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
        if std::env::var("QUIVER_APPINFO_SHAPES").is_ok() {
            for (k, n) in appinfo::key_shapes(&buf, |_| true).unwrap() {
                if k.starts_with("/appinfo/common") || k.starts_with("/appinfo/extended") {
                    println!("  {n:5} {k}");
                }
            }
        }

        let mut installed = Vec::new();
        for lib in steamgames::library_folders(&steam) {
            installed.extend(steamgames::app_manifests(&lib));
        }
        let ids: std::collections::HashSet<u32> = installed.iter().map(|a| a.app_id).collect();
        let t = Instant::now();
        let wanted = appinfo::parse(&buf, |id| ids.contains(&id)).unwrap();
        println!("installed: {} manifests, {} in appinfo ({:.1} ms)", installed.len(), wanted.len(), t.elapsed().as_secs_f64() * 1e3);
        let tools: Vec<_> = installed.iter().filter(|a| is_tool(a.app_id, wanted.get(&a.app_id), &a.name)).collect();
        println!("  tools {}: {:?}", tools.len(), tools.iter().map(|a| (a.app_id, a.name.as_str(), wanted.get(&a.app_id).and_then(|i| i.kind.as_deref()))).collect::<Vec<_>>());
        let by_name = installed.iter().filter(|a| is_tool(a.app_id, None, &a.name)).count();
        println!("  tools by id/name only {by_name}");

        let accounts: Vec<String> = std::fs::read_dir(root.join("userdata"))
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n != "0" && n.bytes().all(|b| b.is_ascii_digit()))
            .collect();
        for (i, account) in accounts.iter().enumerate() {
            println!("account #{i}");
            let t = Instant::now();
            let p = playtime::read(root, account);
            println!(
                "  playtime: {} apps ({} with minutes, {} two-week, {} last played) in {:.1} ms",
                p.len(),
                p.values().filter(|x| x.minutes > 0).count(),
                p.values().filter(|x| x.two_weeks_minutes > 0).count(),
                p.values().filter(|x| x.last_played.is_some()).count(),
                t.elapsed().as_secs_f64() * 1e3
            );
            let stats_dir = root.join("appcache/stats");
            let schema_ids: Vec<u32> = std::fs::read_dir(&stats_dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok()?.file_name().into_string().ok()?.strip_prefix("UserGameStatsSchema_")?.strip_suffix(".bin")?.parse().ok())
                        .collect()
                })
                .unwrap_or_default();
            let t = Instant::now();
            let (mut apps, mut none, mut errs, mut total, mut unlocked, mut timed) = (0, 0, 0, 0, 0, 0);
            for id in &schema_ids {
                match stats::read(root, account, *id) {
                    Ok(Some(a)) => {
                        apps += 1;
                        total += a.total;
                        unlocked += a.unlocked;
                        timed += a.list.iter().filter(|x| x.unlocked_at.is_some()).count();
                    }
                    Ok(None) => none += 1,
                    Err(e) => {
                        errs += 1;
                        println!("  stats error for app {id}: {e:#}");
                    }
                }
            }
            println!(
                "  achievements: {} schemas, {apps} with achievements, {none} without, {errs} errors; {total} defined, {unlocked} unlocked ({timed} with a time) in {:.1} ms",
                schema_ids.len(),
                t.elapsed().as_secs_f64() * 1e3
            );
            let (mut apps, mut files, mut bytes) = (0, 0, 0);
            let mut states = std::collections::BTreeMap::new();
            for e in std::fs::read_dir(root.join("userdata").join(account)).unwrap().filter_map(|e| e.ok()) {
                let Some(id) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
                if let Some(c) = remotecache::read(root, account, id) {
                    apps += 1;
                    files += c.files.len();
                    bytes += c.total_size();
                    for f in &c.files {
                        *states.entry((f.root, f.sync_state, f.persist_state)).or_insert(0) += 1;
                    }
                }
            }
            println!("  cloud: {apps} apps, {files} files, {bytes} bytes; (root, sync, persist) counts {states:?}");
        }
    }
}
