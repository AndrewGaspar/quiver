//! Library home: "Continue playing" hero with Needs attention, then the box-art grid. The
//! library is every store's games; Ubisoft is one store among them, and while no Ubisoft
//! account is signed in the library offers its sign-in as a card.

use quiver_ui::depth::DepthExt;
use gpui::{AnyElement, Context, FontWeight, Hsla, IntoElement, ParentElement, Styled, div, prelude::*, px, relative};

use super::{
    ArtState, BOLD, FocusExt, Icon, Kind, MEDIUM, SEMIBOLD, Size, TagKind, art_chip, art_fill, button, caption, chip,
    chip_count, command_line, cover, display, glyph, icon, progress, soft_shadow, tag,
};
use super::hero::{TitleBox, title_slot};
use crate::app::{Attention, InputMode, Quiver};
use crate::art::{ArtSlot, Variant};
use crate::model::{Gid, GameRow, clock, day, hours, short_name};
use quiver_api::{StoreId, Status};
use crate::nav::Target;
use crate::theme::{Theme, on_art, rgba};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibFilter {
    All,
    Installed,
    Updates,
}

/// The library's Ubisoft sign-in card, while no Ubisoft account is signed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UbisoftCard {
    /// No account yet.
    SignIn,
    /// Accounts are known but none has a session (it ended).
    SignInAgain,
    /// A sign-in started elsewhere (`quiver login`) waits for its two-step code.
    EnterCode,
}

impl UbisoftCard {
    /// The card for the daemon's status; `None` once an account is signed in (or before
    /// the daemon answered).
    pub fn of(status: Option<&Status>) -> Option<UbisoftCard> {
        let s = status?;
        if s.signed_in.is_some() {
            None
        } else if s.awaiting_2fa {
            Some(UbisoftCard::EnterCode)
        } else if !s.accounts.is_empty() {
            Some(UbisoftCard::SignInAgain)
        } else {
            Some(UbisoftCard::SignIn)
        }
    }

    fn copy(self) -> (&'static str, &'static str, &'static str) {
        match self {
            UbisoftCard::SignIn => (
                "Add your Ubisoft games",
                "Sign in with your Ubisoft account and every game you own there joins this library, next to your other stores' games.",
                "Sign in to Ubisoft",
            ),
            UbisoftCard::SignInAgain => (
                "Your Ubisoft session ended",
                "Ubisoft's games are hidden until you sign in again. Your other stores' games are unaffected.",
                "Sign in again",
            ),
            UbisoftCard::EnterCode => (
                "A Ubisoft sign-in is waiting for its code",
                "Enter the 6-digit code from your authenticator app to finish signing in.",
                "Enter code",
            ),
        }
    }
}

/// The store badges on a tile (steer X2, as decided 2026-11-17): every store the game has
/// a record in except Ubisoft while Ubisoft and one other store are all the stores there
/// are; every store, Ubisoft too, once a third store is present. `present` is the stores in
/// the library; Ubisoft always counts, signed in or not.
pub fn tile_badges(game: &[StoreId], present: &[StoreId]) -> Vec<StoreId> {
    let n = present.len() + usize::from(!present.contains(&StoreId::Ubisoft));
    let all = n > 2;
    game.iter().copied().filter(|s| all || *s != StoreId::Ubisoft).collect()
}

pub const HERO_HEIGHT: f32 = 396.;
/// The hero's Needs attention panel, beside the game from `Medium` up.
const ATTENTION_W: f32 = 372.;
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
        .focus_glow(main_focus, th, 10.)
        // Stereo: the two halves move together with their wrapper (the control).
        .stereo_control(false);
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
        .focus_glow(more_focus, th, 10.)
        .stereo_control(false);
    (main, more)
}

impl Quiver {
    pub fn grid_games(&self) -> Vec<&GameRow> {
        self.ordered()
            .into_iter()
            .filter(|g| match self.lib_filter {
                LibFilter::All => true,
                LibFilter::Installed => g.on_disk(),
                LibFilter::Updates => g.has_update(),
            })
            .filter(|g| self.store_filter.is_none_or(|s| g.entry.stores().contains(&s)))
            .collect()
    }

