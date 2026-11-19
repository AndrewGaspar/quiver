//! Game page: the list on the left, the game on the right (hero, action bar, stats, tabs).
//! Each tab is its own module; `page` holds the page's state and daemon calls.

mod addons;
mod epicsaves;
mod files;
mod launch;
mod logs;
mod mods;
mod options;
mod overview;
mod page;
mod profiles;
mod perf;
mod saves;
mod steamhook;
mod storepage;
mod stores;
pub(crate) mod tweaks;

use quiver_ui::depth::DepthExt;
use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, Window, div, prelude::*, px, relative};

pub use page::{ExportAction, GamePage, LaunchDraft, Load, PlayAfter, checkbox, kv, settings_from_reply, toggle};

use super::library::{play_button, primary_label};
use super::{
    ArtState, FocusExt, Icon, Kind, MEDIUM, PillKind, SEMIBOLD, Size, art_chip, art_fill, badge, button, caption, chip, cover,
    glyph, icon, icon_button, pill, soft_shadow,
};
use crate::app::{ACTION_FILES, ACTION_LAUNCH, ACTION_MORE, ACTION_OPTIONS, ACTION_PRIMARY, ACTION_STEAM, InputMode, Quiver, SideFilter};
use crate::art::{ArtPreference, ArtSlot, Variant};
use crate::model::{Gid, GameRow, day, gb, hours, short_name};
use crate::nav::{Tab, Target};
use crate::theme::{Theme, on_art, rgba};
use super::hero::{HeroTitle, TitleBox, title_element};
use super::responsive::{Bp, RAIL_WIDTH, SideMode, tip};

pub const SIDE_WIDTH: f32 = 288.;
pub const HERO: f32 = 312.;
/// Windows shorter than this scroll the whole game page, not just the tab's content.
const SHORT: f32 = 720.;


