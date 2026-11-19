//! `quiver sight`: the in-game panel (design/SIGHT.md §14). `toggle` is what Shift+F2 in a
//! game and the hyprpad chord do.

use anyhow::{Result, bail};
use clap::Subcommand;
use quiver_api::sight::{PanelCommand, PanelSection, SightRequest, SightStatus};
use quiver_api::{Client, Request};

/// The hyprpad snippet (`tools/hyprpad/quiver-sight.lua`).
pub const HYPRPAD_SNIPPET: &str = include_str!("../../../tools/hyprpad/quiver-sight.lua");

#[derive(Subcommand)]
pub enum SightAction {
    /// Open the panel over the running game, or close it (Shift+F2 in game, Guide+Start).
    Toggle {
        /// Open on this section: session, mods, tweaks, performance, achievements.
        #[arg(long)]
        section: Option<String>,
        /// A running session (`s-…`); default: the focused game, else the only one.
        #[arg(long)]
        session: Option<String>,
    },
    /// Open the panel (on SECTION).
    Open {
        section: Option<String>,
        #[arg(long)]
        session: Option<String>,
    },
    /// Close the panel.
    Close,
    /// Whether the panel is open, its backdrop (blur or dim) and why, the controller, the keys.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// The hyprpad config that puts the panel on the controller (`--snippet`: the Lua only).
    Hyprpad {
        #[arg(long)]
        snippet: bool,
    },
    /// The panel's key: read in the game by the Sight layer. `--conf` prints a Hyprland bind,
    /// the fallback for games without the layer (it fires over every window).
    Keybinds {
        #[arg(long)]
        conf: bool,
    },
    /// The controller fallback without hyprpad: Select+Start held 0.6 s opens the panel
    /// (`on`, `off`; no value: show). The game sees those buttons too. Off by default.
    PadChord { value: Option<String> },
}

fn section(s: Option<String>) -> Result<Option<PanelSection>> {
    s.map(|s| PanelSection::parse(&s).ok_or_else(|| anyhow::anyhow!("unknown section {s:?}: session, mods, tweaks, performance, achievements"))).transpose()
}

pub async fn run(client: &Client, action: SightAction) -> Result<()> {
    match action {
        SightAction::Toggle { section: s, session } => {
            let c: PanelCommand = client.call(Request::Sight(SightRequest::Toggle { session, section: section(s)? })).await?;
            println!("sent to {} (#{})", c.session.as_deref().unwrap_or("the panel"), c.seq);
        }
        SightAction::Open { section: s, session } => {
            let c: PanelCommand = client.call(Request::Sight(SightRequest::Open { session, section: section(s)? })).await?;
            println!("opening over {} (#{})", c.session.as_deref().unwrap_or("?"), c.seq);
        }
        SightAction::Close => {
            client.call::<PanelCommand>(Request::Sight(SightRequest::Close)).await?;
            println!("closing");
        }
        SightAction::Status { json } => {
            let s: SightStatus = client.call(Request::Sight(SightRequest::Status)).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
                return Ok(());
            }
            print_status(&s);
            // Every running game's session: the layer (and so the keys) or why not.
            let sessions: Vec<quiver_api::perf::PerfSessionInfo> = client.call(Request::Perf(quiver_api::perf::PerfRequest::Sessions)).await.unwrap_or_default();
            for x in sessions.iter().filter(|x| !x.dev) {
                let what = if x.hud_enabled {
                    format!("Sight layer in; HUD {}", if x.hud_on { "shown" } else { "hidden (Shift+F3)" })
                } else {
                    format!("no Sight layer ({}); `quiver sight toggle` still opens the panel", x.layer_off.as_deref().unwrap_or("off"))
                };
                println!("game:     {} {} — {what}", x.session, x.game_ref());
            }
        }
        SightAction::Hyprpad { snippet } => {
            if snippet {
                print!("{HYPRPAD_SNIPPET}");
            } else {
                println!("The panel on the controller needs a few lines in your hyprpad config:");
                println!("  - a `quiver-sight` mode while the panel is up (the game's pad goes neutral),");
                println!("  - the pad's buttons typed as keys into the panel in that mode,");
                println!("  - Guide+Start running `quiver sight toggle`.");
                println!("Quiver doesn't edit your config. Print the snippet with `quiver sight hyprpad --snippet`;");
                println!("its comments say where it goes and which existing lines to change.");
            }
        }
        SightAction::Keybinds { conf } => {
            if conf {
                println!("-- Lua config (hyprland.lua): fires over every window");
                println!("hl.bind(\"SHIFT + F2\", hl.dsp.exec_cmd(\"quiver sight toggle\"))");
                println!("# hyprlang config (hyprland.conf)");
                println!("bindd = SHIFT, F2, Quiver Sight panel, exec, quiver sight toggle");
            } else {
                println!("Shift+F2 opens the panel: the Sight layer reads it in the game (X11 and Wayland windows),");
                println!("like the HUD's Shift+F3. In the panel, Shift+F2, Escape or B closes it.");
                println!("Games without the layer (OpenGL, anti-cheat): `quiver sight keybinds --conf` prints a Hyprland bind.");
                println!("Controller: `quiver sight hyprpad`.");
            }
        }
        SightAction::PadChord { value } => {
            let path = sight_settings_path();
            let mut v: toml::Table = std::fs::read_to_string(&path).ok().and_then(|s| s.parse().ok()).unwrap_or_default();
            match value.as_deref() {
                None => {}
                Some("on" | "true" | "1") => {
                    v.insert("pad_chord".into(), toml::Value::Boolean(true));
                }
                Some("off" | "false" | "0") => {
                    v.insert("pad_chord".into(), toml::Value::Boolean(false));
                }
                Some(other) => bail!("pad-chord takes on or off, not {other:?}"),
            }
            if value.is_some() {
                if let Some(d) = path.parent() {
                    std::fs::create_dir_all(d)?;
                }
                std::fs::write(&path, toml::to_string(&v)?)?;
            }
            let on = v.get("pad_chord").and_then(|x| x.as_bool()).unwrap_or(false);
            println!("Select+Start held 0.6 s: {} ({})", if on { "opens the panel" } else { "off" }, path.display());
            if value.is_some() {
                println!("quiver-sight reads it when it starts (the next game launch).");
            }
        }
    }
    Ok(())
}

