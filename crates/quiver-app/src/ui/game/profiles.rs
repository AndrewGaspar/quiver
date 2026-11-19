//! Profiles as named setups of a game (design/PROFILES.md): the New profile dialog (start
//! from a profile, Vanilla or blank; its mods; its own game settings; its save store) and the
//! selected profile's Game settings card (its own settings files, Options and Tweaks values,
//! launch overrides).

use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, div, prelude::*, px};
use quiver_api::mods::{ModsRequest, ModsState, ProfileInfo, ProfileRevision, VANILLA};
use quiver_api::Request;

use super::options::ModsView;
use super::page::{Dialog, checkbox, small};
use crate::app::Quiver;
use crate::err_text;
use crate::model::{Gid, GameRow};
use crate::nav::Tab;
use crate::theme::Theme;
use crate::ui::{Icon, PillKind, SEMIBOLD, card, card_header, caption, chip, icon, mono, pill};

/// Blank: no source (`from` in the dialog).
pub const BLANK: &str = "";

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

/// The last two components of a prefix folder ("Remedy/AlanWakeRemastered").
pub fn short_dir(dir: &str) -> String {
    let parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    parts[parts.len().saturating_sub(2)..].join("/")
}

/// The mods a profile stacks, as mod ids in stack order.
fn stack_mods(m: &ModsState, layers: &[String]) -> Vec<String> {
    layers.iter().filter_map(|e| m.resolve(e).map(|r| r.mod_id)).collect()
}

/// A profile's game settings in a few words, for its card: "own settings", "Vanilla's
/// settings", plus Options and Tweaks counts.
pub fn settings_summary(p: &ProfileInfo) -> String {
    let mut v = vec![if p.config_dirs.is_empty() { "shared settings".to_owned() } else { "own settings".to_owned() }];
    let n = (p.option_values + p.tweak_values) as usize;
    if n > 0 {
        v.push(plural(n, "value"));
    }
    v.join(" · ")
}

/// What "from" copies, in words, for the dialog.
fn copies_line(m: &ModsState, from: &str) -> String {
    if from == BLANK {
        return "Blank: no mods, no launch overrides, no Options values; the game's settings as Vanilla has them now.".into();
    }
    if from == VANILLA {
        let n = (m.vanilla_option_values + m.vanilla_tweak_values) as usize;
        return format!(
            "Copies Vanilla: no mods, the game's settings files as they are now{}.",
            if n > 0 { format!(", and its {}", plural(n, "Options value")) } else { String::new() }
        );
    }
    let Some(p) = m.profiles.iter().find(|p| p.profile.id == from) else { return String::new() };
    let mut parts = vec![plural(p.profile.layers.len(), "mod")];
    if p.profile.settings != Default::default() {
        parts.push("launch overrides".into());
    }
    parts.push(if p.config_dirs.is_empty() { "the game's settings files".into() } else { "its own settings files".into() });
    let n = (p.option_values + p.tweak_values) as usize;
    if n > 0 {
        parts.push(format!("{} (Options and Tweaks)", plural(n, "value")));
    }
    if m.has_save_stores() {
        parts.push("its save store".into());
    }
    format!("Copies {}: {}. Changing the copy later doesn't change {}.", p.profile.name, parts.join(", "), p.profile.name)
}

impl Quiver {
    /// Open the New profile dialog, starting from the selected profile (or Vanilla).
    pub fn new_profile_dialog(&mut self, id: Gid, cx: &mut Context<Self>) {
        let from = self.gp.prof_sel.clone().filter(|s| s != VANILLA && self.gp.mods.ready().is_some_and(|m| m.profiles.iter().any(|p| &p.profile.id == s)));
        let name = self.text_input("Profile name, e.g. HDR on, Performance", "", false, cx);
        let own = self.gp.mods.ready().is_some_and(|m| m.settings_dirs.iter().any(|d| d.exists));
        let mut d = Dialog::NewProfile { game: id, name, layers: Vec::new(), saves: None, from: from.unwrap_or_else(|| VANILLA.into()), own_settings: own };
        self.dialog_from(&mut d);
        self.open_dialog(d);
    }

    /// Tick the mods of the dialog's source.
    fn dialog_from(&self, d: &mut Dialog) {
        let Dialog::NewProfile { layers, from, saves, .. } = d else { return };
        let Some(m) = self.gp.mods.ready() else { return };
        let src: Vec<String> = m.profiles.iter().find(|p| &p.profile.id == from).map(|p| stack_mods(m, &p.profile.layers)).unwrap_or_default();
        *layers = m.all_mods().iter().map(|r| (r.id.clone(), r.name.clone(), src.contains(&r.id))).collect();
        *saves = None;
    }

