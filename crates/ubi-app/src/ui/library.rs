//! Library home: "Continue playing" hero with Needs attention, then the box-art grid.

use gpui::{AnyElement, Context, FontWeight, Hsla, IntoElement, ParentElement, Styled, div, prelude::*, px, relative};

use super::{
    ArtState, BOLD, FocusExt, Icon, Kind, MEDIUM, SEMIBOLD, Size, TagKind, art_chip, art_fill, badge, button, caption, chip,
    chip_count, cover, display, glyph, icon, progress, soft_shadow, tag,
};
use crate::app::{Attention, InputMode, Quiver};
use crate::art::{ArtSlot, Variant};
use crate::model::{GameRow, clock, day, duration, hours, short_name};
use crate::nav::Target;
use crate::theme::{Theme, on_art, rgba};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibFilter {
    All,
    Installed,
    Updates,
}

pub const HERO_HEIGHT: f32 = 396.;
/// Box art is Ubisoft's 240×280 thumb ratio.
pub const TILE_RATIO: f32 = 280. / 240.;

/// The split Play button. `main_focus`/`more_focus` draw the ring on either half.
pub fn play_button(
    th: &Theme,
    large: bool,
    label: &str,
    i: Icon,
    pad_glyph: bool,
    main_focus: bool,
    more_focus: bool,
) -> (gpui::Div, gpui::Div) {
    let (h, r, fs, pad_l, pad_r, more_w, is) =
        if large { (46., 10., 15.5, 20., 26., 38., 18.) } else { (36., 8., 13.5, 14., 18., 30., 15.) };
    let main = div()
        .flex()
        .items_center()
        .gap(px(10.))
        .h(px(h))
        .pl(px(pad_l))
        .pr(px(pad_r))
        .rounded_l(px(r))
        .bg(gpui::linear_gradient(180., gpui::linear_color_stop(th.acc_top, 0.), gpui::linear_color_stop(th.acc, 1.)))
        .text_color(th.acc_ink)
        .text_size(px(fs))
        .font_weight(BOLD)
        .when(pad_glyph, |d| {
            d.child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(20.))
                    .rounded_full()
                    .bg(th.acc_ink)
                    .text_color(th.acc)
                    .text_size(px(10.5))
                    .font_weight(FontWeight::EXTRA_BOLD)
                    .child("A"),
            )
        })
        .when(!pad_glyph, |d| d.child(icon(i, is, th.acc_ink)))
        .child(label.to_owned())
        .focus_if(main_focus, th);
    let more = div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(more_w))
        .h(px(h))
        .rounded_r(px(r))
        .bg(th.acc_deep)
        .border_l_1()
        .border_color(th.acc_ink.opacity(0.25))
        .child(icon(Icon::ChevronDown, if large { 16. } else { 14. }, th.acc_ink))
        .focus_if(more_focus, th);
    (main, more)
}

impl Quiver {
    pub fn grid_games(&self) -> Vec<&GameRow> {
        self.ordered()
            .into_iter()
            .filter(|g| match self.lib_filter {
                LibFilter::All => true,
                LibFilter::Installed => g.installed(),
                LibFilter::Updates => g.installed() && g.entry.update_available,
            })
            .collect()
    }