    /// Stores with games in the library, with counts, Ubisoft first (the store chips; shown
    /// once there is more than one store).
    pub fn library_stores(&self) -> Vec<(StoreId, usize)> {
        StoreId::ALL
            .into_iter()
            .map(|s| (s, self.games.iter().filter(|g| g.entry.stores().contains(&s)).count()))
            .filter(|(_, n)| *n > 0)
            .collect()
    }

    pub fn render_library(&mut self, th: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (cols, tile_w) = self.grid_metrics();
        let ids: Vec<Gid> = self.grid_games().iter().map(|g| g.id()).collect();
        let mut rows: Vec<AnyElement> = Vec::new();
        for (r, chunk) in ids.chunks(cols).enumerate() {
            let tiles: Vec<AnyElement> = chunk.iter().map(|id| self.render_tile(th, *id, tile_w, cx)).collect();
            rows.push(
                div()
                    .id(("grid-row", r))
                    .depth_surface()
                    .flex()
                    .gap(px(Self::GRID_GAP))
                    .px(px(self.gutter()))
                    .pt(px(if r == 0 { 0. } else { 22. }))
                    .children(tiles)
                    .into_any_element(),
            );
        }
        if ids.is_empty() {
            let empty = if !self.games.is_empty() {
                "Nothing matches this filter."
            } else if !self.library_loaded {
                "Loading your library…"
            } else if !self.ubisoft_signed_in() {
                "No games here yet. Sign in to Ubisoft above, or install a game in Steam, and it shows up here."
            } else {
                "No games here yet."
            };
            rows.push(div().px(px(self.gutter())).py(px(40.)).text_color(th.tx3).child(empty).into_any_element());
        }
        let hero = self.render_lib_hero(th, cx);
        let header = self.render_lib_header(th, cx);
        let signin = UbisoftCard::of(self.status.as_ref()).map(|c| self.render_ubisoft_card(th, c, cx));
        div()
            .id("library")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.lib_scroll)
            .child(hero)
            // Stereo: the hero's art is the plane; the library (its header, the sign-in card's
            // row and the grid's rows) floats a step above it, its chips and tiles another step
            // above that. Each child takes the depth itself: `scroll_to_item` counts them.
            .child(div().depth_surface().child(header))
            .children(signin.map(|s| div().depth_surface().child(s)))
            .children(rows)
            .child(div().h(px(48.)))
    }

    fn render_lib_header(&mut self, th: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let total = self.games.len();
        let installed = self.games.iter().filter(|g| g.on_disk()).count();
        let updates = self.games.iter().filter(|g| g.has_update()).count();
        let stores = self.library_stores();
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
            .when(stores.len() > 1, |d| {
                d.children(stores.iter().map(|&(s, n)| {
                    let on = self.store_filter == Some(s);
                    chip(th, None, s.label(), on)
                        .child(chip_count(th, n, on))
                        .id(("store-chip", s as usize))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store_filter = if this.store_filter == Some(s) { None } else { Some(s) };
                            cx.notify();
                        }))
                }))
                .child(div().w(px(1.)).h(px(18.)).mx(px(6.)).bg(th.line2))
            })
            .child(
                chip(th, Some(Icon::Users), "All accounts", false)
                    .child(icon(Icon::ChevronDown, 12., th.tx2))
                    .opacity(0.55),
            )
            .child(chip(th, Some(Icon::Star), "Favorites", false).opacity(0.55))
            .child(chip(th, Some(Icon::Layers), "Modded", false).opacity(0.55));
        let bp = self.bp();
        let field = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .when(!bp.stacked(), |d| d.w(px(220.)))
            .when(bp.stacked(), |d| d.flex_1().min_w(px(90.)))
            .h(px(32.))
            .px(px(10.))
            .rounded(px(8.))
            .bg(th.ink1)
            .border_1()
            .border_color(th.line2)
            .text_color(th.tx3)
            .child(icon(Icon::ListFilter, 16., th.tx3))
            .child(div().flex_1().min_w_0().truncate().child("Filter"))
            .child(super::kbd(th, "/"));
        let seg = div()
            .flex()
            .flex_none()
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
            .px(px(self.gutter()))
            .pt(px(6.))
            .pb(px(18.))
            .map(|d| {
                let title = div()
                    .flex()
                    .flex_wrap()
                    .items_baseline()
                    .gap_x(px(14.))
                    .gap_y(px(2.))
                    .child(display(20., FontWeight::BOLD, th.tx1).child("Library"))
                    .child(div().text_color(th.tx3).child(format!("{total} games · {installed} installed")));
                let sort = button("sort", th, Kind::Secondary, Size::Medium, Some(Icon::ArrowDownUp), if bp.narrow() { "" } else { "Recently played" })
                    .when(bp.narrow(), |b| b.tooltip(super::responsive::tip(th, "Recently played")));
                let controls = div().flex().items_center().gap(px(if bp.narrow() { 8. } else { 14. })).child(field).child(sort).child(seg);
                if bp.stacked() {
                    d.child(div().flex().flex_col().gap(px(12.)).child(title).child(controls))
                } else {
                    d.child(div().flex().items_center().gap(px(14.)).child(title).child(div().flex_1()).child(controls))
                }
            })
            .child(chips)
    }

    /// Ubisoft's sign-in, as a row above the grid: signing in is an option, not a gate.
    fn render_ubisoft_card(&mut self, th: &Theme, card: UbisoftCard, cx: &mut Context<Self>) -> AnyElement {
        let (title, body, action) = card.copy();
        let bp = self.bp();
        let warn = card == UbisoftCard::SignInAgain;
        let (mark_bg, mark_fg) = if warn { (th.warn_soft, th.warn) } else { (th.acc_soft, th.acc_hi) };
        let mark = if card == UbisoftCard::EnterCode { Icon::KeyRound } else if warn { Icon::ClockAlert } else { Icon::User };
        div()
            .px(px(self.gutter()))
            .pb(px(22.))
            .child(
                super::card(th)
                    .id("ubisoft-signin")
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(14.))
                    .px(px(16.))
                    .py(px(14.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .flex_none()
                            .size(px(38.))
                            .rounded(px(10.))
                            .bg(mark_bg)
                            .child(icon(mark, 18., mark_fg)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(220.))
                            .gap(px(2.))
                            .child(div().text_size(px(14.)).font_weight(SEMIBOLD).text_color(th.tx1).child(title))
                            .child(div().text_size(px(12.5)).text_color(th.tx3).child(body)),
                    )
                    .when(!bp.narrow(), |d| d.child(command_line(th, "quiver login").w(px(170.))))
                    .child(
                        button("ubisoft-signin-go", th, Kind::Primary, Size::Medium, None, action)
                            .on_click(cx.listener(|this, _, _, cx| this.start_add_account(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_lib_hero(&mut self, th: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let w = self.viewport.0;
        let Some(g) = self.hero_game().cloned() else {
            return div().h(px(28.)).into_any_element();
        };
        let id = g.id();
        let art = self.art_for(&g, ArtSlot::Hero, Variant::Original, cx);
        // The logo (or the name until it loads); nothing when the art has it painted in.
        let title = self.hero_title(&g, cx);
        let bp = self.bp();
        let gutter = bp.gutter();
        let attention = self.attention();
        // Below Medium the hero stacks: Needs attention on top, the game under it, and it
        // grows to fit them; the art is cropped to a generous height and clipped.
        let stacked = bp.stacked();
        let side_panel = !attention.is_empty() && !stacked;
        let content_w = (w - 2. * gutter - if side_panel { ATTENTION_W + 32. + 24. } else { 0. }).clamp(200., 540.);
        let art_h = if stacked { HERO_HEIGHT + 120. + 70. * attention.len() as f32 } else { HERO_HEIGHT };
        let mut hero = div().id("hero").relative().w_full().overflow_hidden().bg(th.ink0).depth_plane();
        hero = if stacked { hero.flex().flex_col().min_h(px(HERO_HEIGHT)) } else { hero.h(px(HERO_HEIGHT)) };
        let art_layer = div().absolute().inset_0().overflow_hidden();
        hero = match &art {
            ArtState::Ready(img) => hero.child(art_layer.child(cover(img, w, art_h, (0.5, 0.35)))),
            other => hero.child(art_layer.child(super::art_placeholder(th, other, "", px(0.)))),
        };
        let scrim = rgba(0x0a0c10, 0.92);
        hero = hero
            .when(stacked, |d| {
                d.child(div().absolute().inset_0().bg(gpui::linear_gradient(
                    180.,
                    gpui::linear_color_stop(rgba(0x0a0c10, 0.35), 0.),
                    gpui::linear_color_stop(rgba(0x0a0c10, 0.7), 1.),
                )))
            })
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
            .child(div().absolute().left_0().right_0().bottom_0().h(relative(if th.dark { 0.45 } else { 0.2 })).bg(gpui::linear_gradient(
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
        // Steam picks the Proton build of a Steam copy.
        let proton = if g.steam_only() {
            Some("Steam's Proton".to_owned())
        } else {
            g.settings
                .as_ref()
                .and_then(|s| s.proton.clone())
                .or_else(|| self.status.as_ref().and_then(|s| s.proton_default.clone()).map(|p| format!("{p} (default)")))
        };
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
            // Stereo: the split button rises as one control, and with its focus.
            .stereo_focused(focus_play)
            .child(main.id("hero-play-main").on_click(cx.listener(|this, _, _, cx| this.click(Target::Hero(0), cx))))
            .child(more.id("hero-play-more").on_click(cx.listener(move |this, _, _, cx| {
                this.nav.open_menu(id);
                cx.notify();
            })));
        let details = button("hero-details", th, Kind::OnArt, Size::Large, None, "Game details")
            .focus_glow(focus_details, th, 10.)
            .on_click(cx.listener(|this, _, _, cx| this.click(Target::Hero(1), cx)));

        let title_box = if bp.narrow() {
            TitleBox { max_w: 300f32.min(content_w), max_h: 80., max_scale: 1.3, text: 30., text_w: content_w }
        } else {
            TitleBox { max_w: 380f32.min(content_w), max_h: 96., max_scale: 1.4, text: 40., text_w: content_w }
        };
        // Stereo: the art is the plane; what's written over it is content, a step up, and
        // its buttons pop from that (the attention panel is a surface of its own).
        let content = div()
            .depth_surface()
            .when(!stacked, |d| d.absolute().left(px(gutter)).bottom(px(44.)))
            .when(stacked, |d| d.relative().mx(px(gutter)).mt_auto().pt(px(28.)).pb(px(36.)))
            .w(px(content_w))
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(caption("Continue playing", accent_on_art))
            .children(title_slot(&title, &g.entry.name, title_box))
            .when(!sub.is_empty(), |d| d.child(div().text_size(px(13.5)).text_color(on_art::tx2()).child(sub)))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(6.))
                    .when_some(proton, |d, p| d.child(art_chip(Some(Icon::Cpu), p)))
                    .when_some(g.current_revision(), |d, r| d.child(art_chip(Some(Icon::History), format!("Launch options r{r}"))))
                    .when(g.running(), |d| d.child(art_chip(Some(Icon::Gamepad), "Playing now"))),
            )
            .child(div().flex().flex_wrap().gap(px(10.)).mt(px(4.)).child(play).child(details));

        if !attention.is_empty() {
            let panel = self.render_attention(th, &attention, cx);
            hero = if stacked {
                hero.child(panel.relative().mx(px(gutter)).mt(px(20.)))
            } else {
                hero.child(panel.absolute().right(px(32.)).top(px(28.)).w(px(ATTENTION_W)))
            };
        }
        hero.child(content).into_any_element()
    }

    fn render_attention(&mut self, th: &Theme, items: &[(Gid, Attention)], cx: &mut Context<Self>) -> gpui::Div {
        let mut panel = div()
            .flex()
            .flex_col()
            .pt(px(14.))
            .px(px(16.))
            .pb(px(6.))
            .rounded(px(14.))
            .depth_surface()
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
            let thumb = self.art_for(&g, ArtSlot::Tile, Variant::Tile, cx);
            let (what, sub): (String, AnyElement) = match a {
                Attention::Update => (
                    "Update ready".into(),
                    div().text_size(px(11.5)).text_color(on_art::tx3()).child("Installs in place, keeps your compatibility folder").into_any_element(),
                ),
                Attention::Installing(f) => (
                    format!("Installing {:.0}%", (f * 100.).floor()),
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
                                    .flex_wrap()
                                    .gap_x(px(4.))
                                    .text_size(px(12.5))
                                    .text_color(on_art::tx1())
                                    .child(div().font_weight(SEMIBOLD).child(short_name(&g.entry.name)))
                                    .child(div().text_color(on_art::tx3()).child(format!("· {what}"))),
                            )
                            .child(sub),
                    )
                    .child(
                        button(("attn", k), th, Kind::OnArt, Size::Small, None, action)
                            .focus_ring(focused, th, 6.)
                            .on_click(cx.listener(move |this, _, _, cx| this.click(target, cx))),
                    ),
            );
        }
        panel
    }

    fn render_tile(&mut self, th: &Theme, id: Gid, w: f32, cx: &mut Context<Self>) -> AnyElement {
        let Some(g) = self.game(id).cloned() else { return div().into_any_element() };
        let h = (w * TILE_RATIO).round();
        // Installed on a paired machine counts as installed (it streams from there).
        let dim = !g.on_disk() && g.install_job().is_none() && g.installed_elsewhere().next().is_none();
        let on_machines = self.installed_on(&g);
        let art = self.art_for(&g, ArtSlot::Tile, if dim { Variant::TileDimmed } else { Variant::Tile }, cx);
        let focused = self.focused(Target::Tile(id));
        let label = short_name(&g.entry.name);

        let mut tl = div().absolute().left(px(8.)).top(px(8.)).flex().gap(px(4.));
        if g.running() {
            tl = tl.child(tag(th, TagKind::Accent, Some(Icon::Gamepad), "Playing"));
        } else if let Some(t) = self.stream_tag(&g) {
            let kind = if t == "Streaming" { TagKind::Accent } else { TagKind::Plain };
            tl = tl.child(tag(th, kind, Some(Icon::Cast), t));
        } else if g.failed().is_some() {
            tl = tl.child(tag(th, TagKind::Warn, Some(Icon::TriangleAlert), "Failed"));
        } else if g.paused() {
            tl = tl.child(tag(th, TagKind::Plain, Some(Icon::Pause), "Paused"));
        } else if g.queued() {
            tl = tl.child(tag(th, TagKind::Plain, Some(Icon::Download), "Queued"));
        } else if let Some(t) = g.update_tag() {
            tl = tl.child(tag(th, TagKind::Accent, Some(Icon::ArrowDownToLine), t));
        }
        let custom = id.ubisoft().and_then(|p| self.custom_art.get(&p)).is_some_and(|c| c.tile.is_some());
        // Store badges ([`tile_badges`]): other stores' games always, Ubisoft's too once a
        // third store is present.
        let present: Vec<StoreId> = self.library_stores().into_iter().map(|(s, _)| s).collect();
        let badges: Vec<&'static str> = tile_badges(&g.entry.stores(), &present).into_iter().map(StoreId::label).collect();

        let group = gpui::SharedString::from(format!("tile-{id}"));
        let deep = vec![soft_shadow(rgba(0x000000, 0.6), 18., 40.)];
        // Hover: a 2 px accent ring (no lift: GPUI has no transforms) and a deeper shadow.
        let hover_ring = div()
            .absolute()
            .top(px(-2.))
            .left(px(-2.))
            .right(px(-2.))
            .bottom(px(-2.))
            .rounded(px(12.))
            .border_2()
            .border_color(th.acc_line)
            .invisible()
            .group_hover(group.clone(), |s| s.visible());
        let mut boxart = div()
            .relative()
            .w(px(w))
            .h(px(h))
            .rounded(px(10.))
            .bg(th.ink3)
            .shadow(vec![soft_shadow(rgba(0x000000, 0.35), 6., 18.)])
            .when(!focused, |d| d.group_hover(group.clone(), move |s| s.shadow(deep.clone())).child(hover_ring))
            .child(art_fill(th, &art, &label, 10., false))
            .child(tl)
            .focus_glow(focused, th, 10.)
            // Stereo: the art and its caption rise together, as the tile (the control).
            .stereo_control(false);
        if custom || !badges.is_empty() {
            boxart = boxart.child(
                div()
                    .absolute()
                    .right(px(8.))
                    .top(px(8.))
                    .flex()
                    .gap(px(4.))
                    .children(badges.into_iter().map(|b| tag(th, TagKind::Plain, None, b)))
                    .when(custom, |d| d.child(tag(th, TagKind::Plain, Some(Icon::Image), ""))),
            );
        }
        // Merged library: where it's installed ("Tower", "This PC + Tower").
        if let Some((label, remote_only)) = on_machines.filter(|_| g.installing().is_none()) {
            boxart = boxart.child(
                div()
                    .absolute()
                    .left(px(8.))
                    .bottom(px(8.))
                    .flex()
                    .child(tag(th, if remote_only { TagKind::Accent } else { TagKind::Plain }, Some(Icon::Server), label)),
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
                            .child(format!("{:.0}%", (f * 100.).floor())),
                    )
                    .child(progress(th, f, None, 4., None)),
            );
        } else if (g.installed() || g.client().is_some_and(|c| c.complete)) && !focused {
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
            .id(("tile", id.ix()))
            // Stereo: a control on the grid's surface; it pops further when focused or hovered.
            .stereo_focused(focused)
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
    if g.running() && g.steam_only() {
        // Started from Steam: quit it there (or in the game).
        ("Playing", Icon::Gamepad)
    } else if g.running() {
        ("Stop", Icon::Square)
    } else if g.installing().is_some() {
        ("Installing", Icon::Download)
    } else if g.queued() {
        ("Queued", Icon::Download)
    } else if g.steam_only() {
        ("Play", Icon::Play)
    } else if !g.entry.game.is_ubisoft() && !g.entry.on_disk() && !g.entry.installable {
        // Another store's game Quiver can't install yet (Epic before phase 2).
        ("Not installed", Icon::Info)
    } else if !g.installed() {
        ("Install", Icon::Download)
    } else if g.entry.update_available {
        ("Update", Icon::ArrowDownToLine)
    } else {
        ("Play", Icon::Play)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::Demo;
    use quiver_api::{LibraryEntry, LoginResult, Request};

    fn status(demo: &Demo) -> Status {
        serde_json::from_value(demo.handle(Request::Status).unwrap()).unwrap()
    }

    fn library(demo: &Demo) -> Vec<LibraryEntry> {
        serde_json::from_value(demo.handle(Request::Library { refresh: false, all_stores: true }).unwrap()).unwrap()
    }

    /// The stores present, as `Quiver::library_stores` finds them.
    fn present(lib: &[LibraryEntry]) -> Vec<StoreId> {
        StoreId::ALL.into_iter().filter(|s| lib.iter().any(|e| e.stores().contains(s))).collect()
    }

    #[test]
    fn badges_other_stores_until_a_third_store_is_present() {
        use StoreId::*;
        let two = [Ubisoft, Steam];
        assert_eq!(tile_badges(&[Ubisoft], &two), []);
        assert_eq!(tile_badges(&[Steam], &two), [Steam]);
        // A Ubisoft game with its Steam copy: the copy's store shows.
        assert_eq!(tile_badges(&[Ubisoft, Steam], &two), [Steam]);
        // Signed out of Ubisoft, only Steam's games: Ubisoft still counts as a store, so
        // Steam's tiles are badged.
        assert_eq!(tile_badges(&[Steam], &[Steam]), [Steam]);
        assert_eq!(tile_badges(&[Ubisoft], &[Ubisoft]), []);
        // A third store: every tile says where it's from.
        let three = [Ubisoft, Steam, Epic];
        assert_eq!(tile_badges(&[Ubisoft], &three), [Ubisoft]);
        assert_eq!(tile_badges(&[Epic], &three), [Epic]);
        assert_eq!(tile_badges(&[Ubisoft, Steam], &three), [Ubisoft, Steam]);
        assert_eq!(tile_badges(&[Ubisoft], &[Steam, Epic]), [Ubisoft]);
    }

    #[test]
    fn sign_in_card_follows_the_session() {
        let mut s: Status = serde_json::from_value(serde_json::json!({
            "version": "t", "signed_in": null, "awaiting_2fa": false, "running": [], "jobs": [], "proton_default": null
        }))
        .unwrap();
        assert_eq!(UbisoftCard::of(None), None, "not connected yet: no card");
        assert_eq!(UbisoftCard::of(Some(&s)), Some(UbisoftCard::SignIn));
        s.awaiting_2fa = true;
        assert_eq!(UbisoftCard::of(Some(&s)), Some(UbisoftCard::EnterCode));
        s.awaiting_2fa = false;
        s.accounts.push(quiver_api::Account { user_id: "u".into(), name: "n".into(), default: true });
        assert_eq!(UbisoftCard::of(Some(&s)), Some(UbisoftCard::SignInAgain));
        s.signed_in = Some("n".into());
        assert_eq!(UbisoftCard::of(Some(&s)), None);
    }

    /// Demo mode, signed out: the library has Steam's games, badged, and the sign-in card;
    /// signing in (the card's form: email and password, then the code) brings Ubisoft's
    /// games in, unbadged, and takes the card away.
    #[tokio::test]
    async fn signed_out_demo_then_sign_in_from_the_card() {
        let (demo, _events) = Demo::start_signed_out();
        let lib = library(&demo);
        assert!(!lib.is_empty() && lib.iter().all(|e| e.game.store == StoreId::Steam));
        assert_eq!(UbisoftCard::of(Some(&status(&demo))), Some(UbisoftCard::SignIn));
        let p = present(&lib);
        assert!(lib.iter().all(|e| tile_badges(&e.stores(), &p) == [StoreId::Steam]));

        // What `submit_login` sends.
        let r: LoginResult =
            serde_json::from_value(demo.handle(Request::Login { email: "me@example.com".into(), password: quiver_api::secret::Secret::new("pw".into()) }).unwrap()).unwrap();
        assert!(matches!(r, LoginResult::TwoFactorRequired));
        assert_eq!(UbisoftCard::of(Some(&status(&demo))), Some(UbisoftCard::EnterCode), "a form left at the code: the card says so");
        let r: LoginResult = serde_json::from_value(demo.handle(Request::Login2fa { code: quiver_api::secret::Secret::new("246810".into()) }).unwrap()).unwrap();
        assert!(matches!(r, LoginResult::SignedIn { .. }));

        assert_eq!(UbisoftCard::of(Some(&status(&demo))), None);
        let lib = library(&demo);
        let p = present(&lib);
        assert_eq!(p, [StoreId::Ubisoft, StoreId::Steam]);
        let ubisoft: Vec<&LibraryEntry> = lib.iter().filter(|e| e.game.is_ubisoft()).collect();
        assert!(!ubisoft.is_empty());
        // Unbadged with two stores; a Steam copy shows as Steam.
        let expect = |e: &LibraryEntry| if e.steam.is_some() { vec![StoreId::Steam] } else { vec![] };
        assert!(ubisoft.iter().all(|e| tile_badges(&e.stores(), &p) == expect(e)), "Ubisoft tiles stay unbadged with two stores");
        assert!(lib.iter().filter(|e| e.game.store == StoreId::Steam).all(|e| tile_badges(&e.stores(), &p) == [StoreId::Steam]));
    }
}