impl Quiver {
    pub fn render_game(&mut self, th: &Theme, id: Gid, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let _ = window;
        let row = div().flex().flex_1().min_h_0().relative();
        match self.side_mode() {
            SideMode::Full => row.child(self.render_side(th, id, SIDE_WIDTH, cx)).child(self.render_pane(th, id, cx)),
            SideMode::Rail => row.child(self.render_rail(th, id, cx)).child(self.render_pane(th, id, cx)),
            SideMode::Drawer => {
                // The list slides over the page: from the Games button, or when keyboard
                // or controller focus moves into it.
                let open = self.gp.side_open || (self.mode != InputMode::Pointer && self.nav.zone == crate::nav::Zone::Side);
                let w = (self.viewport.0 - 48.).min(SIDE_WIDTH + 24.);
                row.child(self.render_pane(th, id, cx)).when(open, |d| {
                    d.child(
                        div()
                            .id("side-scrim")
                            .absolute()
                            .inset_0()
                            .bg(th.scrim)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.gp.side_open = false;
                                if this.nav.zone == crate::nav::Zone::Side {
                                    this.nav.zone = crate::nav::Zone::Actions;
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .top_0()
                            .bottom_0()
                            .flex()
                            // Stereo: the drawer slides over the page as an overlay.
                            .depth_overlay()
                            .shadow(vec![soft_shadow(rgba(0x000000, 0.6), 0., 40.)])
                            .child(self.render_side(th, id, w, cx)),
                    )
                })
            }
        }
    }

    /// Narrow windows: the game list as a row of box art down the left edge.
    fn render_rail(&mut self, th: &Theme, selected: Gid, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let games: Vec<GameRow> = self.side_games().into_iter().cloned().collect();
        // Children line up with the full list's (`side_child_index`): an empty first
        // child where its header is, the divider where "Not installed" is.
        let mut rows: Vec<AnyElement> = vec![div().into_any_element()];
        let mut rest = false;
        for g in &games {
            let id = g.id();
            let active = g.installed() || g.install_job().is_some();
            if !active && !rest {
                rest = true;
                rows.push(div().mx(px(10.)).my(px(6.)).h(px(1.)).bg(th.line1).into_any_element());
            }
            let on = id == selected;
            let art = self.art_for(g, ArtSlot::Tile, if active { Variant::Tile } else { Variant::TileDimmed }, cx);
            let focused = self.focused(Target::SideRow(id));
            let dot = if g.running() {
                Some(th.ok)
            } else if g.installing().is_some() || g.has_update() {
                Some(th.acc)
            } else if g.failed().is_some() {
                Some(th.warn)
            } else {
                None
            };
            let hover = th.ink3;
            rows.push(
                div()
                    .id(("rail", id.ix()))
                    .relative()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .h(px(46.))
                    .mx(px(6.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(th.sel))
                    .when(!on, |d| d.hover(move |s| s.bg(hover)))
                    .focus_ring(focused, th, 7.)
                    .when(on, |d| d.child(div().absolute().left(px(-6.)).top(px(10.)).bottom(px(10.)).w(px(3.)).rounded(px(2.)).bg(th.acc)))
                    .child(div().w(px(30.)).h(px(36.)).child(art_fill(th, &art, "", 4., false)))
                    .when_some(dot, |d, c| {
                        d.child(div().absolute().top(px(4.)).right(px(4.)).size(px(8.)).rounded_full().border_1().border_color(th.ink0).bg(c))
                    })
                    .tooltip(tip(th, g.entry.name.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.nav.focus(&this.layout, Target::SideRow(id));
                        this.load_details(id, cx);
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        div()
            .id("side-rail")
            // Stereo: the game list is a surface beside the page.
            .depth_surface()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(2.))
            .w(px(RAIL_WIDTH))
            .min_h_0()
            .py(px(8.))
            .overflow_y_scroll()
            .track_scroll(&self.side_scroll)
            .bg(th.ink0)
            .border_r_1()
            .border_color(th.line1)
            .children(rows)
    }

    // -----------------------------------------------------------------------------------------
    // Side list

    fn render_side(&mut self, th: &Theme, selected: Gid, width: f32, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let games: Vec<GameRow> = self.side_games().into_iter().cloned().collect();
        let active = |g: &GameRow| g.on_disk() || g.install_job().is_some();
        let n_active = games.iter().filter(|g| active(g)).count();
        let n_rest = games.len() - n_active;
        let group = |label: &str, n: usize| {
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(8.))
                .pt(px(12.))
                .pb(px(6.))
                .child(caption(label, th.tx3))
                .child(badge(th, n.to_string(), false))
        };
        let mut children: Vec<AnyElement> = vec![group("Recent and installed", n_active).into_any_element()];
        let mut rest_header = false;
        for g in &games {
            if !active(g) && !rest_header {
                children.push(group("Not installed", n_rest).into_any_element());
                rest_header = true;
            }
            children.push(self.render_side_row(th, g, g.id() == selected, cx));
        }

        let filter_chip = |f: SideFilter, label: &'static str, cx: &mut Context<Self>| {
            let on = self.side_filter == f;
            chip(th, None, label, on).h(px(24.)).id(label).cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                this.side_filter = f;
                cx.notify();
            }))
        };
        let installed = self.games.iter().filter(|g| g.on_disk()).count();
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(width))
            .min_h_0()
            // Stereo: the game list is a surface beside the page.
            .depth_surface()
            .bg(th.ink0)
            .border_r_1()
            .border_color(th.line1)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .px(px(12.))
                    .pt(px(12.))
                    .pb(px(8.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .h(px(32.))
                            .px(px(10.))
                            .rounded(px(8.))
                            .bg(th.ink1)
                            .border_1()
                            .border_color(th.line2)
                            .text_color(th.tx3)
                            .text_size(px(12.5))
                            .child(icon(Icon::Search, 16., th.tx3))
                            .child(div().flex_1().min_w_0().truncate().child(format!("Filter {} games", self.games.len())))
                            .child(super::kbd(th, "/")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(filter_chip(SideFilter::All, "All", cx))
                            .child(filter_chip(SideFilter::Installed, "Installed", cx))
                            .child(div().flex_1())
                            .child(icon(Icon::ArrowDownUp, 14., th.tx3)),
                    ),
            )
            .child(
                div()
                    .id("side-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.side_scroll)
                    .px(px(8.))
                    .pb(px(8.))
                    .children(children),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .p(px(12.))
                    .border_t_1()
                    .border_color(th.line1)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(11.5))
                            .text_color(th.tx2)
                            .child(icon(Icon::HardDrive, 12., th.tx2))
                            .child(format!("{installed} installed"))
                            .child(div().flex_1())
                            .child(div().text_color(th.tx3).child(format!("{} owned", self.games.len()))),
                    )
                    .child(
                        button("import", th, Kind::Ghost, Size::Small, Some(Icon::FolderInput), "Import Connect downloads")
                            .justify_start()
                            .on_click(cx.listener(|this, _, _, cx| this.run_dl(crate::ui::downloads::DlCmd::Import, cx))),
                    ),
            )
    }

    fn render_side_row(&mut self, th: &Theme, g: &GameRow, on: bool, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        let off = !g.on_disk() && g.install_job().is_none();
        let art = self.art_for(g, ArtSlot::Tile, if off { Variant::TileDimmed } else { Variant::Tile }, cx);
        let focused = self.focused(Target::SideRow(id));
        let fg = if on { th.tx1 } else if off { th.tx3 } else { th.tx2 };
        let right: AnyElement = if g.running() {
            icon(Icon::Gamepad, 12., th.ok).into_any_element()
        } else if let Some(f) = g.installing() {
            div().text_size(px(11.)).text_color(th.acc_hi).child(format!("{:.0}%", (f * 100.).floor())).into_any_element()
        } else if g.paused() {
            div().text_size(px(11.)).text_color(th.tx3).child("Paused").into_any_element()
        } else if g.queued() {
            div().text_size(px(11.)).text_color(th.tx3).child("Queued").into_any_element()
        } else if g.failed().is_some() {
            icon(Icon::TriangleAlert, 12., th.warn).into_any_element()
        } else if g.has_update() {
            icon(Icon::ArrowDownToLine, 12., th.acc_hi).into_any_element()
        } else {
            div().into_any_element()
        };
        let hover = th.ink3;
        let hover_fg = th.tx1;
        div()
            .id(("side", id.ix()))
            .relative()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(34.))
            .px(px(8.))
            .rounded(px(7.))
            .text_size(px(12.5))
            .text_color(fg)
            .cursor_pointer()
            .when(on, |d| d.bg(th.sel))
            .when(!on, |d| d.hover(move |s| s.bg(hover).text_color(hover_fg)))
            .focus_ring(focused, th, 7.)
            .when(on, |d| d.child(div().absolute().left(px(-8.)).top(px(8.)).bottom(px(8.)).w(px(3.)).rounded(px(2.)).bg(th.acc)))
            .child(div().w(px(22.)).h(px(26.)).flex_none().child(art_fill(th, &art, "", 4., false)))
            .child(div().flex_1().min_w_0().truncate().child(short_name(&g.entry.name)))
            .child(right)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.nav.focus(&this.layout, Target::SideRow(id));
                this.load_details(id, cx);
                this.gp.side_open = false;
                cx.notify();
            }))
            .into_any_element()
    }

    // -----------------------------------------------------------------------------------------
    // Pane

    fn render_pane(&mut self, th: &Theme, id: Gid, cx: &mut Context<Self>) -> AnyElement {
        let Some(g) = self.game(id).cloned() else {
            return div().flex_1().p(px(28.)).text_color(th.tx3).child("This game isn't in your library.").into_any_element();
        };
        // Other stores' games have one page: their client's facts (no Quiver tabs).
        let store_page = !id.is_ubisoft();
        let tab = if Tab::of(id).contains(&self.nav.tab) { self.nav.tab } else { Tab::Overview };
        let slim = tab != Tab::Overview;
        let pane_w = self.pane_width();
        let pane_h = (self.viewport.1 - 52.).max(300.);
        let pad = self.pane_bp().pad();
        let amb = self.art_for(&g, ArtSlot::Hero, Variant::Ambient, cx);

        // Ambient tint from a blurred copy of the hero (no backdrop blur in GPUI).
        let ambient = div()
            .absolute()
            .inset_0()
            .overflow_hidden()
            .when_some(
                match &amb {
                    ArtState::Ready(a) => Some(a.clone()),
                    _ => None,
                },
                |d, a| {
                    d.child(
                        div()
                            .absolute()
                            .left(px(-pane_w * 0.1))
                            .top(px(-pane_h * 0.3))
                            .w(px(pane_w * 1.2))
                            .h(px(pane_h * 0.9))
                            .overflow_hidden()
                            .opacity(if th.dark { 0.28 } else { 0.18 })
                            .child(cover(&a, pane_w * 1.2, pane_h * 0.9, (0.5, 0.5))),
                    )
                },
            )
            .child(div().absolute().inset_0().bg(gpui::linear_gradient(
                180.,
                gpui::linear_color_stop(th.ink1.opacity(0.8), 0.),
                gpui::linear_color_stop(th.ink1, 0.6),
            )));

        let body = match tab {
            Tab::Overview if store_page => self.render_store_overview(th, &g, cx),
            Tab::Overview => self.render_overview(th, &g, cx),
            Tab::LaunchOptions => self.render_launch(th, &g, cx),
            Tab::Mods => self.render_mods(th, &g, cx),
            Tab::Tweaks => self.render_tweaks_tab(th, &g, cx),
            Tab::Performance => self.render_perf(th, &g, cx),
            Tab::Saves if store_page => self.render_epic_saves(th, &g, cx),
            Tab::Saves => self.render_saves(th, &g, cx),
            Tab::AddOns => self.render_addons(th, &g, cx),
            Tab::Files => self.render_files(th, &g, cx),
            Tab::Logs => self.render_logs(th, &g, cx),
        };

        let pending = match tab {
            Tab::LaunchOptions => self.render_pending(th, &g, cx),
            Tab::Mods => self.render_options_bar(th, &g, cx),
            Tab::Tweaks => self.render_tweaks_bar(th, cx),
            _ => None,
        };
        let mut pane = div()
            .flex_1()
            .min_w_0()
            .relative()
            .flex()
            .flex_col()
            .min_h_0()
            .overflow_hidden()
            .child(ambient);
        if self.side_mode() == SideMode::Drawer {
            pane = pane.child(self.render_drawer_bar(th, &g, cx));
        }
        let mut head: Vec<AnyElement> = Vec::new();
        if slim {
            head.push(self.render_slim_hero(th, &g, pane_w, cx).into_any_element());
        } else {
            head.push(self.render_hero(th, &g, pane_w, cx).into_any_element());
            head.push(self.render_actionbar(th, &g, false, cx).into_any_element());
            head.push(self.render_stats(th, &g).into_any_element());
        }
        head.push(self.render_tabs(th, &g, cx).into_any_element());
        let scroll = div().id(("body", id.ix())).flex_1().min_h_0().overflow_y_scroll().track_scroll(&self.body_scroll);
        let body = div().px(px(pad)).pt(px(20.)).pb(px(28.)).child(body);
        // Short or narrow windows: the hero and tabs scroll away with the tab's content,
        // which would otherwise get a sliver of the window.
        let pane = if self.viewport.1 < SHORT || self.pane_bp().narrow() {
            pane.child(scroll.flex().flex_col().children(head).child(body))
        } else {
            pane.children(head).child(scroll.child(body))
        };
        pane.children(pending)
            .when_some(self.render_dialog(th, cx), |d, dlg| d.child(dlg))
            .into_any_element()
    }

    fn render_hero(&mut self, th: &Theme, g: &GameRow, pane_w: f32, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let hero = self.art_for(g, ArtSlot::Hero, Variant::Original, cx);
        let title = self.hero_title(g, cx);
        let pb = self.pane_bp();
        let pad = pb.pad();
        // Shorter windows get a shorter hero, so the tab content keeps some room.
        let hero_h = (self.viewport.1 * 0.36).clamp(200., HERO);
        let mut hero_el = div().relative().h(px(hero_h)).flex_none().overflow_hidden();
        hero_el = match &hero {
            ArtState::Ready(a) => hero_el.child(cover(a, pane_w, hero_h, (0.5, 0.3))),
            other => hero_el.child(super::art_placeholder(th, other, "", px(0.))),
        };
        hero_el = hero_el
            .child(div().absolute().inset_0().w(relative(0.55)).bg(gpui::linear_gradient(
                90.,
                gpui::linear_color_stop(rgba(0x080a0e, 0.5), 0.),
                gpui::linear_color_stop(rgba(0x080a0e, 0.), 1.),
            )))
            .child(div().absolute().left_0().right_0().bottom_0().h(relative(if th.dark { 0.65 } else { 0.3 })).bg(
                gpui::linear_gradient(180., gpui::linear_color_stop(th.ink1.opacity(0.), 0.), gpui::linear_color_stop(th.ink1, 1.)),
            ));
        // The logo fits inside 300×120, anchored top-left.
        let tb = if pb.narrow() {
            // Under the art chip, which takes the top right.
            TitleBox { max_w: (pane_w - 2. * pad).min(260.), max_h: 100., max_scale: 1.3, text: 26., text_w: pane_w - 2. * pad }
        } else {
            TitleBox { max_w: 300., max_h: 120., max_scale: 1.5, text: 30., text_w: 520f32.min(pane_w - 2. * pad - 220.) }
        };
        if let Some(t) = title_element(&title, &g.entry.name, tb) {
            let top = if pb.narrow() { 64. } else { 28. };
            hero_el = hero_el.child(div().absolute().left(px(pad)).top(px(top)).child(t));
        }
        let art_chip_el = self.render_art_chip(th, g, cx);
        hero_el.child(
            div()
                .absolute()
                .right(px(pad.min(24.)))
                .top(px(if pb.narrow() { 14. } else { 20. }))
                .flex()
                .gap(px(6.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(32.))
                        .rounded(px(8.))
                        .bg(on_art::glass())
                        .border_1()
                        .border_color(rgba(0xffffff, 0.09))
                        .child(icon(Icon::Star, 14., on_art::tx2())),
                )
                .child(art_chip_el),
        )
    }

    /// Whose art the page shows ("Art: Ubisoft", "Art: Steam", "Art: yours"). For a game
    /// with a Steam copy it opens a menu that puts Ubisoft's or Steam's art first (the
    /// game's `art` setting, which the library, Now Playing and the home hero follow too).
    fn render_art_chip(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        let custom = self.custom_art.get(&id.pid()).is_some_and(|c| c.hero.is_some() || c.tile.is_some() || c.logo.is_some());
        let shown = [ArtSlot::Hero, ArtSlot::Tile, ArtSlot::Logo]
            .into_iter()
            .find_map(|slot| match self.art_pick(g, slot, Variant::Original, cx) {
                (ArtState::Failed, _) => None,
                (_, c) => c.and_then(|c| c.origin),
            });
        let label = if custom {
            "Art: yours".to_owned()
        } else {
            match shown {
                Some(o) => format!("Art: {}", o.vendor()),
                None => "Art: none".to_owned(),
            }
        };
        let tip_text = shown.map(|o| format!("From the {}", o.describe())).unwrap_or_else(|| "No art from Ubisoft or Steam".into());
        let Some(app_id) = g.steam().map(|s| s.app_id) else {
            return art_chip(Some(Icon::Image), label).h(px(32.)).id("art-chip").tooltip(tip(th, &tip_text)).into_any_element();
        };
        let pref = self.art_pref(g);
        let open = self.gp.art_menu;
        let chip = art_chip(Some(Icon::Image), label)
            .h(px(32.))
            .child(icon(Icon::ChevronDown, 12., on_art::tx2()))
            .id("art-chip")
            .cursor_pointer()
            .tooltip(tip(th, &tip_text))
            .on_click(cx.listener(|this, _, _, cx| {
                this.gp.art_menu = !this.gp.art_menu;
                cx.notify();
            }));
        let menu = open.then(|| {
            let opts = [
                (ArtPreference::Ubisoft, "Ubisoft's art first", "Product config, then Ubisoft's games API, then Steam".to_owned()),
                (ArtPreference::Steam, "Steam's art first", format!("Steam app {app_id}'s library art, then Ubisoft's")),
            ];
            let items: Vec<AnyElement> = opts
                .into_iter()
                .enumerate()
                .map(|(i, (p, name, detail))| {
                    let on = p == pref;
                    div()
                        .id(("art-opt", i))
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .px(px(10.))
                        .py(px(7.))
                        .rounded(px(7.))
                        .cursor_pointer()
                        .hover(|s| s.bg(th.ink3))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .child(div().w(px(14.)).child(if on { icon(Icon::Check, 14., th.acc_hi).into_any_element() } else { div().into_any_element() }))
                                .child(div().font_weight(if on { SEMIBOLD } else { MEDIUM }).child(name)),
                        )
                        .child(div().pl(px(22.)).text_size(px(11.5)).text_color(th.tx3).child(detail))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.gp.art_menu = false;
                            this.set_art_pref(id, p, cx);
                            cx.notify();
                        }))
                        .into_any_element()
                })
                .collect();
            gpui::deferred(
                gpui::anchored().anchor(gpui::Anchor::TopRight).offset(gpui::point(px(0.), px(36.))).snap_to_window_with_margin(px(8.)).child(
                    div()
                        .id("art-menu")
                        .depth_overlay()
                        .occlude()
                        .w(px(300.))
                        .p(px(4.))
                        .flex()
                        .flex_col()
                        .rounded(px(10.))
                        .bg(th.ink2)
                        .border_1()
                        .border_color(th.line2)
                        .shadow(vec![soft_shadow(rgba(0x000000, 0.55), 14., 40.)])
                        .text_size(px(12.5))
                        .text_color(th.tx1)
                        .on_mouse_down_out(cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                            this.gp.art_menu = false;
                            cx.notify();
                        }))
                        .children(items),
                ),
            )
            .with_priority(1)
        });
        // The menu hangs from the chip's right edge.
        div().relative().child(chip).children(menu.map(|m| div().absolute().right_0().top_0().child(m))).into_any_element()
    }

    /// Save the game's `art` setting (`steam`, or unset for Ubisoft's first).
    pub fn set_art_pref(&mut self, id: Gid, p: ArtPreference, cx: &mut Context<Self>) {
        if self.game(id).is_some_and(|g| self.art_pref(g) == p) {
            return;
        }
        let op = match p {
            ArtPreference::Steam => quiver_api::SettingOp::Set { key: "art".into(), value: "steam".into() },
            ArtPreference::Ubisoft => quiver_api::SettingOp::Unset { key: "art".into() },
        };
        let message = Some(match p {
            ArtPreference::Steam => "Steam's art first".to_owned(),
            ArtPreference::Ubisoft => "Ubisoft's art first".to_owned(),
        });
        self.call::<quiver_api::Revision, _>(quiver_api::Request::SettingsSet { game: id.0.into(), ops: vec![op], message }, cx, move |this, r, cx| match r {
            Ok(_) => this.load_details(id, cx),
            Err(e) => this.show_error(&e, cx),
        });
    }

    /// The 112 px banner over the dense tabs: logo or name, then the action bar.
    fn render_slim_hero(&mut self, th: &Theme, g: &GameRow, pane_w: f32, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        // Below Medium the title and the action bar stack.
        let stacked = !self.pane_bp().two_columns();
        let pad = self.pane_bp().pad();
        let slim_h: f32 = if stacked { 124. } else { 112. };
        let hero = self.art_for(g, ArtSlot::Hero, Variant::Original, cx);
        let title = self.hero_title(g, cx);
        let baked = title == HeroTitle::Baked;
        let mut el = div().relative().h(px(slim_h)).flex_none().overflow_hidden();
        el = match &hero {
            // Skip logos painted into the art.
            ArtState::Ready(a) => el.child(cover(a, pane_w, slim_h, (0.5, if baked { 0.92 } else { 0.32 }))),
            other => el.child(super::art_placeholder(th, other, "", px(0.))),
        };
        el = el
            .child(div().absolute().inset_0().bg(gpui::linear_gradient(
                90.,
                gpui::linear_color_stop(rgba(0x080a0e, 0.82), 0.),
                gpui::linear_color_stop(rgba(0x080a0e, 0.25), 1.),
            )))
            .child(div().absolute().left_0().right_0().bottom_0().h(px(36.)).bg(gpui::linear_gradient(
                180.,
                gpui::linear_color_stop(th.ink1.opacity(0.), 0.),
                gpui::linear_color_stop(th.ink1.opacity(0.85), 1.),
            )));
        // The crop above hides a painted-in logo, so baked art gets the separate logo here.
        let title = if baked { self.logo_title(g, cx) } else { title };
        let tb = if stacked {
            TitleBox { max_w: 180., max_h: 36., max_scale: 1.2, text: 18., text_w: pane_w - 2. * pad }
        } else {
            TitleBox { max_w: 230., max_h: 60., max_scale: 1.2, text: 24., text_w: 420. }
        };
        let title: AnyElement = title_element(&title, &g.entry.name, tb).unwrap_or_else(|| div().into_any_element());
        let bar = self.render_actionbar(th, g, true, cx);
        if stacked {
            return el.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(12.))
                    .px(px(pad))
                    .child(div().h(px(36.)).flex().items_center().overflow_hidden().child(title))
                    .child(bar),
            );
        }
        el.child(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .gap(px(16.))
                .px(px(28.))
                .child(title)
                .child(div().flex_1())
                .child(bar),
        )
    }

    fn render_actionbar(&mut self, th: &Theme, g: &GameRow, slim: bool, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = g.id();
        let steam_profile = self.render_steam_profile_chip(th, g, slim, cx);
        let pad = self.mode == InputMode::Pad;
        let f = |i| self.focused(Target::Action(i));
        let (label, i) = primary_label(g);
        let (main, more) = play_button(th, !slim, label, i, pad, f(ACTION_PRIMARY), f(ACTION_OPTIONS));
        let play = div()
            .flex()
            .flex_none()
            .rounded(px(10.))
            .shadow(vec![soft_shadow(th.acc.opacity(0.28), 8., 30.)])
            .cursor_pointer()
            .child(main.id("act-play").on_click(cx.listener(|this, _, _, cx| this.click(Target::Action(ACTION_PRIMARY), cx))))
            .child(more.id("act-more").on_click(cx.listener(|this, _, _, cx| {
                this.gp.default_menu = false;
                this.click(Target::Action(ACTION_OPTIONS), cx)
            })));
        let proton = g.settings.as_ref().and_then(|s| s.proton.clone()).or_else(|| self.status.as_ref().and_then(|s| s.proton_default.clone()));
        let rev = g.current_revision();
        let proton_chip = match (proton, rev) {
            (Some(p), Some(r)) => Some(format!("{p} · r{r}")),
            (Some(p), None) => Some(p),
            (None, Some(r)) => Some(format!("r{r}")),
            _ => None,
        };
        // The game's default profile (what Play runs), once it has any; picking one here
        // changes the default.
        let profiles = if self.gp.id == id { self.gp.profiles() } else { vec![] };
        let profile_chip = (g.installed() && !profiles.is_empty()).then(|| {
            let cur = self.default_profile(id).and_then(|p| profiles.iter().find(|x| x.id == p).map(|x| x.name.clone()));
            let on = cur.is_some();
            let label = cur.unwrap_or_else(|| "Vanilla".into());
            let (bg, fg, line) = if slim { (rgba(0x0e1016, 0.62), on_art::tx1(), on_art::line()) } else { (th.ink2, th.tx1, th.line2) };
            div()
                .id("act-profile")
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.))
                .h(px(if slim { 30. } else { 36. }))
                .px(px(12.))
                .rounded(px(8.))
                .bg(bg)
                .border_1()
                .border_color(if on { th.acc_line } else { line })
                .text_color(fg)
                .font_weight(MEDIUM)
                .cursor_pointer()
                .when(pad, |d| d.child(glyph(th, "X", false)))
                .child(icon(Icon::Layers, 14., if on { th.acc_hi } else { fg }))
                .child(label)
                .child(icon(Icon::ChevronDown, 12., fg))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.gp.default_menu = true;
                    this.click(Target::Action(ACTION_OPTIONS), cx)
                }))
        });
        // Another store's game: Play (its client starts it), its state, and the menu. No
        // shortcuts, files or launch options: those are Quiver's own launches'. A Steam game
        // with mod profiles: the chip says which one Play in Steam runs.
        if let Some(c) = g.client() {
            let label = g.entry.store().label();
            let (kind, i, text) = if g.running() {
                (PillKind::Ok, Icon::Gamepad, "Playing now".to_owned())
            } else if let Some(u) = &c.update {
                (PillKind::Warn, Icon::ArrowDownToLine, u.describe(label))
            } else if !c.complete {
                (PillKind::Warn, Icon::Download, format!("Downloading in {label}"))
            } else if c.update_pending {
                (PillKind::Warn, Icon::ArrowDownToLine, format!("Update waiting in {label}"))
            } else {
                (PillKind::Ok, Icon::Gamepad, format!("Installed via {label}"))
            };
            let more_btn = icon_button("act-menu", th, Icon::Ellipsis, true)
                .bg(th.ink2)
                .focus_ring(f(ACTION_MORE), th, 8.)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.gp.play_menu = false;
                    this.click(Target::Action(ACTION_MORE), cx)
                }));
            // The client updates it; Quiver can open the client's downloads (its queue).
            let open_downloads = c.update.is_some().then(|| {
                button("act-client-update", th, Kind::Secondary, Size::Medium, Some(Icon::ArrowDownToLine), format!("Open {label} downloads"))
                    .on_click(cx.listener(move |this, _, _, cx| this.update_in_client(id, cx)))
            });
            let pad = if slim { 0. } else { self.pane_bp().pad() };
            return div()
                .flex()
                .flex_none()
                .flex_wrap()
                .items_center()
                .gap(px(10.))
                .px(px(pad))
                .when(!slim, |d| d.mt(px(-58.)))
                .child(play)
                .children(steam_profile)
                .child(pill(th, kind, Some(i), text).py(px(5.)))
                .children(open_downloads)
                .child(div().flex_1())
                .child(more_btn);
        }
        let status: Option<(PillKind, Icon, String)> = if g.running() {
            Some((PillKind::Ok, Icon::Gamepad, "Playing now".into()))
        } else if let Some(fr) = g.installing() {
            Some((PillKind::Accent, Icon::Download, format!("Installing {:.0}%", (fr * 100.).floor())))
        } else if g.queued() {
            Some((PillKind::Muted, Icon::Download, "Queued".into()))
        } else if g.installed() && g.entry.update_available {
            Some((PillKind::Accent, Icon::ArrowDownToLine, "Update available".into()))
        } else if g.installed() && !slim {
            Some((PillKind::Ok, Icon::CircleCheck, "Installed".into()))
        } else if g.steam_only() {
            Some((PillKind::Ok, Icon::Gamepad, "Installed via Steam".into()))
        } else {
            None
        };
        // Bought on Steam: Ubisoft has no build of its own for this account.
        let steam_owned = (g.steam_only() && !g.entry.installable).then(|| {
            let label = if g.entry.owned_by.is_empty() { "On Steam · not on your Ubisoft account" } else { "Owned on Steam" };
            pill(th, PillKind::Muted, None, label).py(px(5.))
        });
        // A Steam copy: the button switches it to Quiver (or shows that it is) instead of
        // adding a shortcut.
        let copy = g.steam().and_then(|s| self.steam_copy(id.pid()).or_else(|| Some(s.clone())));
        // Add to Steam shows the shortcut's state.
        let steam_state = self.gp.export(id, quiver_api::exports::ExportKind::Steam).map(|e| e.state);
        use quiver_api::exports::ExportState as S;
        let (steam_label, steam_icon, steam_tone) = match steam_state {
            None => ("Add to Steam", Icon::Gamepad, None),
            Some(S::Ok) => ("In Steam", Icon::CircleCheck, Some(th.ok)),
            Some(S::WaitingForSteam) => ("Restart Steam to show", Icon::RefreshCw, Some(th.warn)),
            Some(S::Missing) | Some(S::Changed) => ("Add to Steam again", Icon::TriangleAlert, Some(th.warn)),
        };
        let (steam_label, steam_icon, steam_tone) = match &copy {
            Some(c) if g.steam_only() => match (crate::ui::steamgames::control(c), c.state) {
                (_, quiver_api::exports::PatchState::Waiting) => ("Quiver after Steam restarts", Icon::RefreshCw, Some(th.warn)),
                (Some(false), _) => ("Starts through Quiver", Icon::CircleCheck, Some(th.ok)),
                (Some(true), _) => ("Use Quiver", Icon::Gamepad, Some(th.acc_hi)),
                (None, _) => ("Opens Connect first", Icon::Gamepad, None),
            },
            _ => (steam_label, steam_icon, steam_tone),
        };
        let dim_steam = !g.installed() && !g.steam_only();
        let steam_tip = tip(th, steam_label);
        let steam = if slim || !self.pane_bp().two_columns() {
            icon_button("act-steam", th, steam_icon, true)
                .bg(th.ink2)
                .relative()
                .tooltip(steam_tip)
                .when_some(steam_tone, |d, c| d.child(div().absolute().top(px(5.)).right(px(5.)).size(px(6.)).rounded_full().bg(c)))
        } else {
            let b = button("act-steam", th, Kind::Secondary, Size::Medium, None, "");
            let fg = steam_tone.unwrap_or(th.tx1);
            b.child(icon(steam_icon, 16., fg)).child(div().text_color(fg).child(steam_label))
        }
        .when(dim_steam, |d| d.opacity(0.5))
        .focus_glow(f(ACTION_STEAM), th, 8.)
        .on_click(cx.listener(|this, _, _, cx| this.click(Target::Action(ACTION_STEAM), cx)));
        let files = icon_button("act-files", th, Icon::FolderOpen, true)
            .bg(th.ink2)
            .tooltip(tip(th, "Files"))
            .focus_ring(f(ACTION_FILES), th, 8.)
            .on_click(cx.listener(|this, _, _, cx| this.click(Target::Action(ACTION_FILES), cx)));
        let launch = icon_button("act-launch", th, Icon::SlidersHorizontal, true)
            .bg(th.ink2)
            .tooltip(tip(th, "Launch options"))
            .focus_ring(f(ACTION_LAUNCH), th, 8.)
            .on_click(cx.listener(|this, _, _, cx| this.click(Target::Action(ACTION_LAUNCH), cx)));
        let more_btn = if pad {
            button("act-menu", th, Kind::Secondary, Size::Medium, None, "").px(px(8.)).child(glyph(th, "Y", false)).child(icon(Icon::Ellipsis, 16., th.tx2))
        } else {
            icon_button("act-menu", th, Icon::Ellipsis, true).bg(th.ink2)
        }
        .focus_ring(f(ACTION_MORE), th, 8.)
        .on_click(cx.listener(|this, _, _, cx| {
            this.gp.play_menu = false;
            this.click(Target::Action(ACTION_MORE), cx)
        }));

        let pb = self.pane_bp();
        if slim && !pb.two_columns() {
            // Stacked slim banner: Play and the buttons; the chips are on the tabs themselves.
            return div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.))
                .child(play)
                .when(!pb.narrow(), |d| d.children(profile_chip))
                .child(div().flex_1())
                .child(steam)
                .child(files)
                .child(launch)
                .child(more_btn);
        }
        if slim {
            return div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.))
                .when_some(status, |d, (k, i, s)| d.child(pill(th, k, Some(i), s).py(px(5.))))
                .when(!g.steam_only(), |d| d.when_some(proton_chip.clone(), |d, p| d.child(art_chip(Some(Icon::Cpu), p).h(px(30.)))))
                .children(profile_chip)
                .child(play)
                .child(div().w(px(4.)))
                .child(steam)
                .child(files)
                .child(launch)
                .child(more_btn);
        }
        if !pb.two_columns() {
            // Two rows: Play and the buttons, then the chips (wrapping).
            let pad = pb.pad();
            let (top_profile, chip_profile) = if pb.narrow() { (None, profile_chip) } else { (profile_chip, None) };
            return div()
                .flex()
                .flex_col()
                .flex_none()
                .gap(px(10.))
                .px(px(pad))
                .mt(px(-58.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(play)
                        .children(top_profile)
                        .child(div().flex_1())
                        .child(steam)
                        .child(files)
                        .child(launch)
                        .child(more_btn),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(8.))
                        .children(chip_profile)
                        .when(!g.steam_only(), |d| d.when_some(proton_chip, |d, p| d.child(chip(th, Some(Icon::Cpu), p, false).h(px(30.)))))
                        .when_some(status, |d, (k, i, s)| d.child(pill(th, k, Some(i), s).py(px(5.))))
                        .children(steam_owned),
                );
        }
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .px(px(28.))
            .mt(px(-58.))
            .child(play)
            .children(profile_chip)
            .when(!g.steam_only(), |d| d.when_some(proton_chip, |d, p| d.child(chip(th, Some(Icon::Cpu), p, false).h(px(30.)))))
            .when_some(status, |d, (k, i, s)| d.child(pill(th, k, Some(i), s).py(px(5.))))
            .children(steam_owned)
            .child(div().flex_1())
            .child(steam)
            .child(files)
            .child(launch)
            .child(more_btn)
    }

    /// Narrow windows: a bar over the game with the button that opens the game list.
    fn render_drawer_bar(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let n = self.games.len();
        div()
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .h(px(44.))
            .px(px(10.))
            .bg(th.ink0)
            .border_b_1()
            .border_color(th.line1)
            .child(
                button("side-open", th, Kind::Secondary, Size::Small, Some(Icon::List), format!("Games {n}"))
                    .child(icon(Icon::ChevronDown, 12., th.tx2))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.gp.side_open = !this.gp.side_open;
                        cx.notify();
                    })),
            )
            .child(div().flex_1().min_w_0().truncate().font_weight(SEMIBOLD).child(g.entry.name.clone()))
    }

    fn render_stats(&self, th: &Theme, g: &GameRow) -> impl IntoElement + use<> {
        let id = g.id();
        let pt = g.playtime.clone().unwrap_or_default();
        let ubi = self.ubisoft_playtime(id);
        let info = self.gp.info.ready().filter(|_| self.gp.id == id);
        let (account, override_) = self.launch_account(id);
        let stats: Vec<(&str, String)> = if !id.is_ubisoft() { self.store_stats(g) } else { vec![
            ("Last played", g.last_played().map(day).unwrap_or_else(|| "Never".into())),
            (
                "Play time",
                match ubi {
                    Some((s, _)) => hours(s),
                    None if pt.total_seconds > 0 => hours(pt.total_seconds),
                    None => "—".into(),
                },
            ),
            ("Sessions", pt.sessions.len().to_string()),
            (
                "Installed",
                if let Some(s) = info.and_then(|i| i.install.as_ref()).map(|i| i.size_bytes).filter(|s| *s > 0) {
                    gb(s)
                } else if g.steam_only() {
                    "Via Steam".into()
                } else if g.installed() {
                    "Yes".into()
                } else if let Some(f) = g.installing() {
                    format!("{:.0}%", (f * 100.).floor())
                } else {
                    "No".into()
                },
            ),
            (
                "Version",
                info.and_then(|i| i.install.as_ref()).and_then(|i| i.version.clone()).unwrap_or_else(|| {
                    if g.installed() {
                        "—".into()
                    } else if g.steam_only() {
                        "Steam's".into()
                    } else {
                        "Not installed".into()
                    }
                }),
            ),
            ("Account", format!("{account} · {}", if override_ { "this game" } else { "default" })),
        ] };
        let n = stats.len();
        let pb = self.pane_bp();
        if !pb.two_columns() {
            // Below Medium, a grid: three columns (two when narrow), no separators.
            let pad = pb.pad();
            let cols = if pb.narrow() { 2 } else { 3 };
            let cell = ((self.pane_width() - 2. * pad - 16. * (cols - 1) as f32) / cols as f32).floor();
            return div()
                .flex()
                .flex_wrap()
                .flex_none()
                .gap_x(px(16.))
                .gap_y(px(10.))
                .px(px(pad))
                .pt(px(14.))
                .pb(px(16.))
                .children(stats.into_iter().map(|(k, v)| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .w(px(cell))
                        .child(caption(k, th.tx3))
                        .child(div().text_size(px(14.)).font_weight(SEMIBOLD).truncate().child(v))
                }));
        }
        let sp = if pb >= Bp::Large { 26. } else { 16. };
        div().flex().flex_none().px(px(28.)).pt(px(14.)).pb(px(16.)).children(stats.into_iter().enumerate().map(|(i, (k, v))| {
            div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .when(i + 1 < n, |d| d.pr(px(sp)).mr(px(sp)).border_r_1().border_color(th.line1))
                .child(caption(k, th.tx3))
                .child(div().text_size(px(14.)).font_weight(SEMIBOLD).child(v))
        }))
    }

    fn render_tabs(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let pad = self.mode == InputMode::Pad;
        let current = self.nav.tab;
        let id = g.id();
        let mods_n = if self.gp.id == id { self.gp.mods.ready().map(|m| m.profiles.len()) } else { None };
        let addons_n = if self.gp.id == id { self.gp.info.ready().map(|i| i.addons.iter().filter(|a| a.visible || a.owned).count()) } else { None };
        let side = self.pane_bp().pad();
        let unsaved = !self.launch_pending(id, cx).is_empty();
        let tweaks_unsaved = self.tweaks_unsaved(&self.tweaks_game_scope(id));
        div()
            .flex()
            .flex_wrap()
            .flex_none()
            .items_center()
            .gap(px(2.))
            .px(px(if side >= 28. { 28. } else { side - 8. }))
            .border_b_1()
            .border_color(th.line1)
            .when(pad, |d| d.child(glyph(th, "LT", true).mr(px(6.))))
            .children(Tab::of(id).iter().map(|&t| {
                let on = t == current;
                let focused = self.focused(Target::Tab(t));
                let count = match t {
                    Tab::Mods => mods_n.filter(|n| *n > 0),
                    Tab::AddOns => addons_n.filter(|n| *n > 0),
                    _ => None,
                };
                div()
                    .id(("tab", t.index()))
                    .relative()
                    .flex()
                    .items_center()
                    .pt(px(8.))
                    .pb(px(7.))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.nav.focus(&this.layout, Target::Tab(t));
                        this.nav.select_tab(t);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .px(px(12.))
                            .py(px(3.))
                            .rounded(px(6.))
                            .text_size(px(13.))
                            .font_weight(MEDIUM)
                            .text_color(if on { th.tx1 } else { th.tx2 })
                            .focus_ring(focused, th, 6.)
                            .child(t.label())
                            .when_some(count, |d, n| d.child(badge(th, n.to_string(), false)))
                            // Unsaved launch-options edits and tweaks (kept while elsewhere).
                            .when(t == Tab::LaunchOptions && unsaved, |d| d.child(div().size(px(7.)).rounded_full().bg(th.warn)))
                            .when(t == Tab::Tweaks && tweaks_unsaved, |d| d.child(div().size(px(7.)).rounded_full().bg(th.warn))),
                    )
                    .when(on, |d| d.child(div().absolute().left(px(10.)).right(px(10.)).bottom(px(-1.)).h(px(2.)).rounded(px(2.)).bg(th.acc)))
            }))
            .when(pad, |d| d.child(glyph(th, "RT", true).ml(px(6.))))
    }
}