    /// Create the profile the dialog describes.
    pub(super) fn create_profile_from_dialog(&mut self, d: Dialog, cx: &mut Context<Self>) {
        let Dialog::NewProfile { game, name, layers, saves, from, own_settings } = d else { return };
        let n = name.read(cx).text().trim().to_owned();
        if n.is_empty() {
            self.show_toast("A profile needs a name", cx);
            self.gp.dialog = Some(Dialog::NewProfile { game, name, layers, saves, from, own_settings });
            self.gp.dialog_btn = 1;
            return;
        }
        let ticked: Vec<String> = layers.iter().filter(|l| l.2).map(|l| l.0.clone()).collect();
        // The source's stack as it is (its order and pins) unless the ticks changed it.
        let src = self.gp.mods.ready().and_then(|m| m.profiles.iter().find(|p| p.profile.id == from).map(|p| stack_mods(m, &p.profile.layers)));
        let same = src.as_ref().is_some_and(|s| {
            let mut a = s.clone();
            let mut b = ticked.clone();
            a.sort();
            b.sort();
            a == b
        });
        let req = ModsRequest::ProfileCreate {
            game: game.0.into(),
            name: n,
            layers: if same { None } else { Some(ticked) },
            message: None,
            saves,
            from: (from != BLANK).then_some(from),
            own_settings: Some(own_settings),
        };
        self.call::<serde_json::Value, _>(Request::Mods(req), cx, move |this, r, cx| {
            match r.and_then(|v| {
                let copied: Vec<String> = v.get("copied").and_then(|c| serde_json::from_value(c.clone()).ok()).unwrap_or_default();
                Ok((serde_json::from_value::<ProfileRevision>(v)?, copied))
            }) {
                Ok((rev, copied)) => {
                    let what = copied.first().map(|c| format!(": {c}")).unwrap_or_default();
                    this.show_toast(format!("Created {}{what}", rev.profile.name), cx);
                    this.load_mods(game, cx);
                    this.select_profile(game, Some(rev.profile.id), cx);
                }
                Err(e) => this.show_toast(err_text(&e), cx),
            }
        });
    }