/// `$XDG_DATA_HOME/quiver/sight.toml` (quiver-sight's `panel::settings_path`).
fn sight_settings_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME").map(std::path::PathBuf::from).unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    base.join("quiver").join("sight.toml")
}

fn print_status(s: &SightStatus) {
    println!("quiver-sight: {}", if s.running { "running" } else { "not running (quiverd starts it with a game)" });
    match &s.panel {
        Some(p) => {
            let state = match (&p.hidden, p.open) {
                (Some(_), true) => "open, hidden",
                (None, true) => "open",
                _ => "closed",
            };
            println!("panel:    {state}{}", p.session.as_deref().map(|x| format!(" over {x}")).unwrap_or_default());
            if p.open {
                println!("section:  {}", p.section.label());
            }
            if let Some(h) = p.hidden.as_ref().filter(|_| p.open) {
                println!("hidden:   {h} (it comes back when the game is in front)");
            }
            if p.open && !p.front.is_empty() {
                println!("game:     {}{}", p.front, p.game_window.as_deref().map(|w| format!(" · tied to {w}")).unwrap_or_default());
            }
            if let Some(m) = &p.monitor {
                println!("monitor:  {m}");
            }
            if !p.presenter.is_empty() {
                let how = if p.presenter == "game" { "in the game (input through the preload; the game keeps focus)" } else { "on the desktop (layer-shell, takes the keyboard)" };
                let why = if p.presenter_detail.is_empty() { String::new() } else { format!(" — {}", p.presenter_detail) };
                println!("drawn:    {how}{why}");
            }
            println!("backdrop: {} — {}", p.backdrop.as_str(), if p.backdrop_detail.is_empty() { "-" } else { &p.backdrop_detail });
            println!("pad:      {}{}", if p.pad.is_empty() { "none" } else { &p.pad }, if p.pad_chord { " · Select+Start held opens the panel" } else { "" });
            if let Some(e) = &p.last_error {
                println!("error:    {e}");
            }
        }
        None => println!("panel:    never reported (not opened since quiverd started)"),
    }
    if let Some(c) = &s.last_command {
        println!("last:     {:?} #{} by {} at {}", c.action, c.seq, c.by, c.at.with_timezone(&chrono::Local).format("%H:%M:%S"));
    }
    for k in &s.keys {
        println!("keys:     {} — {} (Shift+F2 presses: {})", k.session, if k.keys.is_empty() { "unknown" } else { &k.keys }, k.presses);
        if let Some(st) = &k.stereo {
            println!("stereo:   {} — {}", k.session, st.line());
        }
    }
}
