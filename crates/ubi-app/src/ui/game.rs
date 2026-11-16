//! Game page: the list on the left, the game on the right (hero, action bar, stats, tabs).

use gpui::{AnyElement, Context, FontWeight, IntoElement, ParentElement, Styled, Window, div, img, prelude::*, px, relative};

use super::library::{play_button, primary_label};
use super::{
    ArtState, FocusExt, Icon, Kind, MEDIUM, PillKind, SEMIBOLD, Size, art_chip, art_fill, badge, button, caption, card,
    card_header, chip, command_line, cover, glyph, icon, icon_button, mono, pill, soft_shadow,
};
use crate::app::{
    ACTION_FILES, ACTION_LAUNCH, ACTION_MORE, ACTION_OPTIONS, ACTION_PRIMARY, ACTION_STEAM, InputMode, Quiver, SideFilter,
};
use crate::art::{ArtSlot, Variant};
use crate::model::{GameRow, actor, actor_label, ago, clock, day, duration, hours, short_name, tilde};
use crate::nav::{Tab, Target};
use crate::theme::{Theme, on_art, rgba};

pub const SIDE_WIDTH: f32 = 288.;
pub const HERO: f32 = 312.;

/// Heroes that already have the logo painted in, so Quiver doesn't draw it again.
const BAKED_LOGO: &[u32] = &[3539, 3600];

impl Quiver {
    pub fn render_game(&mut self, th: &Theme, id: u32, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let _ = window;
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .child(self.render_side(th, id, cx))
            .child(self.render_pane(th, id, cx))
    }

    // -----------------------------------------------------------------------------------------
    // Side list

