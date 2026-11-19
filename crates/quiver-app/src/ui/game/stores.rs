//! Save stores on the game page: the Saves tab's list of stores (who uses each, which one
//! syncs with the cloud), the profile editor's store picker with the advice, and the
//! new-profile dialog's choice. Stores are chosen per profile, independently of its mods.

use gpui::{AnyElement, Context, IntoElement, ParentElement, SharedString, Styled, div, prelude::*, px};
use quiver_api::Request;
use quiver_api::SettingOp;
use quiver_api::mods::{ModsState, ProfileInfo};
use quiver_api::savestores::{DEFAULT_STORE, NEW_STORE, SaveStore, SaveStoresReport, SaveSync, SavesAdvice, cloud_name, real_saves_note, saves_advice, store_sync_note};

use super::page::{Dialog, Load, load_note};
use crate::app::Quiver;
use crate::model::{Gid, GameRow, ago};
use crate::theme::Theme;
use crate::ui::{Icon, PillKind, SEMIBOLD, card, card_header, caption, chip, icon, mono, pill};

/// Who syncs the game's saves, as far as the app knows: the daemon's answer, made
/// [`SaveSync::UbisoftNone`] for a Ubisoft game whose page says it has no cloud saves.
pub fn sync_of(m: &ModsState, ubisoft_cloud_saves: Option<bool>) -> Option<SaveSync> {
    match m.saves_sync() {
        Some(SaveSync::Ubisoft) if ubisoft_cloud_saves == Some(false) => Some(SaveSync::UbisoftNone),
        s => s,
    }
}

/// The saves of a game without save stores (Epic, Steam), for every profile: the real
/// saves and who syncs them, with no store picker.
pub fn real_saves_text(m: &ModsState, sync: Option<SaveSync>) -> String {
    let label = m.game_ref().store.label();
    format!("{}. Profiles with saves of their own aren't available for {label} games yet.", real_saves_note(m.game_ref().store, sync))
}

/// The saves section of such a game's profile editor and new-profile dialog.
fn real_saves(th: &Theme, m: &ModsState, sync: Option<SaveSync>) -> AnyElement {
    let ic = if sync.is_some_and(SaveSync::syncs) { Icon::Cloud } else { Icon::HardDrive };
    div().flex().flex_col().gap(px(6.)).child(caption("Saves", th.tx3)).child(line(ic, th.tx3, real_saves_text(m, sync))).into_any_element()
}

/// The advice for a stack (entries: mods tracking their current version, or pinned ones;
/// enabled mods only, as a launch applies them).
pub fn advice_for(m: &ModsState, layers: &[String]) -> SavesAdvice {
    saves_advice(&m.stack_layers(layers))
}

/// The live stores of a Ubisoft game, Default first; none syncs when the game has no
/// cloud saves (`sync` [`SaveSync::UbisoftNone`]).
fn live(m: &ModsState, sync: Option<SaveSync>) -> Vec<SaveStore> {
    let mut v: Vec<SaveStore> = m.save_stores.iter().filter(|s| !s.retired).cloned().collect();
    if v.is_empty() {
        v.push(SaveStore { id: DEFAULT_STORE.into(), name: "Default".into(), cloud: true, ..Default::default() });
    }
    if sync == Some(SaveSync::UbisoftNone) {
        v.iter_mut().for_each(|s| s.cloud = false);
    }
    v
}

/// One line with an icon, in a tone.
fn line(ic: Icon, color: gpui::Hsla, text: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .gap(px(6.))
        .text_size(px(11.5))
        .text_color(color)
        .child(div().pt(px(1.)).child(icon(ic, 12., color)))
        .child(div().flex_1().min_w_0().child(text.into()))
}

/// A row of store chips: each live store, then "New store". `chosen` is highlighted,
/// `recommended` gets a mark; clicking calls `pick` with the store id (or `new`).
fn store_chips(
    th: &Theme,
    key: &'static str,
    stores: &[SaveStore],
    chosen: &str,
    recommended: &str,
    pick: impl Fn(&mut Quiver, String, &mut Context<Quiver>) + Clone + 'static,
    cx: &mut Context<Quiver>,
) -> gpui::Div {
    let mut items: Vec<(String, String, Option<Icon>)> =
        stores.iter().map(|s| (s.id.clone(), s.name.clone(), Some(if s.cloud { Icon::Cloud } else { Icon::HardDrive }))).collect();
    items.push((NEW_STORE.into(), "New store".into(), Some(Icon::Plus)));
    div().flex().flex_wrap().gap(px(6.)).children(items.into_iter().enumerate().map(|(i, (id, name, ic))| {
        let on = id == chosen;
        let label = if id == recommended { format!("{name} · recommended") } else { name };
        let pick = pick.clone();
        chip(th, ic, label, on)
            .id((key, i))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                pick(this, id.clone(), cx);
                cx.notify();
            }))
            .into_any_element()
    }))
}

