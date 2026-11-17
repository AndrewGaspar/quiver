//! `quiver setup`: the app's first-run welcome, in a terminal. The same choices, stored in
//! the daemon (`Request::Setup`), so the app doesn't ask again afterwards.

use std::io::IsTerminal;

use anyhow::{Result, bail};
use quiver_api::exports::{ExportKind, ExportStatus, PatchState, SteamExportResult, SteamGame, SteamGames, SteamPatchResult, SteamUsers};
use quiver_api::setup::{FinishSetup, FixesChoice, FoundInstall, SetupState};
use quiver_api::mods::{ModPrefs, ModsRequest};
use quiver_api::{Client, LibraryEntry, Request, Status};

use crate::{dev, exports, follow, print_json};

fn gb(bytes: u64) -> String {
    let g = bytes as f64 / 1e9;
    if g >= 100.0 { format!("{g:.0} GB") } else if g >= 1.0 { format!("{g:.1} GB") } else { format!("{:.0} MB", bytes as f64 / 1e6) }
}

fn tilde(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s,
    }
}

/// `[Y/n]`-style question; Enter takes `default`.
fn yes(question: &str, default: bool) -> Result<bool> {
    let a = dev::prompt(&format!("{question} [{}] ", if default { "Y/n" } else { "y/N" }))?;
    Ok(match a.to_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    })
}

fn step(n: usize, title: &str) {
    println!("\n{n}/7  {title}");
}

/// Print the setup state (`quiver setup --show`).
pub async fn show(client: &Client, json: bool) -> Result<()> {
    let s: SetupState = client.call(Request::Setup { library: None, prefixes: None, finish: None }).await?;
    if json {
        return print_json(&s);
    }
    match &s.first_run {
        Some(f) => println!(
            "setup {} {} by {}",
            if f.skipped { "skipped" } else { "done" },
            f.at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
            f.source
        ),
        None => println!("setup not done yet (quiver setup)"),
    }
    let free = |f: Option<u64>| f.map(|b| format!(" ({} free)", gb(b))).unwrap_or_default();
    println!("library:  {}{}", tilde(&s.library), free(s.library_free));
    println!("prefixes: {}{}", tilde(&s.prefixes), free(s.prefixes_free));
    println!(
        "recommended fixes on unmodded games: {}",
        match s.fixes_on_vanilla {
            Some(true) => "yes",
            Some(false) => "no",
            None => "not decided",
        }
    );
    Ok(())
}