    pub fn render_library(&mut self, th: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (cols, tile_w) = self.grid_metrics();
        let ids: Vec<u32> = self.grid_games().iter().map(|g| g.id()).collect();
        let mut rows: Vec<AnyElement> = Vec::new();
        for (r, chunk) in ids.chunks(cols).enumerate() {
            let tiles: Vec<AnyElement> = chunk.iter().map(|id| self.render_tile(th, *id, tile_w, cx)).collect();
            rows.push(
                div()
                    .id(("grid-row", r))
                    .flex()
                    .gap(px(Self::GRID_GAP))
                    .px(px(Self::GUTTER))
                    .pt(px(if r == 0 { 0. } else { 22. }))
                    .children(tiles)
                    .into_any_element(),
            );
        }
        if ids.is_empty() {
            rows.push(
                div()
                    .px(px(Self::GUTTER))
                    .py(px(40.))
                    .text_color(th.tx3)
                    .child(if self.games.is_empty() { "Loading your library…" } else { "Nothing matches this filter." })
                    .into_any_element(),
            );
        }
        let hero = self.render_lib_hero(th, cx);
        let header = self.render_lib_header(th, cx);
        div()
            .id("library")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.lib_scroll)
            .child(hero)
            .child(header)
            .children(rows)
            .child(div().h(px(48.)))
    }

    fn render_lib_header(&mut self, th: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let total = self.games.len();
        let installed = self.games.iter().filter(|g| g.installed()).count();
        let updates = self.games.iter().filter(|g| g.installed() && g.entry.update_available).count();
        let filter_chip = |f: LibFilter, label: &'static str, n: usize, i: Option<Icon>, cx: &mut Context<Self>| {
            let on = self.lib_filter == f;
            chip(th, i, label, on)
                .child(chip_count(th, n, on))
                .id(label)
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.lib_filter = f;
                    cx.notify();
                }))
        };
        let chips = div()
            .flex()
            .flex_wrap()
            .gap(px(6.))
            .child(filter_chip(LibFilter::All, "All", total, None, cx))
            .child(filter_chip(LibFilter::Installed, "Installed", installed, None, cx))
            .child(filter_chip(LibFilter::Updates, "Updates", updates, Some(Icon::ArrowDownToLine), cx))
            .child(div().w(px(1.)).h(px(18.)).mx(px(6.)).bg(th.line2))
            .child(
                chip(th, Some(Icon::Users), "All accounts", false)
                    .child(icon(Icon::ChevronDown, 12., th.tx2))
                    .opacity(0.55),
            )
            .child(chip(th, Some(Icon::Star), "Favorites", false).opacity(0.55))
            .child(chip(th, Some(Icon::Layers), "Modded", false).opacity(0.55));
        let field = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .w(px(220.))
            .h(px(32.))
            .px(px(10.))
            .rounded(px(8.))
            .bg(th.ink1)
            .border_1()
            .border_color(th.line2)
            .text_color(th.tx3)
            .child(icon(Icon::ListFilter, 16., th.tx3))
            .child(div().flex_1().child("Filter"))
            .child(super::kbd(th, "/"));
        let seg = div()
            .flex()
            .gap(px(2.))
            .p(px(2.))
            .rounded(px(8.))
            .bg(th.ink2)
            .border_1()
            .border_color(th.line2)
            .child(div().px(px(10.)).py(px(4.)).rounded(px(6.)).bg(th.ink5).child(icon(Icon::LayoutGrid, 14., th.tx1)))
            .child(div().px(px(10.)).py(px(4.)).rounded(px(6.)).child(icon(Icon::List, 14., th.tx2)));
        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .px(px(Self::GUTTER))
            .pt(px(6.))
            .pb(px(18.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .child(display(20., FontWeight::BOLD, th.tx1).child("Library"))
                    .child(div().text_color(th.tx3).child(format!("{total} games · {installed} installed")))
                    .child(div().flex_1())
                    .child(field)
                    .child(button("sort", th, Kind::Secondary, Size::Medium, Some(Icon::ArrowDownUp), "Recently played"))
                    .child(seg),
            )
            .child(chips)
    }

    fn render_lib_hero(&mut self, th: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let w = self.viewport.0;
        let Some(g) = self.hero_game().cloned() else {
            return div().h(px(28.)).into_any_element();
        };
        let id = g.id();
        let art = self.art_for(&g, ArtSlot::Hero, Variant::Original, cx);
        let mut hero = div().id("hero").relative().h(px(HERO_HEIGHT)).w_full().overflow_hidden().bg(th.ink0);
        hero = match &art {
            ArtState::Ready(img) => hero.child(cover(img, w, HERO_HEIGHT, (0.5, 0.35))),
            other => hero.child(super::art_placeholder(th, other, "", px(0.))),
        };
        let scrim = rgba(0x0a0c10, 0.92);
        hero = hero
            .child(
                div().absolute().left_0().top_0().bottom_0().w(relative(0.58)).bg(gpui::linear_gradient(
                    90.,
                    gpui::linear_color_stop(scrim, 0.),
                    gpui::linear_color_stop(scrim.opacity(0.), 1.),
                )),
            )
            .child(
                div().absolute().left_0().top_0().bottom_0().w(relative(0.45)).bg(gpui::linear_gradient(
                    90.,
                    gpui::linear_color_stop(rgba(0x0a0c10, 0.45), 0.),
                    gpui::linear_color_stop(rgba(0x0a0c10, 0.), 1.),
                )),
            )
            .child(div().absolute().left_0().right_0().bottom_0().h(relative(0.45)).bg(gpui::linear_gradient(
                180.,
                gpui::linear_color_stop(th.ink1.opacity(0.), 0.),
                gpui::linear_color_stop(th.ink1, 1.),
            )));

        // Copy: last session, play time, the Proton build it will launch with.
        let pt = g.playtime.clone().unwrap_or_default();
        let mut sub = String::new();
        if let Some(s) = pt.sessions.first() {
            sub = format!("Last session {}, {} to {}", day(s.started).to_lowercase(), clock(s.started), clock(s.ended));
            if sub.contains("last session yesterday") || sub.contains("last session today") {
                // keep lowercase relative words
            }
        }
        if pt.total_seconds > 0 {
            if !sub.is_empty() {
                sub.push_str(" · ");
            }
            sub.push_str(&format!("{} played", hours(pt.total_seconds)));
        }
        let proton = g
            .settings
            .as_ref()
            .and_then(|s| s.proton.clone())
            .or_else(|| self.status.as_ref().and_then(|s| s.proton_default.clone()).map(|p| format!("{p} (default)")));
        let accent_on_art: Hsla = if th.dark { th.acc_hi } else { th.focus };

        let focus_play = self.focused(Target::Hero(0));
        let focus_details = self.focused(Target::Hero(1));
        let pad = self.mode == InputMode::Pad;
        let (label, i) = primary_label(&g);
        let (main, more) = play_button(th, true, label, i, pad && focus_play, focus_play, false);
        let play = div()
            .id("hero-play")
            .flex()
            .flex_none()
            .rounded(px(10.))
            .shadow(vec![soft_shadow(th.acc.opacity(0.28), 8., 30.)])
            .cursor_pointer()
            .child(main.id("hero-play-main").on_click(cx.listener(|this, _, _, cx| this.click(Target::Hero(0), cx))))
            .child(more.id("hero-play-more").on_click(cx.listener(move |this, _, _, cx| {
                this.nav.open_menu(id);
                cx.notify();
            })));
        let details = button("hero-details", th, Kind::OnArt, Size::Large, None, "Game details")
            .focus_if(focus_details, th)
            .on_click(cx.listener(|this, _, _, cx| this.click(Target::Hero(1), cx)));

        let content = div()
            .absolute()
            .left(px(Self::GUTTER))
            .bottom(px(44.))
            .w(px(540.))
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(caption("Continue playing", accent_on_art))
            .child(display(40., FontWeight::EXTRA_BOLD, on_art::tx1()).line_height(px(41.)).child(g.entry.name.clone()))
            .when(!sub.is_empty(), |d| d.child(div().text_size(px(13.5)).text_color(on_art::tx2()).child(sub)))
            .child(
                div()
                    .flex()
                    .gap(px(6.))
                    .when_some(proton, |d, p| d.child(art_chip(Some(Icon::Cpu), p)))
                    .when_some(g.current_revision(), |d, r| d.child(art_chip(Some(Icon::History), format!("Launch options r{r}"))))
                    .when(g.running(), |d| d.child(art_chip(Some(Icon::Gamepad), "Playing now"))),
            )
            .child(div().flex().gap(px(10.)).mt(px(4.)).child(play).child(details));
        hero = hero.child(content);

        let attention = self.attention();
        if !attention.is_empty() {
            hero = hero.child(self.render_attention(th, &attention, cx));
        }
        hero.into_any_element()
    }

    fn render_attention(&mut self, th: &Theme, items: &[(u32, Attention)], cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut panel = div()
            .absolute()
            .right(px(32.))
            .top(px(28.))
            .w(px(372.))
            .flex()
            .flex_col()
            .pt(px(14.))
            .px(px(16.))
            .pb(px(6.))
            .rounded(px(14.))
            .bg(on_art::glass())
            .border_1()
            .border_color(rgba(0xffffff, 0.09))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .mb(px(6.))
                    .child(caption("Needs attention", on_art::tx2()))
                    .child(
                        div()
                            .px(px(6.))
                            .py(px(1.))
                            .rounded_full()
                            .bg(on_art::fill())
                            .text_size(px(10.5))
                            .font_weight(SEMIBOLD)
                            .text_color(on_art::tx2())
                            .child(items.len().to_string()),
                    ),
            );
        for (k, (id, a)) in items.iter().enumerate() {
            let Some(g) = self.game(*id).cloned() else { continue };
            let thumb = self.art_for(&g, ArtSlot::Tile, Variant::Original, cx);
            let (what, sub): (String, AnyElement) = match a {
                Attention::Update => (
                    "Update ready".into(),
                    div().text_size(px(11.5)).text_color(on_art::tx3()).child("Installs in place, keeps your prefix").into_any_element(),
                ),
                Attention::Installing(f) => (
                    format!("Installing {:.0}%", f * 100.),
                    div().mt(px(6.)).w(px(150.)).child(progress(th, *f, Some(150.), 4., None)).into_any_element(),
                ),
                Attention::Failed(e) => (
                    "Failed".into(),
                    div().text_size(px(11.5)).text_color(on_art::tx3()).truncate().child(e.clone()).into_any_element(),
                ),
            };
            let target = Target::Hero(2 + k);
            let focused = self.focused(target);
            let action = match a {
                Attention::Update => "Update now",
                Attention::Installing(_) => "Details",
                Attention::Failed(_) => "Open",
            };
            panel = panel.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .py(px(10.))
                    .border_t_1()
                    .border_color(rgba(0xffffff, 0.07))
                    .child(div().w(px(34.)).h(px(40.)).flex_none().child(art_fill(th, &thumb, "", 5., false)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                div()
                                    .flex()
                                    .gap(px(4.))
                                    .text_size(px(12.5))
                                    .text_color(on_art::tx1())
                                    .child(div().font_weight(SEMIBOLD).child(short_name(&g.entry.name)))
                                    .child(div().text_color(on_art::tx3()).child(format!("· {what}"))),
                            )
                            .child(sub),
                    )
                    .child(
                        button(("attn", k), th, Kind::OnArt, Size::Small, None, action)
                            .focus_if(focused, th)
                            .on_click(cx.listener(move |this, _, _, cx| this.click(target, cx))),
                    ),
            );
        }
        panel
    }

    fn render_tile(&mut self, th: &Theme, id: u32, w: f32, cx: &mut Context<Self>) -> AnyElement {
        let Some(g) = self.game(id).cloned() else { return div().into_any_element() };
        let h = (w * TILE_RATIO).round();
        let dim = !g.installed() && g.install_job().is_none();
        let art = self.art_for(&g, ArtSlot::Tile, if dim { Variant::Dimmed } else { Variant::Original }, cx);
        let focused = self.focused(Target::Tile(id));
        let label = short_name(&g.entry.name);

        let mut tl = div().absolute().left(px(8.)).top(px(8.)).flex().gap(px(4.));
        if g.running() {
            tl = tl.child(tag(th, TagKind::Accent, Some(Icon::Gamepad), "Playing"));
        } else if g.failed().is_some() {
            tl = tl.child(tag(th, TagKind::Warn, Some(Icon::TriangleAlert), "Failed"));
        } else if g.queued() {
            tl = tl.child(tag(th, TagKind::Plain, Some(Icon::Download), "Queued"));
        } else if g.installed() && g.entry.update_available {
            tl = tl.child(tag(th, TagKind::Accent, Some(Icon::ArrowDownToLine), "Update"));
        }
        let custom = self.custom_art.get(&id).is_some_and(|c| c.tile.is_some());

        let ring = if focused {
            super::focus_ring(th)
        } else {
            vec![soft_shadow(rgba(0x000000, 0.35), 6., 18.)]
        };
        let hover_ring = vec![
            gpui::BoxShadow {
                color: th.acc_line,
                offset: gpui::point(px(0.), px(0.)),
                blur_radius: px(0.),
                spread_radius: px(2.),
            },
            soft_shadow(rgba(0x000000, 0.6), 18., 40.),
        ];
        let group = gpui::SharedString::from(format!("tile-{id}"));
        let mut boxart = div()
            .relative()
            .w(px(w))
            .h(px(h))
            .rounded(px(10.))
            .bg(th.ink3)
            .shadow(ring)
            .when(!focused, |d| d.group_hover(group.clone(), move |s| s.shadow(hover_ring.clone())))
            .child(art_fill(th, &art, &label, 10., false))
            .child(tl);
        if custom {
            boxart = boxart.child(
                div().absolute().right(px(8.)).top(px(8.)).child(tag(th, TagKind::Plain, Some(Icon::Image), "")),
            );
        }
        if let Some(f) = g.installing() {
            boxart = boxart.child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .pt(px(26.))
                    .px(px(8.))
                    .pb(px(8.))
                    .rounded_b(px(10.))
                    .bg(gpui::linear_gradient(
                        180.,
                        gpui::linear_color_stop(rgba(0x05060a, 0.), 0.),
                        gpui::linear_color_stop(rgba(0x05060a, 0.9), 1.),
                    ))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .mb(px(5.))
                            .text_size(px(11.))
                            .font_weight(SEMIBOLD)
                            .text_color(on_art::tx1())
                            .child("Installing")
                            .child(format!("{:.0}%", f * 100.)),
                    )
                    .child(progress(th, f, None, 4., None)),
            );
        } else if g.installed() && !focused {
            // Hover: a play disc over the art.
            boxart = boxart.child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(px(10.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgba(0x05070c, 0.35))
                    .invisible()
                    .group_hover(group.clone(), |s| s.visible())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(52.))
                            .rounded_full()
                            .bg(th.acc)
                            .shadow(vec![soft_shadow(rgba(0x000000, 0.5), 8., 24.)])
                            .child(div().ml(px(3.)).child(icon(Icon::Play, 22., th.acc_ink))),
                    ),
            );
        }
        if focused && self.mode == InputMode::Pad {
            boxart = boxart.child(
                div()
                    .absolute()
                    .left(px(8.))
                    .bottom(px(8.))
                    .flex()
                    .gap(px(4.))
                    .child(glyph(th, "A", false))
                    .child(div().text_size(px(11.)).font_weight(SEMIBOLD).text_color(on_art::tx1()).child("Open")),
            );
        }
        let meta = g.tile_meta();
        div()
            .id(("tile", id as usize))
            .group(group)
            .flex()
            .flex_col()
            .gap(px(8.))
            .w(px(w))
            .flex_none()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.click(Target::Tile(id), cx)))
            .child(boxart)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().text_size(px(12.5)).font_weight(MEDIUM).truncate().child(g.entry.name.clone()))
                    .child(div().text_size(px(11.5)).text_color(th.tx3).truncate().child(meta)),
            )
            .into_any_element()
    }
}

/// The primary action's label and icon for a game.
pub fn primary_label(g: &GameRow) -> (&'static str, Icon) {
    if g.running() {
        ("Stop", Icon::Square)
    } else if g.installing().is_some() {
        ("Installing", Icon::Download)
    } else if g.queued() {
        ("Queued", Icon::Download)
    } else if !g.installed() {
        ("Install", Icon::Download)
    } else if g.entry.update_available {
        ("Update", Icon::ArrowDownToLine)
    } else {
        ("Play", Icon::Play)
    }
}

/// The last-session line used by the hero and the game page.
pub fn session_line(g: &GameRow) -> Option<String> {
    let s = g.playtime.as_ref()?.sessions.first()?;
    Some(format!("{} · {}", day(s.started), duration(s.seconds)))
}