    fn render_side(&mut self, th: &Theme, selected: u32, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let games: Vec<GameRow> = self.side_games().into_iter().cloned().collect();
        let active = |g: &GameRow| g.installed() || g.install_job().is_some();
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
        let installed = self.games.iter().filter(|g| g.installed()).count();
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(SIDE_WIDTH))
            .min_h_0()
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
                            .child(div().flex_1().child(format!("Filter {} games", self.games.len())))
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
                        button("import", th, Kind::Ghost, Size::Small, Some(Icon::FolderInput), "Import existing games")
                            .justify_start()
                            .on_click(cx.listener(|this, _, _, cx| this.show_toast("Import: use `quiver import` for now", cx))),
                    ),
            )
    }

    fn render_side_row(&mut self, th: &Theme, g: &GameRow, on: bool, cx: &mut Context<Self>) -> AnyElement {
        let id = g.id();
        let off = !g.installed() && g.install_job().is_none();
        let art = self.art_for(g, ArtSlot::Tile, if off { Variant::Dimmed } else { Variant::Original }, cx);
        let focused = self.focused(Target::SideRow(id));
        let fg = if on { th.tx1 } else if off { th.tx3 } else { th.tx2 };
        let right: AnyElement = if g.running() {
            icon(Icon::Gamepad, 12., th.ok).into_any_element()
        } else if let Some(f) = g.installing() {
            div().text_size(px(11.)).text_color(th.acc_hi).child(format!("{:.0}%", f * 100.)).into_any_element()
        } else if g.queued() {
            div().text_size(px(11.)).text_color(th.tx3).child("Queued").into_any_element()
        } else if g.failed().is_some() {
            icon(Icon::TriangleAlert, 12., th.warn).into_any_element()
        } else if g.installed() && g.entry.update_available {
            icon(Icon::ArrowDownToLine, 12., th.acc_hi).into_any_element()
        } else {
            div().into_any_element()
        };
        let hover = th.ink3;
        let hover_fg = th.tx1;
        div()
            .id(("side", id as usize))
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
            .focus_if(focused, th)
            .when(on, |d| d.child(div().absolute().left(px(-8.)).top(px(8.)).bottom(px(8.)).w(px(3.)).rounded(px(2.)).bg(th.acc)))
            .child(div().w(px(22.)).h(px(26.)).flex_none().child(art_fill(th, &art, "", 4., false)))
            .child(div().flex_1().min_w_0().truncate().child(short_name(&g.entry.name)))
            .child(right)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.nav.focus(&this.layout, Target::SideRow(id));
                this.load_details(id, cx);
                cx.notify();
            }))
            .into_any_element()
    }

    // -----------------------------------------------------------------------------------------
    // Pane

    fn render_pane(&mut self, th: &Theme, id: u32, cx: &mut Context<Self>) -> AnyElement {
        let Some(g) = self.game(id).cloned() else {
            return div().flex_1().p(px(28.)).text_color(th.tx3).child("This game isn't in your library.").into_any_element();
        };
        let pane_w = (self.viewport.0 - SIDE_WIDTH - 1.).max(400.);
        let pane_h = (self.viewport.1 - 52.).max(300.);
        let amb = self.art_for(&g, ArtSlot::Hero, Variant::Ambient, cx);
        let hero = self.art_for(&g, ArtSlot::Hero, Variant::Original, cx);
        let logo = if BAKED_LOGO.contains(&id) && !self.custom_art.contains_key(&id) {
            None
        } else {
            Some(self.art_for(&g, ArtSlot::Logo, Variant::Trimmed, cx))
        };

        // Ambient tint from a blurred copy of the hero (no backdrop blur in GPUI).
        let ambient = div()
            .absolute()
            .inset_0()
            .overflow_hidden()
            .when_some(match &amb {
                ArtState::Ready(a) => Some(a.clone()),
                _ => None,
            }, |d, a| {
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
            })
            .child(div().absolute().inset_0().bg(gpui::linear_gradient(
                180.,
                gpui::linear_color_stop(th.ink1.opacity(0.8), 0.),
                gpui::linear_color_stop(th.ink1, 0.6),
            )));

        let mut hero_el = div().relative().h(px(HERO)).flex_none().overflow_hidden();
        hero_el = match &hero {
            ArtState::Ready(a) => hero_el.child(cover(a, pane_w, HERO, (0.5, 0.3))),
            other => hero_el.child(super::art_placeholder(th, other, "", px(0.))),
        };
        hero_el = hero_el
            .child(div().absolute().inset_0().w(relative(0.55)).bg(gpui::linear_gradient(
                90.,
                gpui::linear_color_stop(rgba(0x080a0e, 0.5), 0.),
                gpui::linear_color_stop(rgba(0x080a0e, 0.), 1.),
            )))
            .child(div().absolute().left_0().right_0().bottom_0().h(relative(0.65)).bg(gpui::linear_gradient(
                180.,
                gpui::linear_color_stop(th.ink1.opacity(0.), 0.),
                gpui::linear_color_stop(th.ink1, 1.),
            )));
        match logo {
            Some(ArtState::Ready(l)) => {
                // Fit inside 300×120, anchored top-left.
                let s = (300. / l.width as f32).min(120. / l.height as f32).min(1.5);
                hero_el = hero_el.child(
                    img(l.path.clone())
                        .absolute()
                        .left(px(28.))
                        .top(px(28.))
                        .w(px(l.width as f32 * s))
                        .h(px(l.height as f32 * s)),
                );
            }
            Some(_) => {
                hero_el = hero_el.child(
                    super::display(30., FontWeight::EXTRA_BOLD, on_art::tx1())
                        .absolute()
                        .left(px(28.))
                        .top(px(32.))
                        .max_w(px(520.))
                        .child(g.entry.name.clone()),
                );
            }
            None => {}
        }
        let custom = self.custom_art.get(&id).is_some_and(|c| c.hero.is_some() || c.tile.is_some() || c.logo.is_some());
        hero_el = hero_el.child(
            div()
                .absolute()
                .right(px(24.))
                .top(px(20.))
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
                .child(
                    art_chip(Some(Icon::Image), if custom { "Art: yours" } else { "Art: Ubisoft" })
                        .h(px(32.))
                        .child(icon(Icon::ChevronDown, 12., on_art::tx2())),
                ),
        );

        let tab = self.nav.tab;
        let body = match tab {
            Tab::Overview => self.render_overview(th, &g, cx),
            Tab::LaunchOptions => self.render_launch(th, &g, cx),
            t => not_yet(th, t, &g),
        };

        div()
            .flex_1()
            .min_w_0()
            .relative()
            .flex()
            .flex_col()
            .min_h_0()
            .overflow_hidden()
            .child(ambient)
            .child(hero_el)
            .child(self.render_actionbar(th, &g, cx))
            .child(self.render_stats(th, &g))
            .child(self.render_tabs(th, cx))
            .child(
                div()
                    .id(("body", id as usize))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.body_scroll)
                    .px(px(28.))
                    .pt(px(20.))
                    .pb(px(28.))
                    .child(body),
            )
            .into_any_element()
    }

    fn render_actionbar(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let pad = self.mode == InputMode::Pad;
        let f = |i| self.focused(Target::Action(i));
        let (label, i) = primary_label(g);
        let (main, more) = play_button(th, true, label, i, pad, f(ACTION_PRIMARY), f(ACTION_OPTIONS));
        let play = div()
            .flex()
            .flex_none()
            .rounded(px(10.))
            .shadow(vec![soft_shadow(th.acc.opacity(0.28), 8., 30.)])
            .cursor_pointer()
            .child(main.id("act-play").on_click(cx.listener(|this, _, _, cx| this.click(Target::Action(ACTION_PRIMARY), cx))))
            .child(more.id("act-more").on_click(cx.listener(|this, _, _, cx| this.click(Target::Action(ACTION_OPTIONS), cx))));
        let proton = g
            .settings
            .as_ref()
            .and_then(|s| s.proton.clone())
            .or_else(|| self.status.as_ref().and_then(|s| s.proton_default.clone()));
        let rev = g.current_revision();
        let proton_chip = match (proton, rev) {
            (Some(p), Some(r)) => Some(format!("{p} · r{r}")),
            (Some(p), None) => Some(p),
            (None, Some(r)) => Some(format!("r{r}")),
            _ => None,
        };
        let status: Option<(PillKind, Icon, String)> = if g.running() {
            Some((PillKind::Ok, Icon::Gamepad, "Playing now".into()))
        } else if let Some(fr) = g.installing() {
            Some((PillKind::Accent, Icon::Download, format!("Installing {:.0}%", fr * 100.)))
        } else if g.queued() {
            Some((PillKind::Muted, Icon::Download, "Queued".into()))
        } else if g.installed() && g.entry.update_available {
            Some((PillKind::Accent, Icon::ArrowDownToLine, "Update available".into()))
        } else if g.installed() {
            Some((PillKind::Ok, Icon::CircleCheck, "Installed".into()))
        } else {
            None
        };
        let steam = button("act-steam", th, Kind::Secondary, Size::Medium, Some(Icon::Gamepad), "Add to Steam")
            .focus_if(f(ACTION_STEAM), th)
            .on_click(cx.listener(|this, _, _, cx| this.click(Target::Action(ACTION_STEAM), cx)));
        let files = icon_button("act-files", th, Icon::FolderOpen, true)
            .bg(th.ink2)
            .focus_if(f(ACTION_FILES), th)
            .on_click(cx.listener(|this, _, _, cx| this.click(Target::Action(ACTION_FILES), cx)));
        let launch = icon_button("act-launch", th, Icon::SlidersHorizontal, true)
            .bg(th.ink2)
            .focus_if(f(ACTION_LAUNCH), th)
            .on_click(cx.listener(|this, _, _, cx| this.click(Target::Action(ACTION_LAUNCH), cx)));
        let more_btn = if pad {
            button("act-menu", th, Kind::Secondary, Size::Medium, None, "")
                .px(px(8.))
                .child(glyph(th, "Y", false))
                .child(icon(Icon::Ellipsis, 16., th.tx2))
        } else {
            icon_button("act-menu", th, Icon::Ellipsis, true).bg(th.ink2)
        }
        .focus_if(f(ACTION_MORE), th)
        .on_click(cx.listener(|this, _, _, cx| this.click(Target::Action(ACTION_MORE), cx)));

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .px(px(28.))
            .mt(px(-58.))
            .child(play)
            .when_some(proton_chip, |d, p| d.child(chip(th, Some(Icon::Cpu), p, false).h(px(30.))))
            .when_some(status, |d, (k, i, s)| d.child(pill(th, k, Some(i), s).py(px(5.))))
            .child(div().flex_1())
            .child(steam)
            .child(files)
            .child(launch)
            .child(more_btn)
    }

    fn render_stats(&self, th: &Theme, g: &GameRow) -> impl IntoElement + use<> {
        let pt = g.playtime.clone().unwrap_or_default();
        let account = self.status.as_ref().and_then(|s| s.signed_in.clone()).unwrap_or_else(|| "—".into());
        let stats: Vec<(&str, String)> = vec![
            ("Last played", g.last_played().map(day).unwrap_or_else(|| "Never".into())),
            ("Play time", if pt.total_seconds > 0 { hours(pt.total_seconds) } else { "—".into() }),
            ("Sessions", pt.sessions.len().to_string()),
            (
                "Install",
                if g.installed() {
                    "Installed".into()
                } else if let Some(f) = g.installing() {
                    format!("{:.0}%", f * 100.)
                } else {
                    "Not installed".into()
                },
            ),
            ("Launch options", g.current_revision().map(|r| format!("r{r}")).unwrap_or_else(|| "Defaults".into())),
            ("Account", format!("{account} · default")),
        ];
        let n = stats.len();
        div().flex().flex_none().px(px(28.)).pt(px(14.)).pb(px(16.)).children(stats.into_iter().enumerate().map(
            |(i, (k, v))| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .when(i + 1 < n, |d| d.pr(px(26.)).mr(px(26.)).border_r_1().border_color(th.line1))
                    .child(caption(k, th.tx3))
                    .child(div().text_size(px(14.)).font_weight(SEMIBOLD).child(v))
            },
        ))
    }

    fn render_tabs(&mut self, th: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let pad = self.mode == InputMode::Pad;
        let current = self.nav.tab;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(2.))
            .px(px(28.))
            .border_b_1()
            .border_color(th.line1)
            .when(pad, |d| d.child(glyph(th, "LT", true).mr(px(6.))))
            .children(Tab::ALL.iter().map(|&t| {
                let on = t == current;
                let focused = self.focused(Target::Tab(t));
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
                            .px(px(12.))
                            .py(px(3.))
                            .rounded(px(6.))
                            .text_size(px(13.))
                            .font_weight(MEDIUM)
                            .text_color(if on { th.tx1 } else { th.tx2 })
                            .focus_if(focused, th)
                            .child(t.label()),
                    )
                    .when(on, |d| {
                        d.child(div().absolute().left(px(10.)).right(px(10.)).bottom(px(-1.)).h(px(2.)).rounded(px(2.)).bg(th.acc))
                    })
            }))
            .when(pad, |d| d.child(glyph(th, "RT", true).ml(px(6.))))
    }

    // -----------------------------------------------------------------------------------------
    // Tabs

    fn render_overview(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let _ = cx;
        let pt = g.playtime.clone().unwrap_or_default();
        let rows: Vec<AnyElement> = pt
            .sessions
            .iter()
            .take(8)
            .enumerate()
            .map(|(i, s)| {
                let focused = self.focused(Target::Body(i));
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(16.))
                    .py(px(10.))
                    .when(i > 0, |d| d.border_t_1().border_color(th.line1))
                    .when(focused, |d| d.bg(th.acc_soft))
                    .child(
                        div()
                            .w(px(140.))
                            .flex()
                            .flex_col()
                            .child(div().font_weight(MEDIUM).child(day(s.started)))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(th.tx3)
                                    .child(format!("{} – {}", clock(s.started), clock(s.ended))),
                            ),
                    )
                    .child(div().w(px(90.)).font_weight(SEMIBOLD).child(duration(s.seconds)))
                    .child(chip(th, Some(Icon::History), format!("r{}", s.settings_revision.max(1)), false).h(px(22.)))
                    .child(div().flex_1())
                    .child(icon(Icon::CircleCheck, 14., th.tx3))
                    .into_any_element()
            })
            .collect();
        let sessions = card(th)
            .child(
                card_header(th, "Sessions")
                    .child(div().text_color(th.tx3).child(if pt.sessions.is_empty() {
                        "No sessions yet".to_string()
                    } else {
                        format!("{} played in total", hours(pt.total_seconds))
                    }))
                    .child(div().flex_1()),
            )
            .when(rows.is_empty(), |d| {
                d.child(div().p(px(16.)).text_color(th.tx3).child("Play this game and its sessions show up here, with the launch options each one used."))
            })
            .children(rows);
        let progression = card(th).child(
            card_header(th, "Progression")
                .child(pill(th, PillKind::Future, Some(Icon::PlugZap), "UbiServices, not connected yet"))
                .child(div().flex_1()),
        ).child(
            div()
                .p(px(16.))
                .text_size(px(12.5))
                .text_color(th.tx3)
                .child("Achievements and player stats will come from UbiServices; the product config already names them."),
        );

        let kv = |k: &str, v: AnyElement| {
            div()
                .flex()
                .gap(px(12.))
                .text_size(px(12.5))
                .child(div().w(px(112.)).flex_none().text_color(th.tx3).child(k.to_owned()))
                .child(div().flex_1().min_w_0().truncate().child(v))
        };
        let txt = |s: String| div().child(s).into_any_element();
        let mono_ = |s: String| mono(12., th.tx1).truncate().child(s).into_any_element();
        let mut install = card(th).child(card_header(th, "This install").child(div().flex_1()).child(
            button("verify", th, Kind::Ghost, Size::Small, Some(Icon::ShieldCheck), "Verify").opacity(0.6),
        ));
        let mut kvs = div().flex().flex_col().gap(px(7.)).p(px(16.));
        match &g.entry.installed {
            Some(game) => {
                kvs = kvs
                    .child(kv("Executable", mono_(game.exe.clone())))
                    .child(kv("Arguments", mono_(game.args.clone().unwrap_or_else(|| "—".into()))))
                    .child(kv("Language", txt(game.language.clone().unwrap_or_else(|| "Default".into()))))
                    .child(kv(
                        "Proton",
                        txt(g
                            .settings
                            .as_ref()
                            .and_then(|s| s.proton.clone())
                            .unwrap_or_else(|| {
                                format!(
                                    "{} (default)",
                                    self.status.as_ref().and_then(|s| s.proton_default.clone()).unwrap_or_default()
                                )
                            })),
                    ))
                    .child(kv(
                        "Wrappers",
                        txt(g
                            .settings
                            .as_ref()
                            .map(|s| s.wrappers.join(" › "))
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| "None".into())),
                    ))
                    .child(kv("Update", txt(if g.entry.update_available { "Available".into() } else { "Up to date".into() })));
            }
            None => {
                kvs = kvs.child(
                    div().text_color(th.tx3).child("Not installed. Install puts it in its own prefix; nothing is shared with other games."),
                );
            }
        }
        install = install.child(kvs);
        let locations = g.entry.installed.as_ref().map(|game| {
            let row = |k: &str, v: String| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .text_size(px(12.))
                    .child(div().w(px(46.)).text_color(th.tx3).child(k.to_owned()))
                    .child(mono(12., th.tx2).flex_1().min_w_0().truncate().child(v))
                    .child(icon(Icon::FolderOpen, 14., th.tx3))
            };
            card(th)
                .child(card_header(th, "Locations"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .p(px(16.))
                        .child(row("Game", tilde(&game.install_dir)))
                        .child(row("Prefix", tilde(&game.prefix))),
                )
        });
        let cmd = if g.installed() { format!("quiver play {}", g.id()) } else { format!("quiver install {}", g.id()) };
        div()
            .flex()
            .gap(px(20.))
            .child(div().flex().flex_col().flex_1().min_w_0().gap(px(20.)).child(sessions).child(progression))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(360.))
                    .flex_none()
                    .gap(px(20.))
                    .child(install)
                    .when_some(locations, |d, l| d.child(l))
                    .child(command_line(th, cmd)),
            )
            .into_any_element()
    }

    fn render_launch(&mut self, th: &Theme, g: &GameRow, cx: &mut Context<Self>) -> AnyElement {
        let _ = cx;
        let s = g.settings.clone().unwrap_or_default();
        let row = |k: &str, v: AnyElement| {
            div()
                .flex()
                .items_center()
                .gap(px(16.))
                .py(px(9.))
                .border_t_1()
                .border_color(th.line1)
                .child(div().w(px(170.)).flex_none().font_weight(MEDIUM).text_size(px(12.5)).child(k.to_owned()))
                .child(div().flex_1().min_w_0().child(v))
        };
        let field = |v: String, muted: bool| {
            div()
                .flex()
                .items_center()
                .h(px(30.))
                .px(px(10.))
                .rounded(px(8.))
                .bg(th.ink1)
                .border_1()
                .border_color(th.line2)
                .font_family(crate::assets::MONO_FONT)
                .text_size(px(12.))
                .text_color(if muted { th.tx3 } else { th.tx1 })
                .truncate()
                .child(v)
                .into_any_element()
        };
        let default_proton = self.status.as_ref().and_then(|s| s.proton_default.clone()).unwrap_or_default();
        let wrappers = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .children(s.wrappers.iter().chain(std::iter::once(&"proton".to_string())).map(|w| {
                pill(th, PillKind::Muted, None, w.clone()).font_family(crate::assets::MONO_FONT).font_weight(FontWeight::NORMAL)
            }))
            .child(
                pill(th, PillKind::Accent, None, g.entry.installed.as_ref().map(|i| i.exe.clone()).unwrap_or_default())
                    .font_family(crate::assets::MONO_FONT),
            );
        let env = if s.env.is_empty() {
            field("No extra variables".into(), true)
        } else {
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .children(s.env.iter().map(|(k, v)| field(format!("{k}={v}"), false)))
                .into_any_element()
        };
        let current = card(th)
            .px(px(16.))
            .pb(px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .py(px(12.))
                    .child(div().font_weight(SEMIBOLD).text_size(px(13.5)).child("Launch options"))
                    .child(div().text_color(th.tx3).text_size(px(12.)).child("This game · edits come with the next screens"))
                    .child(div().flex_1())
                    .child(pill(th, PillKind::Muted, Some(Icon::Lock), "Read only")),
            )
            .child(row(
                "Compatibility tool",
                field(s.proton.clone().unwrap_or_else(|| format!("{default_proton} (global default)")), s.proton.is_none()),
            ))
            .child(row("Wrappers", wrappers.into_any_element()))
            .child(row("Environment", env))
            .child(row("Extra arguments", field(if s.args.is_empty() { "None".into() } else { s.args.join(" ") }, s.args.is_empty())))
            .child(row(
                "DLL overrides",
                field(
                    if s.dll_overrides.is_empty() {
                        "None".into()
                    } else {
                        s.dll_overrides.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(";")
                    },
                    s.dll_overrides.is_empty(),
                ),
            ));

        let history = g.history.clone().unwrap_or_default();
        let latest = history.iter().map(|r| r.revision).max();
        let items: Vec<AnyElement> = history
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let is_cur = Some(r.revision) == latest;
                let focused = self.focused(Target::Body(i));
                let who = actor(&r.source);
                let who_color = match who {
                    crate::model::Actor::App => th.who_gui,
                    crate::model::Actor::Cli => th.who_cli,
                    crate::model::Actor::Agent => th.who_agent,
                    crate::model::Actor::System => th.who_sys,
                };
                let who_icon = match who {
                    crate::model::Actor::App => Icon::MousePointer,
                    crate::model::Actor::Cli => Icon::SquareTerminal,
                    crate::model::Actor::Agent => Icon::Bot,
                    crate::model::Actor::System => Icon::Cog,
                };
                let last = i + 1 == history.len();
                div()
                    .flex()
                    .gap(px(10.))
                    .child(
                        div()
                            .relative()
                            .w(px(22.))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .items_center()
                            .child(
                                div()
                                    .size(px(21.))
                                    .rounded_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(if is_cur { th.acc } else { th.ink3 })
                                    .border_1()
                                    .border_color(if is_cur { th.acc } else { th.line2 })
                                    .child(icon(
                                        if is_cur { Icon::CircleCheck } else { Icon::History },
                                        11.,
                                        if is_cur { th.acc_ink } else { th.tx2 },
                                    )),
                            )
                            .when(!last, |d| d.child(div().w(px(1.)).flex_1().bg(th.line2))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .pb(px(14.))
                            .px(px(8.))
                            .rounded(px(8.))
                            .when(focused, |d| d.bg(th.ink3).focus_if(true, th))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .child(mono(12., if is_cur { th.acc_hi } else { th.tx1 }).font_weight(SEMIBOLD).child(format!("r{}", r.revision)))
                                    .when(is_cur, |d| d.child(pill(th, PillKind::Accent, None, "Current")))
                                    .child(div().flex_1())
                                    .child(div().text_size(px(11.5)).text_color(th.tx3).child(ago(r.at))),
                            )
                            .child(div().text_size(px(12.5)).child(r.message.clone().unwrap_or_else(|| "No message".into())))
                            .child(
                                div().flex().child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(5.))
                                        .px(px(6.))
                                        .py(px(2.))
                                        .rounded(px(6.))
                                        .bg(who_color.opacity(0.13))
                                        .text_size(px(11.5))
                                        .font_weight(SEMIBOLD)
                                        .text_color(who_color)
                                        .child(icon(who_icon, 12., who_color))
                                        .child(actor_label(&r.source)),
                                ),
                            ),
                    )
                    .into_any_element()
            })
            .collect();
        let hist = card(th)
            .w(px(380.))
            .flex_none()
            .child(
                card_header(th, "History")
                    .child(badge(th, format!("{} revisions", history.len()), false))
                    .child(div().flex_1()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .p(px(16.))
                    .when(items.is_empty(), |d| d.child(div().text_color(th.tx3).child("No revisions yet.")))
                    .children(items),
            );
        div()
            .flex()
            .gap(px(20.))
            .items_start()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(16.))
                    .child(current)
                    .child(command_line(th, format!("quiver set {} proton=…  -m \"why\"", g.id()))),
            )
            .child(hist)
            .into_any_element()
    }
}