/// Walk the first-run choices.
pub async fn run(client: &Client) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("quiver setup asks questions; run it in a terminal (quiver setup --show prints the state)");
    }
    println!("Welcome to Quiver: a few choices, each skippable (Enter takes the default).");

    step(1, "Sign in");
    let status: Status = client.call(Request::Status).await?;
    if status.accounts.is_empty() {
        if yes("Sign in to Ubisoft now?", true)? {
            crate::login(client, None).await?;
        } else {
            println!("  Later: quiver login");
        }
    } else {
        let names: Vec<&str> = status.accounts.iter().map(|a| a.name.as_str()).collect();
        println!("  Signed in: {}", names.join(", "));
    }

    step(2, "Where games are installed");
    let mut s: SetupState = client.call(Request::Setup { library: None, prefixes: None, finish: None }).await?;
    let free = |f: Option<u64>| f.map(|b| format!("  ({} free)", gb(b))).unwrap_or_default();
    println!("  Games: {}{}", tilde(&s.library), free(s.library_free));
    println!("  (Advanced: each game's Windows compatibility folder, its Proton prefix, goes in {})", tilde(&s.prefixes));
    if s.paths_locked {
        println!("  {} game(s) installed, so these stay as they are.", s.installed);
    } else if yes("Change where games go?", false)? {
        let ask = |what: &str, cur: &std::path::Path| -> Result<Option<std::path::PathBuf>> {
            let a = dev::prompt(&format!("  {what} [{}]: ", tilde(cur)))?;
            Ok((!a.is_empty()).then(|| a.into()))
        };
        let library = ask("Games folder", &s.library)?;
        let prefixes = if yes("Also move the compatibility folders (advanced)?", false)? {
            ask("Compatibility folders (Proton prefixes)", &s.prefixes)?
        } else {
            None
        };
        if library.is_some() || prefixes.is_some() {
            s = client.call(Request::Setup { library, prefixes, finish: None }).await?;
            println!("  Now: {} and {}", tilde(&s.library), tilde(&s.prefixes));
            if s.restarting {
                println!("  quiverd restarts to use them.");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }

    step(3, "Ubisoft Connect games you already downloaded");
    println!("  Games downloaded with Ubisoft Connect under Lutris, Wine or inside a Steam game can join");
    println!("  your library without downloading again. (Ubisoft games bought on Steam: see step 4.)");
    println!("  Looking in Lutris, ~/Games and Steam games for Ubisoft Connect downloads (read-only)…");
    let found: Vec<FoundInstall> = client.call(Request::ScanInstalls).await?;
    if found.is_empty() {
        println!("  None found.");
    }
    for (i, f) in found.iter().enumerate() {
        let state = if f.importable { String::new() } else { format!("  [{}]", f.note.as_deref().unwrap_or("can't import")) };
        println!("  {:>2}) {}  {} · {}{}", i + 1, f.name, gb(f.size_bytes), f.source_label, state);
        println!("      {}", tilde(&f.path));
    }
    let importable: Vec<&FoundInstall> = found.iter().filter(|f| f.importable).collect();
    if !importable.is_empty() {
        println!("  Importing copies the game's own files into your library (reflinks: instant on btrfs/xfs).");
        println!("  The originals stay where they are; mods and other extra files are listed, not copied.");
        let a = dev::prompt("  Import which? (numbers like 1,3, `all`, or Enter for none): ")?;
        let picks: Vec<&FoundInstall> = if a.eq_ignore_ascii_case("all") {
            importable.clone()
        } else {
            a.split([',', ' ']).filter_map(|n| n.trim().parse::<usize>().ok()).filter_map(|n| found.get(n.wrapping_sub(1))).filter(|f| f.importable).collect()
        };
        for f in picks {
            let Some(product_id) = f.product_id else { continue };
            let events = client.subscribe().await?;
            let id: u64 = client
                .call(Request::Import { product_id, path: f.path.clone(), language: f.language.clone() })
                .await?;
            eprintln!("  importing {} (job {id})", f.name);
            if let Err(e) = follow(client, events, id).await {
                eprintln!("  {}: {e:#}", f.name);
            }
        }
    }

    step(4, "Steam");
    let users: SteamUsers = client.call(Request::SteamUsers).await?;
    let mut steam_on_install = true;
    if users.root.is_none() {
        println!("  Steam isn't installed; skipping.");
    } else {
        // Ubisoft games bought on Steam: start them through Quiver instead of Connect.
        let sg: SteamGames = client.call(Request::SteamGames { steam_user: None }).await?;
        let switchable: Vec<&SteamGame> = sg.games.iter().filter(|g| g.patchable && g.state == PatchState::Off).collect();
        if sg.games.is_empty() {
            println!("  No Ubisoft games are installed through Steam.");
        } else {
            println!("  Ubisoft games installed through Steam:");
            for g in &sg.games {
                let how = if g.state.uses_quiver() { "starts through Quiver" } else { "opens Ubisoft Connect first" };
                println!("    {} ({how}){}", g.name, g.note.as_deref().map(|n| format!(": {n}")).unwrap_or_default());
            }
            println!("  Started through Quiver they skip the Connect window and its second sign-in; play time");
            println!("  still counts. Only their Steam launch options change; `quiver steam unpatch` undoes it.");
        }
        if !switchable.is_empty() && yes(&format!("Start {} of them through Quiver?", switchable.len()), true)? {
            let mut staged = false;
            for g in switchable {
                match client.call::<SteamPatchResult>(Request::SteamPatch { app_id: g.app_id, steam_user: None }).await {
                    Ok(r) => {
                        staged |= r.staged;
                        println!("  {} now starts through Quiver", r.name);
                    }
                    Err(e) => println!("  {}: {e:#}", g.name),
                }
            }
            if staged {
                println!("  {}", exports::restart_hint());
            }
        }
        let offers = yes("Offer this when a Ubisoft game is installed through Steam later?", sg.offers)?;
        if offers != sg.offers {
            let _: serde_json::Value = client.call(Request::SteamOffers { enabled: Some(offers) }).await?;
        }
        println!("  Quiver can add games to Steam as non-Steam shortcuts with Ubisoft art.");
        println!("  Steam reads shortcuts only when it starts, so new ones show after it restarts;");
        println!("  Quiver never restarts Steam without asking you first.");
        steam_on_install = yes("Offer \"Add to Steam\" when an install finishes?", true)?;
        let lib: Vec<LibraryEntry> = client.call(Request::Library { refresh: false, all_stores: false }).await.unwrap_or_default();
        let exports: Vec<ExportStatus> = client.call(Request::ExportList).await.unwrap_or_default();
        let missing: Vec<&LibraryEntry> = lib
            .iter()
            .filter(|e| e.installed.is_some())
            .filter(|e| !exports.iter().any(|x| x.record.kind == ExportKind::Steam && x.record.game == e.game && x.record.profile_id.is_none()))
            .collect();
        if !missing.is_empty() && yes(&format!("Add {} installed game(s) to Steam now?", missing.len()), false)? {
            let user = exports::pick_user(client, None).await?;
            let mut staged = false;
            for (e, product_id) in missing.into_iter().filter_map(|e| Some((e, e.ubisoft_id()?))) {
                match client
                    .call::<SteamExportResult>(Request::ExportSteam { product_id, remove: false, steam_user: user.clone() })
                    .await
                {
                    Ok(r) => {
                        staged |= r.staged;
                        println!("  added {}", e.name);
                    }
                    Err(err) => println!("  {}: {err:#}", e.name),
                }
            }
            if staged {
                println!("  {}", exports::restart_hint());
            }
        }
    }

    step(5, "Controller");
    let hyprpad = std::fs::read_dir("/proc")
        .map(|rd| {
            rd.flatten()
                .any(|e| std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim().starts_with("hyprpad")))
        })
        .unwrap_or(false);
    println!(
        "  hyprpad is {}. Quiver reads the pad itself while its window has focus and never uses",
        if hyprpad { "running" } else { "not running" }
    );
    println!("  the Guide button. A select · B back · X play options · Y menu · LB/RB sections ·");
    println!("  LT/RT tabs · Start command palette.");

    step(6, "Recommended fixes for unmodded games");
    println!("  Small, audited tweaks from PCGamingWiki such as skipping intro videos and fixing");
    println!("  mouse input. They never change gameplay or your saves, and you can turn them off");
    println!("  per game. Without them, unmodded games run exactly as shipped.");
    let prefs: ModPrefs = client.call(Request::Mods(ModsRequest::Prefs { fixes_on_vanilla: None })).await?;
    let def = match prefs.fixes_on_vanilla {
        Some(true) => "y",
        Some(false) => "n",
        None => "l",
    };
    let a = dev::prompt(&format!("  Apply them? [y]es / [n]o / [l]ater (default {def}): "))?;
    let choice: FixesChoice = if a.is_empty() { def.parse().map_err(anyhow::Error::msg)? } else { a.parse().map_err(anyhow::Error::msg)? };
    // "Later" leaves it undecided: the app asks again.
    if let Some(v) = choice.value() {
        let _: ModPrefs = client.call(Request::Mods(ModsRequest::Prefs { fixes_on_vanilla: Some(v) })).await?;
    }

    step(7, "Theme");
    println!("  Quiver follows your Omarchy theme and restyles live when it changes.");

    let _: SetupState = client
        .call(Request::Setup { library: None, prefixes: None, finish: Some(FinishSetup { skipped: false, steam_on_install }) })
        .await?;
    println!("\nDone. `quiver-app` won't show its welcome again; run `quiver setup` any time to revisit.");
    Ok(())
}