    /// The dialog's body: name, start from, mods, game settings, save store.
    pub(super) fn render_new_profile(&mut self, th: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(Dialog::NewProfile { name, layers, saves, from, own_settings, .. }) = &self.gp.dialog else { return Vec::new() };
        let (name, layers, saves, from, own) = (name.clone(), layers.clone(), saves.clone(), from.clone(), *own_settings);
        let Some(m) = self.gp.mods.ready().cloned() else { return vec![div().child(name).into_any_element()] };
        let sync = self.save_sync(&m);

        // Start from: each profile, Vanilla, Blank.
        let mut sources: Vec<(String, String, Icon)> = m.profiles.iter().map(|p| (p.profile.id.clone(), p.profile.name.clone(), Icon::Layers)).collect();
        sources.push((VANILLA.into(), "Vanilla".into(), Icon::ShieldCheck));
        sources.push((BLANK.into(), "Blank".into(), Icon::Plus));
        let from_chips = div().flex().flex_wrap().gap(px(6.)).children(sources.into_iter().enumerate().map(|(i, (sid, label, ic))| {
            let on = sid == from;
            chip(th, Some(ic), label, on)
                .id(("np-from", i))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(mut d) = this.gp.dialog.take() {
                        if let Dialog::NewProfile { from, .. } = &mut d {
                            *from = sid.clone();
                        }
                        this.dialog_from(&mut d);
                        this.gp.dialog = Some(d);
                    }
                    cx.notify();
                }))
                .into_any_element()
        }));

        // Mods: the library, ticked from the source.
        let rows: Vec<AnyElement> = layers
            .iter()
            .enumerate()
            .map(|(i, (_, n, on))| {
                div()
                    .id(("np-layer", i))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .hover(|s| s.bg(th.ink3))
                    .child(checkbox(th, *on))
                    .child(div().flex_1().child(n.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(Dialog::NewProfile { layers, .. }) = &mut this.gp.dialog
                            && let Some(l) = layers.get_mut(i)
                        {
                            l.2 = !l.2;
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        let ticked = layers.iter().filter(|l| l.2).count();
        let mods_note = if layers.is_empty() {
            "This game's mod library is empty. A profile doesn't need mods: it's a setup of the game's settings, and you can add mods later."
        } else if ticked == 0 {
            "No mods: a settings-only profile."
        } else {
            "Ticked mods stack in library order; reorder them on the profile afterwards."
        };

        // Game settings: its own copy of the settings folders, or Vanilla's files.
        let known: Vec<&quiver_api::mods::SettingsDir> = m.settings_dirs.iter().filter(|d| d.exists).collect();
        let src_name = m.profiles.iter().find(|p| p.profile.id == from).map_or("Vanilla".to_owned(), |p| p.profile.name.clone());
        let settings_text = if known.is_empty() {
            "Quiver doesn't know where this game keeps its settings yet, so the profile shares Vanilla's settings files. After the game has run once you can give it its own.".to_owned()
        } else if own {
            let dirs: Vec<String> = known.iter().map(|d| short_dir(&d.dir)).collect();
            let shared: Vec<String> = known.iter().flat_map(|d| d.shared.iter().cloned()).collect();
            format!(
                "A copy of {} as {} has them now, so this profile's in-game settings don't change other profiles'.{}",
                dirs.join(", "),
                if from == BLANK { "Vanilla".to_owned() } else { src_name.clone() },
                if shared.is_empty() { String::new() } else { format!(" Saves in {} stay shared.", shared.join(", ")) }
            )
        } else {
            "Shares the game's settings files with Vanilla (and every profile that shares them): a change in game changes them for all.".to_owned()
        };
        let own_row = div()
            .id("np-own")
            .flex()
            .items_start()
            .gap(px(10.))
            .px(px(10.))
            .py(px(6.))
            .rounded(px(8.))
            .when(!known.is_empty(), |d| {
                d.cursor_pointer().hover(|s| s.bg(th.ink3)).on_click(cx.listener(|this, _, _, cx| {
                    if let Some(Dialog::NewProfile { own_settings, .. }) = &mut this.gp.dialog {
                        *own_settings = !*own_settings;
                    }
                    cx.notify();
                }))
            })
            .child(div().pt(px(1.)).child(checkbox(th, own && !known.is_empty())))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().child("Its own game settings"))
                    .child(div().text_size(px(11.5)).text_color(th.tx3).child(settings_text)),
            );

        let saves_section = super::stores::new_profile_saves(
            th,
            Some(&m),
            sync,
            &layers,
            saves.as_deref(),
            m.profiles.iter().find(|p| p.profile.id == from).and_then(|p| p.profile.saves.as_deref().map(|s| (s.to_owned(), p.profile.name.clone()))),
            cx,
        );
        vec![
            div().child(name).into_any_element(),
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(caption("Start from", th.tx3))
                .child(from_chips)
                .child(div().text_size(px(11.5)).text_color(th.tx3).child(copies_line(&m, &from)))
                .into_any_element(),
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(div().flex().items_center().gap(px(8.)).mb(px(4.)).child(caption("Mods", th.tx3)).child(div().text_size(px(11.)).text_color(th.tx3).child(format!("{ticked} of {}", layers.len()))))
                .children(rows)
                .child(div().px(px(10.)).pt(px(2.)).text_size(px(11.5)).text_color(th.tx3).child(mods_note))
                .into_any_element(),
            div().flex().flex_col().gap(px(4.)).child(caption("Game settings", th.tx3)).child(own_row).into_any_element(),
            saves_section,
        ]
    }

    /// Give a profile its own game settings, or make it share Vanilla's.
    pub fn profile_settings(&mut self, id: Gid, profile: &str, own: bool, cx: &mut Context<Self>) {
        let p = profile.to_owned();
        let req = ModsRequest::ProfileSettings { game: id.0.into(), profile: p.clone(), own, message: None };
        self.call::<serde_json::Value, _>(Request::Mods(req), cx, move |this, r, cx| match r {
            Ok(v) => {
                let copied: Vec<String> = v.get("copied").and_then(|c| serde_json::from_value(c.clone()).ok()).unwrap_or_default();
                let msg = if own { "Its own game settings" } else { "Shares Vanilla's game settings" };
                this.show_toast(match copied.first() {
                    Some(c) => format!("{msg}: {c}"),
                    None => msg.to_owned(),
                }, cx);
                this.load_mods(id, cx);
                this.load_profile_history(id, &p, cx);
            }
            Err(e) => this.show_toast(err_text(&e), cx),
        });
    }

    /// The selected profile's (or Vanilla's) game settings: its settings files, Options and
    /// Tweaks values, launch overrides.
    pub(super) fn render_game_settings(&mut self, th: &Theme, g: &GameRow, m: &ModsState, p: Option<&ProfileInfo>, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        let known: Vec<quiver_api::mods::SettingsDir> = m.settings_dirs.clone();
        let (options, tweaks) = match p {
            Some(p) => (p.option_values, p.tweak_values),
            None => (m.vanilla_option_values, m.vanilla_tweak_values),
        };
        let row = |label: &str, value: AnyElement, action: Option<AnyElement>| {
            div()
                .flex()
                .items_start()
                .gap(px(12.))
                .px(px(16.))
                .py(px(10.))
                .border_t_1()
                .border_color(th.line1)
                .child(div().w(px(118.)).flex_none().text_size(px(12.5)).text_color(th.tx2).child(label.to_owned()))
                .child(div().flex_1().min_w_0().text_size(px(12.5)).child(value))
                .children(action)
        };
        // Settings files.
        let files: AnyElement = match p {
            None if known.is_empty() => div().text_color(th.tx3).child("The game's own files in its prefix (Quiver hasn't found its settings folder yet)").into_any_element(),
            None => div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(div().child("The game's own files in its prefix"))
                .children(known.iter().map(|d| mono(11., th.tx3).truncate().child(d.dir.clone())))
                .into_any_element(),
            Some(p) if p.config_dirs.is_empty() => div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(div().child("Shares Vanilla's settings files"))
                .child(div().text_size(px(11.5)).text_color(th.tx3).child(if known.iter().any(|d| d.exists) {
                    format!("{}: a change in game changes them for every profile that shares them.", known.iter().filter(|d| d.exists).map(|d| short_dir(&d.dir)).collect::<Vec<_>>().join(", "))
                } else {
                    "Quiver doesn't know where this game keeps its settings yet; after a first run it can give the profile its own.".to_owned()
                }))
                .into_any_element(),
            Some(p) => div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .children(p.config_dirs.iter().map(|d| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap(px(8.))
                                .child(div().font_weight(SEMIBOLD).child(short_dir(&d.dir)))
                                .child(pill(th, PillKind::Accent, None, format!("{} own", plural(d.files.len(), "file")))),
                        )
                        .child(mono(11., th.tx3).truncate().child(d.upper.display().to_string()))
                        .when(!d.shared.is_empty(), |x| {
                            x.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.))
                                    .text_size(px(11.5))
                                    .text_color(th.tx3)
                                    .child(icon(Icon::Users, 11., th.tx3))
                                    .child(format!("Saves in {} stay shared with every profile", d.shared.join(", "))),
                            )
                        })
                }))
                .into_any_element(),
        };
        let files_action = p.filter(|_| known.iter().any(|d| d.exists) || p.is_some_and(|p| !p.config_dirs.is_empty())).map(|p| {
            let own = p.config_dirs.is_empty();
            let pid = p.profile.id.clone();
            small("gs-own", th, Some(if own { Icon::Copy } else { Icon::Users }), if own { "Give it its own" } else { "Share Vanilla's" })
                .on_click(cx.listener(move |this, _, _, cx| this.profile_settings(id, &pid, own, cx)))
                .into_any_element()
        });
        let values = |n: u32, what: &str| if n == 0 { format!("None set ({what} defaults)") } else { plural(n as usize, "value") };
        let open_options = small("gs-options", th, Some(Icon::SlidersHorizontal), "Open Options").on_click(cx.listener(|this, _, _, cx| {
            this.gp.opt.view = ModsView::Options;
            cx.notify();
        }));
        let open_tweaks = small("gs-tweaks", th, Some(Icon::Sparkles), "Open Tweaks").on_click(cx.listener(|this, _, _, cx| {
            this.nav.tab = Tab::Tweaks;
            cx.notify();
        }));
        let overrides = p.map(|p| super::page::flatten(&p.profile.settings)).unwrap_or_default();
        let overrides_el: AnyElement = if overrides.is_empty() {
            div().text_color(th.tx3).child(if p.is_some() { "None: the game's launch options" } else { "The game's launch options" }).into_any_element()
        } else {
            div().flex().flex_col().gap(px(2.)).children(overrides.iter().map(|(k, v)| mono(11.5, th.tx2).child(format!("{k}={v}")))).into_any_element()
        };
        card(th)
            .child(
                card_header(th, "Game settings")
                    .child(div().text_size(px(12.)).text_color(th.tx3).child(if p.is_some() { "what this profile sets besides its mods" } else { "the base game's" }))
                    .child(div().flex_1()),
            )
            .child(row("Settings files", files, files_action))
            .child(row("Options", div().child(values(options, "the mods'")).into_any_element(), Some(open_options.into_any_element())))
            .child(row("Tweaks", div().child(values(tweaks, if p.is_some() { "this game's" } else { "global" })).into_any_element(), Some(open_tweaks.into_any_element())))
            .child(row("Launch overrides", overrides_el, None))
            .into_any_element()
    }
}