/// The new-profile dialog's saves section: the advice for the ticked layers and a choice
/// (`choice` `None`: the source profile's store, `inherit` (its id and the profile's name),
/// else the advice). `sync`: [`sync_of`].
pub fn new_profile_saves(
    th: &Theme,
    m: Option<&ModsState>,
    sync: Option<SaveSync>,
    layers: &[(String, String, bool)],
    choice: Option<&str>,
    inherit: Option<(String, String)>,
    cx: &mut Context<Quiver>,
) -> AnyElement {
    let Some(m) = m else { return div().into_any_element() };
    if !m.has_save_stores() {
        return real_saves(th, m, sync);
    }
    let ticked: Vec<String> = layers.iter().filter(|l| l.2).map(|l| l.0.clone()).collect();
    let advice = advice_for(m, &ticked);
    let recommended = if advice.share_default { DEFAULT_STORE } else { NEW_STORE };
    let stores = live(m, sync);
    let inherit = inherit.filter(|(s, _)| stores.iter().any(|x| &x.id == s));
    let chosen = choice.or(inherit.as_ref().map(|(s, _)| s.as_str())).unwrap_or(recommended);
    let chips = store_chips(th, "np-store", &stores, chosen, recommended, |this, id, _| {
        if let Some(Dialog::NewProfile { saves, .. }) = &mut this.gp.dialog {
            *saves = Some(id);
        }
    }, cx);
    let risky_shared = !advice.share_default && chosen != NEW_STORE;
    let chosen_store = stores.iter().find(|s| s.id == chosen);
    let cloud_store = stores.iter().find(|s| s.cloud && !s.retired);
    let synced = chosen_store.is_some_and(|s| s.cloud);
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(caption("Save store", th.tx3))
        .child(chips)
        .when_some(inherit.filter(|_| choice.is_none()), |d, (_, from)| {
            d.child(line(Icon::Layers, th.tx3, format!("The same store as {from}, so the copy plays the same saves.")))
        })
        .child(line(if advice.share_default { Icon::Info } else { Icon::TriangleAlert },
            if risky_shared { th.warn } else { th.tx3 },
            format!("Recommended: {}. {}", if advice.share_default { "Default" } else { "a new store" }, capitalize(&advice.reason)),
        ))
        .child(line(if synced { Icon::Cloud } else { Icon::HardDrive }, th.tx3, store_sync_note(chosen_store, cloud_store, sync) + "."))
        .into_any_element()
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

impl Quiver {
    /// [`sync_of`] with what the game page knows.
    pub fn save_sync(&self, m: &ModsState) -> Option<SaveSync> {
        sync_of(m, self.gp.info.ready().map(|i| i.features.cloud_saves))
    }

    pub fn load_stores(&mut self, id: Gid, cx: &mut Context<Self>) {
        if self.gp.stores.ready().is_none() {
            self.gp.stores = Load::Loading;
        }
        self.call::<SaveStoresReport, _>(Request::SaveStores { product_id: id.pid() }, cx, move |this, r, _| {
            if this.gp.id == id {
                this.gp.stores = match r {
                    Ok(s) => Load::Ready(s),
                    Err(e) => Load::Failed(crate::err_text(&e)),
                };
            }
        });
    }

    /// The profile editor's saves section: which store, the picker, the advice for its
    /// stack, and where its saves sync.
    pub(super) fn render_store_picker(&mut self, th: &Theme, g: &GameRow, m: &ModsState, p: &ProfileInfo, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        let sync = self.save_sync(m);
        if !m.has_save_stores() {
            return real_saves(th, m, sync);
        }
        let pid = p.profile.id.clone();
        let stores = live(m, sync);
        let current = p.profile.saves.clone().unwrap_or_default();
        let advice = advice_for(m, &p.profile.layers);
        let recommended = if advice.share_default { DEFAULT_STORE } else { NEW_STORE };
        let chips = store_chips(th, "prof-store", &stores, &current, recommended, move |this, store, cx| {
            this.profile_set(id, &pid, vec![SettingOp::Set { key: "saves".into(), value: store }], cx);
        }, cx);
        let shared_warning = p.warnings.iter().find(|w| w.contains("could change saves")).cloned();
        let saves = p.saves.clone().unwrap_or_default();
        let users: Vec<String> = m
            .profiles
            .iter()
            .filter(|o| o.profile.id != p.profile.id && o.profile.saves.as_deref() == Some(current.as_str()))
            .map(|o| o.profile.name.clone())
            .chain(m.vanilla_saves.as_ref().filter(|v| v.store.as_deref() == Some(current.as_str())).map(|_| "Vanilla".to_owned()))
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(12.5))
                    .child(div().text_color(th.tx2).child("Save store"))
                    .child(div().flex_1())
                    .child(if saves.cloud && sync != Some(SaveSync::UbisoftNone) {
                        pill(th, PillKind::Ok, Some(Icon::Cloud), "Synced")
                    } else {
                        pill(th, PillKind::Muted, Some(Icon::HardDrive), "This PC only")
                    }),
            )
            .child(chips)
            .child(line(Icon::Layers,
                th.tx3,
                if users.is_empty() { "No other profile uses this store.".to_owned() } else { format!("Shared with {}.", users.join(", ")) },
            ))
            .child(match (sync, saves.cloud_note.clone()) {
                // The daemon can't tell a Ubisoft game without cloud saves; the page can.
                (Some(SaveSync::UbisoftNone), _) => {
                    let s = stores.iter().find(|s| s.id == current);
                    line(Icon::HardDrive, th.tx3, store_sync_note(s, None, sync) + ".")
                }
                (_, Some(n)) => line(Icon::HardDrive, th.tx3, capitalize(&n) + "."),
                (_, None) => line(Icon::Cloud, th.tx3, format!("Syncs with {}.", cloud_name(quiver_api::StoreId::Ubisoft))),
            })
            .child(match shared_warning {
                Some(w) => line(Icon::TriangleAlert, th.warn, capitalize(&w.replace("give it a store of its own with saves=new", "pick New store to give it its own")) + "."),
                None => line(Icon::Info,
                    th.tx3,
                    format!("Recommended: {}. {}.", if advice.share_default { "Default" } else { "a store of its own" }, capitalize(&advice.reason)),
                ),
            })
            .into_any_element()
    }

    /// The Saves tab's list of stores.
    pub(super) fn render_stores_card(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        if self.gp.id == id && self.gp.stores.is_idle() {
            self.load_stores(id, cx);
        }
        let Some(r) = self.gp.stores.ready().filter(|_| self.gp.id == id).cloned() else {
            return card(th).child(card_header(th, "Save stores")).children(load_note(th, &self.gp.stores, "save stores")).into_any_element();
        };
        let cloud_store_name = r.cloud.as_ref().and_then(|c| r.stores.iter().find(|s| &s.store.id == c)).map(|s| s.store.name.clone());
        let no_cloud = self.gp.info.ready().is_some_and(|i| !i.features.cloud_saves);
        let rows: Vec<AnyElement> = r
            .stores
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let users: Vec<String> = s.used_by.iter().map(|u| u.name.clone()).collect();
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(16.))
                    .py(px(10.))
                    .when(i > 0, |d| d.border_t_1().border_color(th.line1))
                    .child(icon(if s.store.cloud && !no_cloud { Icon::Cloud } else { Icon::HardDrive }, 15., if s.store.cloud && !no_cloud { th.ok } else { th.tx3 }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(div().font_weight(SEMIBOLD).truncate().child(s.store.name.clone()))
                                    .when(s.store.cloud && !no_cloud, |d| d.child(pill(th, PillKind::Ok, None, "Cloud")))
                                    .when(!s.store.cloud || no_cloud, |d| d.child(pill(th, PillKind::Muted, None, "This PC only"))),
                            )
                            .child(div().text_size(px(11.5)).text_color(th.tx3).truncate().child(if users.is_empty() {
                                "Not used by any profile".to_owned()
                            } else {
                                format!("Used by {}", users.join(", "))
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_end()
                            .gap(px(2.))
                            .flex_none()
                            .child(div().text_size(px(12.)).child(format!("{} slot{}", s.slots, if s.slots == 1 { "" } else { "s" })))
                            .child(div().text_size(px(11.)).text_color(th.tx3).child(s.last_modified.map(|t| format!("changed {}", ago(t))).unwrap_or_else(|| "empty".into()))),
                    )
                    .child(mono(11., th.tx3).w(px(130.)).flex_none().truncate().child(s.store.folder.clone()))
                    .into_any_element()
            })
            .collect();
        card(th)
            .child(
                card_header(th, "Save stores")
                    .child(div().text_size(px(12.)).text_color(th.tx3).child(format!(
                        "{} · each profile picks one on the Profiles tab",
                        match (&cloud_store_name, no_cloud) {
                            (_, true) => "this game has no cloud saves: every store stays on this PC".to_owned(),
                            (Some(n), _) => format!("only {n} syncs with {}", cloud_name(quiver_api::StoreId::Ubisoft)),
                            (None, _) => format!("none syncs with {}", cloud_name(quiver_api::StoreId::Ubisoft)),
                        }
                    )))
                    .child(div().flex_1()),
            )
            .children(rows)
            .when(r.stores.len() > 1, |d| {
                d.child(
                    div()
                        .px(px(16.))
                        .py(px(9.))
                        .border_t_1()
                        .border_color(th.line1)
                        .child(line(Icon::Info,
                            th.tx3,
                            format!("To combine two stores, newest slot wins with backups first: quiver saves {id} merge STORE into default"),
                        )),
                )
            })
            .into_any_element()
    }
}