/// Tabs whose data isn't wired to the daemon yet: say so, and where it will come from.
fn not_yet(th: &Theme, tab: Tab, g: &GameRow) -> AnyElement {
    let (i, what, source, cli) = match tab {
        Tab::Mods => (Icon::Layers, "Mod profiles and overlay layers", "the quiverd mod store", format!("quiver mods list {}", g.id())),
        Tab::Saves => (Icon::Cloud, "Cloud save slots, backups and play time", "cloudsave_service and the Ubisoft play-session API", format!("quiver saves list {}", g.id())),
        Tab::AddOns => (Icon::Package, "Owned and available add-ons", "ownership associations and each add-on's product config", format!("quiver show {} --addons", g.id())),
        Tab::Files => (Icon::FolderOpen, "Install files, prefix and verification", "the install record and manifest", format!("quiver verify {}", g.id())),
        Tab::Logs => (Icon::ScrollText, "Launch and Proton logs", "quiverd's per-session logs", format!("quiver logs {}", g.id())),
        _ => (Icon::Info, "", "", String::new()),
    };
    card(th)
        .items_center()
        .py(px(48.))
        .px(px(28.))
        .gap(px(12.))
        .child(
            div()
                .size(px(44.))
                .rounded(px(12.))
                .flex()
                .items_center()
                .justify_center()
                .bg(th.ink4)
                .child(icon(i, 20., th.tx2)),
        )
        .child(div().text_size(px(15.)).font_weight(SEMIBOLD).child(what))
        .child(
            div()
                .max_w(px(460.))
                .text_center()
                .text_color(th.tx3)
                .child(format!("{} isn't wired to the daemon yet. It will come from {source}.", tab.label())),
        )
        .child(pill(th, PillKind::Future, Some(Icon::PlugZap), "Not wired yet"))
        .child(div().w(px(420.)).child(command_line(th, cli)))
        .into_any_element()
}
