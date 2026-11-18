//! Focus and navigation, independent of GPUI and of the input device. Keyboard and gamepad
//! both produce [`Input`]s; [`Nav::apply`] moves focus spatially over what's on screen
//! (described by a [`Layout`] the view builds each time) and returns an [`Effect`] for
//! anything the app itself has to do (play, run a palette command, ...).

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Section {
    Library,
    Downloads,
    Activity,
}

impl Section {
    pub const ALL: [Section; 3] = [Section::Library, Section::Downloads, Section::Activity];

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    pub fn label(self) -> &'static str {
        match self {
            Section::Library => "Library",
            Section::Downloads => "Downloads",
            Section::Activity => "Activity",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Page {
    Library,
    Game(Gid),
    Downloads,
    Activity,
    /// Global settings; the section is [`Nav::settings`].
    Settings,
}

/// The sections of the Settings page, in the order of its side list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SettingsSection {
    Accounts,
    Library,
    Compatibility,
    /// The performance HUD's defaults for every game (`ui/perf.rs`).
    Performance,
    /// Quiver Tweaks at the global scope (`ui/game/tweaks/`).
    Tweaks,
    Steam,
    Appearance,
    Automation,
    About,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 9] = [
        SettingsSection::Accounts,
        SettingsSection::Library,
        SettingsSection::Compatibility,
        SettingsSection::Performance,
        SettingsSection::Tweaks,
        SettingsSection::Steam,
        SettingsSection::Appearance,
        SettingsSection::Automation,
        SettingsSection::About,
    ];

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::Accounts => "Accounts",
            SettingsSection::Library => "Library and storage",
            SettingsSection::Compatibility => "Compatibility",
            SettingsSection::Performance => "Performance HUD",
            SettingsSection::Tweaks => "Tweaks",
            SettingsSection::Steam => "Steam and shortcuts",
            SettingsSection::Appearance => "Appearance",
            SettingsSection::Automation => "Automation and CLI",
            SettingsSection::About => "About",
        }
    }

    pub fn step(self, delta: isize) -> SettingsSection {
        let i = (self.index() as isize + delta).clamp(0, Self::ALL.len() as isize - 1);
        Self::ALL[i as usize]
    }
}

impl Page {
    pub fn section(self) -> Section {
        match self {
            Page::Library | Page::Game(_) | Page::Settings => Section::Library,
            Page::Downloads => Section::Downloads,
            Page::Activity => Section::Activity,
        }
    }

    fn of(section: Section) -> Page {
        match section {
            Section::Library => Page::Library,
            Section::Downloads => Page::Downloads,
            Section::Activity => Page::Activity,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Tab {
    Overview,
    LaunchOptions,
    Mods,
    /// Quiver Tweaks for the game and its profiles (`ui/game/tweaks/`); other stores' games
    /// have it too.
    Tweaks,
    /// Performance summaries, comparisons, live numbers and the HUD (`ui/game/perf.rs`).
    Performance,
    Saves,
    AddOns,
    Files,
    Logs,
}

impl Tab {
    pub const ALL: [Tab; 9] = [Tab::Overview, Tab::LaunchOptions, Tab::Mods, Tab::Tweaks, Tab::Performance, Tab::Saves, Tab::AddOns, Tab::Files, Tab::Logs];
    /// Other stores' games: their client's facts and Quiver Tweaks.
    pub const STORE: [Tab; 2] = [Tab::Overview, Tab::Tweaks];
    /// Steam games: also Mods (with their Options), as the Steam store's capabilities say
    /// (`mods: quiver`); they reach the game when it starts through `quiver steam-run`.
    pub const STEAM: [Tab; 3] = [Tab::Overview, Tab::Mods, Tab::Tweaks];
    /// Epic games: also Saves & progress (cloud saves, play time, achievements; phase 3).
    pub const EPIC: [Tab; 3] = [Tab::Overview, Tab::Tweaks, Tab::Saves];

    /// The tabs a game's page has.
    pub fn of(id: Gid) -> &'static [Tab] {
        match id.store() {
            quiver_api::StoreId::Ubisoft => &Self::ALL,
            quiver_api::StoreId::Steam => &Self::STEAM,
            quiver_api::StoreId::Epic => &Self::EPIC,
            _ => &Self::STORE,
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    pub fn label(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::LaunchOptions => "Launch options",
            Tab::Mods => "Mods",
            Tab::Tweaks => "Tweaks",
            Tab::Performance => "Performance",
            Tab::Saves => "Saves & progress",
            Tab::AddOns => "Add-ons",
            Tab::Files => "Files",
            Tab::Logs => "Logs",
        }
    }

    /// The next tab of `id`'s page (a tab the page doesn't have steps from its first).
    fn step_in(self, id: Gid, delta: isize) -> Tab {
        let list = Self::of(id);
        let Some(cur) = list.iter().position(|t| *t == self) else { return list[0] };
        let i = (cur as isize + delta).clamp(0, list.len() as isize - 1);
        list[i as usize]
    }
}

/// A focus region. Each keeps its own remembered index in [`Nav`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Zone {
    /// Library · Downloads · Activity in the top bar.
    TopBar,
    /// The library home hero's buttons (Play, Game details, ...).
    Hero,
    /// The library grid.
    Grid,
    /// The game list on the left of a game page, or the section list of Settings.
    Side,
    /// The game page's action bar (Play, ...).
    Actions,
    /// The game page's tabs.
    Tabs,
    /// Rows in the page body (sessions, downloads, activity entries).
    Body,
}

/// The input vocabulary, shared with Sight's panel.
pub use quiver_ui::input::Input;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Overlay {
    Palette { selected: usize },
    /// The menu for one game.
    Menu { game: Gid, selected: usize },
}

/// What is focusable on screen right now, in display order. Built by the view.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    /// Library grid, product ids in order.
    pub grid: Vec<Gid>,
    /// Grid columns at the current window width (at least 1).
    pub columns: usize,
    /// Buttons in the library hero (0 when there is no hero).
    pub hero_buttons: usize,
    /// The game list next to a game page.
    pub side: Vec<Gid>,
    /// Buttons in the game page's action bar.
    pub actions: usize,
    /// Focusable rows in the page body.
    pub body: usize,
    pub palette_items: usize,
    pub menu_items: usize,
}

/// The focused thing, for drawing the focus ring and for activation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Section(Section),
    Hero(usize),
    Tile(Gid),
    SideRow(Gid),
    Action(usize),
    Tab(Tab),
    Body(usize),
    /// A section in the Settings side list.
    SettingsNav(SettingsSection),
    PaletteItem(usize),
    MenuItem(usize),
}

/// What the app has to do after an input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Nothing happened (edge of a region, nothing focusable).
    None,
    /// Focus, page, tab or overlay changed; redraw and scroll focus into view.
    Moved,
    /// Library hero button `i` was activated.
    Hero(usize),
    /// Game page action `i` was activated.
    Action(Gid, usize),
    /// Body row `i` was activated.
    Body(usize),
    /// Play this game.
    Play(Gid),
    /// Run palette entry `i` (the palette has closed).
    Palette(usize),
    /// Run menu entry `i` for this game (the menu has closed).
    Menu(Gid, usize),
}

// Serializable so a relaunch can carry it over (`selfupdate.rs`).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Nav {
    pub page: Page,
    pub tab: Tab,
    pub zone: Zone,
    pub overlay: Option<Overlay>,
    /// The Settings page's section (kept while elsewhere, like a remembered tab).
    pub settings: SettingsSection,
    top: usize,
    hero: usize,
    grid: usize,
    action: usize,
    body: usize,
}

impl Default for Nav {
    fn default() -> Self {
        Self::new()
    }
}

impl Nav {
    pub fn new() -> Self {
        Self {
            page: Page::Library,
            tab: Tab::Overview,
            zone: Zone::Grid,
            overlay: None,
            settings: SettingsSection::Accounts,
            top: 0,
            hero: 0,
            grid: 0,
            action: 0,
            body: 0,
        }
    }

    pub fn grid_index(&self) -> usize {
        self.grid
    }

    pub fn body_index(&self) -> usize {
        self.body
    }

    /// The focused element, if any, clamped to the layout.
    pub fn focused(&self, l: &Layout) -> Option<Target> {
        if let Some(o) = self.overlay {
            return match o {
                Overlay::Palette { selected } if l.palette_items > 0 => Some(Target::PaletteItem(selected)),
                Overlay::Menu { selected, .. } if l.menu_items > 0 => Some(Target::MenuItem(selected)),
                _ => None,
            };
        }
        match self.zone {
            Zone::TopBar => Some(Target::Section(Section::ALL[self.top.min(2)])),
            Zone::Hero if l.hero_buttons > 0 => Some(Target::Hero(self.hero.min(l.hero_buttons - 1))),
            Zone::Grid => l.grid.get(self.grid.min(l.grid.len().saturating_sub(1))).map(|id| Target::Tile(*id)),
            Zone::Side => match self.page {
                Page::Game(id) => Some(Target::SideRow(id)),
                Page::Settings => Some(Target::SettingsNav(self.settings)),
                _ => None,
            },
            Zone::Actions if l.actions > 0 => Some(Target::Action(self.action.min(l.actions - 1))),
            Zone::Tabs => Some(Target::Tab(self.tab)),
            Zone::Body if l.body > 0 => Some(Target::Body(self.body.min(l.body - 1))),
            _ => None,
        }
    }

    pub fn is_focused(&self, l: &Layout, t: Target) -> bool {
        self.focused(l) == Some(t)
    }

    /// The game the menu and Play act on: the focused tile, or the open game page.
    pub fn focused_game(&self, l: &Layout) -> Option<Gid> {
        match self.page {
            Page::Game(id) => Some(id),
            Page::Library => match self.zone {
                Zone::Grid => l.grid.get(self.grid.min(l.grid.len().saturating_sub(1))).copied(),
                _ => None,
            },
            _ => None,
        }
    }

    // -----------------------------------------------------------------------------------------
    // Direct moves (mouse clicks, deep links)

    pub fn open_game(&mut self, id: Gid) {
        self.overlay = None;
        if self.page != Page::Game(id) {
            self.tab = Tab::Overview;
            self.body = 0;
        }
        self.page = Page::Game(id);
        self.zone = Zone::Actions;
        self.action = 0;
    }

    pub fn open_section(&mut self, s: Section) {
        self.overlay = None;
        self.top = s.index();
        self.page = Page::of(s);
        self.zone = self.main_zone_for(self.page, None);
        self.body = 0;
    }

    /// Open Settings at a section, with focus on the section list.
    pub fn open_settings(&mut self, s: SettingsSection) {
        self.overlay = None;
        self.page = Page::Settings;
        self.settings = s;
        self.zone = Zone::Side;
        self.body = 0;
    }

    pub fn select_tab(&mut self, t: Tab) {
        self.tab = t;
        self.body = 0;
    }

    /// Focus a target directly (a click), without activating it.
    pub fn focus(&mut self, l: &Layout, t: Target) {
        match t {
            Target::Section(s) => {
                self.zone = Zone::TopBar;
                self.top = s.index();
            }
            Target::Hero(i) => {
                self.zone = Zone::Hero;
                self.hero = i;
            }
            Target::Tile(id) => {
                if let Some(i) = l.grid.iter().position(|g| *g == id) {
                    self.zone = Zone::Grid;
                    self.grid = i;
                }
            }
            Target::SideRow(id) => {
                self.page = Page::Game(id);
                self.zone = Zone::Side;
            }
            Target::Action(i) => {
                self.zone = Zone::Actions;
                self.action = i;
            }
            Target::Tab(tab) => {
                self.zone = Zone::Tabs;
                self.tab = tab;
            }
            Target::Body(i) => {
                self.zone = Zone::Body;
                self.body = i;
            }
            Target::SettingsNav(s) => {
                if self.page != Page::Settings || self.settings != s {
                    self.body = 0;
                }
                self.page = Page::Settings;
                self.settings = s;
                self.zone = Zone::Side;
            }
            Target::PaletteItem(i) => {
                if let Some(Overlay::Palette { selected }) = &mut self.overlay {
                    *selected = i;
                }
            }
            Target::MenuItem(i) => {
                if let Some(Overlay::Menu { selected, .. }) = &mut self.overlay {
                    *selected = i;
                }
            }
        }
    }

    pub fn open_palette(&mut self) {
        self.overlay = Some(Overlay::Palette { selected: 0 });
    }

    pub fn open_menu(&mut self, game: Gid) {
        self.overlay = Some(Overlay::Menu { game, selected: 0 });
    }

    pub fn close_overlay(&mut self) {
        self.overlay = None;
    }

    fn main_zone_for(&self, page: Page, l: Option<&Layout>) -> Zone {
        match page {
            Page::Library => match l {
                Some(l) if l.hero_buttons > 0 => Zone::Hero,
                _ => Zone::Grid,
            },
            Page::Game(_) => Zone::Actions,
            Page::Downloads | Page::Activity => Zone::Body,
            Page::Settings => Zone::Side,
        }
    }

    // -----------------------------------------------------------------------------------------
    // Input

    pub fn apply(&mut self, input: Input, l: &Layout) -> Effect {
        self.clamp(l);
        if let Some(o) = self.overlay {
            return self.apply_overlay(o, input, l);
        }
        match input {
            Input::Palette => {
                self.open_palette();
                Effect::Moved
            }
            Input::Menu | Input::PlayOptions => match self.focused_game(l) {
                Some(id) => {
                    self.open_menu(id);
                    Effect::Moved
                }
                None => Effect::None,
            },
            Input::Play => self.focused_game(l).map(Effect::Play).unwrap_or(Effect::None),
            Input::PrevSection | Input::NextSection => {
                let d = if input == Input::NextSection { 1 } else { Section::ALL.len() - 1 };
                let s = Section::ALL[(self.page.section().index() + d) % Section::ALL.len()];
                self.open_section(s);
                self.zone = self.main_zone_for(self.page, Some(l));
                Effect::Moved
            }
            Input::PrevTab | Input::NextTab => match self.page {
                Page::Game(id) => {
                    let t = self.tab.step_in(id, if input == Input::NextTab { 1 } else { -1 });
                    if t == self.tab {
                        return Effect::None;
                    }
                    self.select_tab(t);
                    if self.zone == Zone::Body {
                        self.zone = Zone::Tabs;
                    }
                    Effect::Moved
                }
                // LT / RT step through the Settings sections.
                Page::Settings => {
                    let s = self.settings.step(if input == Input::NextTab { 1 } else { -1 });
                    if s == self.settings {
                        return Effect::None;
                    }
                    self.settings = s;
                    self.body = 0;
                    if self.zone == Zone::Body {
                        self.zone = Zone::Side;
                    }
                    Effect::Moved
                }
                _ => Effect::None,
            },
            Input::Back => self.back(l),
            Input::Activate => self.activate(l),
            Input::Up | Input::Down | Input::Left | Input::Right => self.step(input, l),
        }
    }

    fn apply_overlay(&mut self, o: Overlay, input: Input, l: &Layout) -> Effect {
        let (n, sel) = match o {
            Overlay::Palette { selected } => (l.palette_items, selected),
            Overlay::Menu { selected, .. } => (l.menu_items, selected),
        };
        let set = |nav: &mut Nav, v: usize| match &mut nav.overlay {
            Some(Overlay::Palette { selected }) | Some(Overlay::Menu { selected, .. }) => *selected = v,
            None => {}
        };
        match input {
            Input::Up | Input::Left if n > 0 => {
                set(self, (sel + n - 1) % n);
                Effect::Moved
            }
            Input::Down | Input::Right if n > 0 => {
                set(self, (sel + 1) % n);
                Effect::Moved
            }
            Input::Activate if n > 0 => {
                self.overlay = None;
                match o {
                    Overlay::Palette { .. } => Effect::Palette(sel.min(n - 1)),
                    Overlay::Menu { game, .. } => Effect::Menu(game, sel.min(n - 1)),
                }
            }
            Input::Back => {
                self.overlay = None;
                Effect::Moved
            }
            // The same button closes what it opened.
            Input::Palette if matches!(o, Overlay::Palette { .. }) => {
                self.overlay = None;
                Effect::Moved
            }
            Input::Menu | Input::PlayOptions if matches!(o, Overlay::Menu { .. }) => {
                self.overlay = None;
                Effect::Moved
            }
            Input::Palette => {
                self.open_palette();
                Effect::Moved
            }
            _ => Effect::None,
        }
    }

    fn back(&mut self, l: &Layout) -> Effect {
        match self.page {
            Page::Game(id) => {
                self.page = Page::Library;
                self.top = 0;
                if let Some(i) = l.grid.iter().position(|g| *g == id) {
                    self.grid = i;
                }
                self.zone = Zone::Grid;
                Effect::Moved
            }
            Page::Settings if self.zone == Zone::Body => {
                self.zone = Zone::Side;
                Effect::Moved
            }
            Page::Downloads | Page::Activity | Page::Settings => {
                self.open_section(Section::Library);
                self.zone = self.main_zone_for(Page::Library, Some(l));
                Effect::Moved
            }
            Page::Library if self.zone == Zone::TopBar => {
                self.zone = self.main_zone_for(Page::Library, Some(l));
                Effect::Moved
            }
            Page::Library => Effect::None,
        }
    }

    fn activate(&mut self, l: &Layout) -> Effect {
        match self.focused(l) {
            None => Effect::None,
            Some(Target::Section(s)) => {
                self.open_section(s);
                self.zone = Zone::TopBar;
                Effect::Moved
            }
            Some(Target::Hero(i)) => Effect::Hero(i),
            Some(Target::Tile(id)) => {
                self.open_game(id);
                Effect::Moved
            }
            Some(Target::SideRow(_)) => {
                self.zone = Zone::Actions;
                self.action = 0;
                Effect::Moved
            }
            Some(Target::Action(i)) => match self.page {
                Page::Game(id) => Effect::Action(id, i),
                _ => Effect::None,
            },
            Some(Target::Tab(_)) => {
                if l.body > 0 {
                    self.zone = Zone::Body;
                    self.body = 0;
                    Effect::Moved
                } else {
                    Effect::None
                }
            }
            Some(Target::Body(i)) => Effect::Body(i),
            Some(Target::SettingsNav(_)) => {
                if l.body > 0 {
                    self.zone = Zone::Body;
                    self.body = 0;
                    Effect::Moved
                } else {
                    Effect::None
                }
            }
            Some(Target::PaletteItem(_)) | Some(Target::MenuItem(_)) => Effect::None,
        }
    }

    fn step(&mut self, dir: Input, l: &Layout) -> Effect {
        use Input::*;
        let before = (self.page, self.zone, self.top, self.hero, self.grid, self.action, self.body, self.tab, self.settings);
        match (self.page, self.zone, dir) {
            // Top bar: a row of three; down enters the page.
            (_, Zone::TopBar, Left) => self.top = self.top.saturating_sub(1),
            (_, Zone::TopBar, Right) => self.top = (self.top + 1).min(Section::ALL.len() - 1),
            (page, Zone::TopBar, Down) => {
                let z = self.main_zone_for(page, Some(l));
                if self.zone_has_items(z, l) {
                    self.zone = z;
                }
            }
            (_, Zone::TopBar, Up) => {}

            // Library hero: a row of buttons above the grid.
            (_, Zone::Hero, Left) => self.hero = self.hero.saturating_sub(1),
            (_, Zone::Hero, Right) => self.hero = (self.hero + 1).min(l.hero_buttons.saturating_sub(1)),
            (_, Zone::Hero, Up) => self.zone = Zone::TopBar,
            (_, Zone::Hero, Down) => {
                if !l.grid.is_empty() {
                    self.zone = Zone::Grid;
                    let cols = l.columns.max(1);
                    self.grid = (self.grid % cols).min(l.grid.len() - 1);
                }
            }

            // Grid: reading order for left/right, columns for up/down.
            (_, Zone::Grid, Left) => self.grid = self.grid.saturating_sub(1),
            (_, Zone::Grid, Right) => self.grid = (self.grid + 1).min(l.grid.len().saturating_sub(1)),
            (_, Zone::Grid, Up) => {
                let cols = l.columns.max(1);
                if self.grid >= cols {
                    self.grid -= cols;
                } else if l.hero_buttons > 0 {
                    self.zone = Zone::Hero;
                } else {
                    self.zone = Zone::TopBar;
                }
            }
            (_, Zone::Grid, Down) => {
                let cols = l.columns.max(1);
                let rows = l.grid.len().div_ceil(cols);
                if self.grid / cols + 1 < rows {
                    self.grid = (self.grid + cols).min(l.grid.len() - 1);
                }
            }

            // Game page: the list on the left, the page on the right.
            (Page::Game(id), Zone::Side, Up | Down) => {
                if let Some(i) = l.side.iter().position(|g| *g == id) {
                    let j = if dir == Up { i.checked_sub(1) } else { Some(i + 1).filter(|j| *j < l.side.len()) };
                    if let Some(j) = j {
                        self.page = Page::Game(l.side[j]);
                        self.tab = Tab::Overview;
                        self.body = 0;
                    } else if dir == Up {
                        self.zone = Zone::TopBar;
                    }
                } else if let Some(first) = l.side.first() {
                    self.page = Page::Game(*first);
                }
            }
            (Page::Game(_), Zone::Side, Right) => {
                self.zone = Zone::Actions;
                self.action = 0;
            }
            (Page::Game(_), Zone::Side, Left) => {}

            (Page::Game(_), Zone::Actions, Left) => {
                if self.action == 0 {
                    self.zone = Zone::Side;
                } else {
                    self.action -= 1;
                }
            }
            (Page::Game(_), Zone::Actions, Right) => self.action = (self.action + 1).min(l.actions.saturating_sub(1)),
            (Page::Game(_), Zone::Actions, Up) => self.zone = Zone::TopBar,
            (Page::Game(_), Zone::Actions, Down) => self.zone = Zone::Tabs,

            // Tabs: selection follows focus.
            (Page::Game(id), Zone::Tabs, Left) => {
                if self.tab.index() == 0 {
                    self.zone = Zone::Side;
                } else {
                    self.select_tab(self.tab.step_in(id, -1));
                }
            }
            (Page::Game(id), Zone::Tabs, Right) => self.select_tab(self.tab.step_in(id, 1)),
            (Page::Game(_), Zone::Tabs, Up) => self.zone = Zone::Actions,
            (Page::Game(_), Zone::Tabs, Down) => {
                if l.body > 0 {
                    self.zone = Zone::Body;
                    self.body = 0;
                }
            }

            // Settings: the section list on the left, the section's controls on the right.
            (Page::Settings, Zone::Side, Up) => match self.settings.index() {
                0 => self.zone = Zone::TopBar,
                _ => {
                    self.settings = self.settings.step(-1);
                    self.body = 0;
                }
            },
            (Page::Settings, Zone::Side, Down) => {
                let s = self.settings.step(1);
                if s != self.settings {
                    self.settings = s;
                    self.body = 0;
                }
            }
            (Page::Settings, Zone::Side, Right) => {
                if l.body > 0 {
                    self.zone = Zone::Body;
                }
            }
            (Page::Settings, Zone::Side, Left) => {}
            (Page::Settings, Zone::Body, Left) => self.zone = Zone::Side,

            (page, Zone::Body, Up) => {
                if self.body > 0 {
                    self.body -= 1;
                } else if matches!(page, Page::Game(_)) {
                    self.zone = Zone::Tabs;
                } else {
                    self.zone = Zone::TopBar;
                }
            }
            (_, Zone::Body, Down) => self.body = (self.body + 1).min(l.body.saturating_sub(1)),
            (Page::Game(_), Zone::Body, Left) => self.zone = Zone::Side,
            (_, Zone::Body, Left | Right) => {}

            // Zones that don't belong to the page (stale state): go to its main zone.
            (page, _, _) => self.zone = self.main_zone_for(page, Some(l)),
        }
        let after = (self.page, self.zone, self.top, self.hero, self.grid, self.action, self.body, self.tab, self.settings);
        if before == after { Effect::None } else { Effect::Moved }
    }

    fn zone_has_items(&self, z: Zone, l: &Layout) -> bool {
        match z {
            Zone::TopBar | Zone::Tabs => true,
            Zone::Hero => l.hero_buttons > 0,
            Zone::Grid => !l.grid.is_empty(),
            Zone::Side => self.page == Page::Settings || !l.side.is_empty(),
            Zone::Actions => l.actions > 0,
            Zone::Body => l.body > 0,
        }
    }

    /// Keep remembered indices inside the current layout, and the zone valid for the page.
    fn clamp(&mut self, l: &Layout) {
        self.hero = self.hero.min(l.hero_buttons.saturating_sub(1));
        self.grid = self.grid.min(l.grid.len().saturating_sub(1));
        self.action = self.action.min(l.actions.saturating_sub(1));
        self.body = self.body.min(l.body.saturating_sub(1));
        let valid = match (self.page, self.zone) {
            (_, Zone::TopBar) => true,
            (Page::Library, Zone::Hero) => l.hero_buttons > 0,
            (Page::Library, Zone::Grid) => true,
            (Page::Game(_), Zone::Side | Zone::Actions | Zone::Tabs) => true,
            (Page::Settings, Zone::Side) => true,
            (_, Zone::Body) => !matches!(self.page, Page::Library),
            _ => false,
        };
        if !valid {
            self.zone = self.main_zone_for(self.page, Some(l));
        }
        let n = match self.overlay {
            Some(Overlay::Palette { .. }) => l.palette_items,
            _ => l.menu_items,
        };
        if let Some(Overlay::Palette { selected }) | Some(Overlay::Menu { selected, .. }) = &mut self.overlay {
            *selected = (*selected).min(n.saturating_sub(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Input::*;

    fn lib_layout() -> Layout {
        Layout {
            grid: (1..=19).map(Gid::from).collect(), // 8 + 8 + 3
            columns: 8,
            hero_buttons: 2,
            side: (1..=19).map(Gid::from).collect(),
            actions: 5,
            body: 4,
            palette_items: 6,
            menu_items: 4,
        }
    }

    fn run(nav: &mut Nav, l: &Layout, inputs: &[Input]) -> Effect {
        let mut last = Effect::None;
        for i in inputs {
            last = nav.apply(*i, l);
        }
        last
    }

    #[test]
    fn grid_moves_in_reading_order_and_by_columns() {
        let l = lib_layout();
        let mut n = Nav::new();
        assert_eq!(n.focused(&l), Some(Target::Tile(Gid::from(1))));
        run(&mut n, &l, &[Right, Right, Down]);
        assert_eq!(n.focused(&l), Some(Target::Tile(Gid::from(11))));
        // Down into the short last row clamps to its last item.
        run(&mut n, &l, &[Right, Right, Right, Down]);
        assert_eq!(n.focused(&l), Some(Target::Tile(Gid::from(19))));
        // Last row: down stays put.
        assert_eq!(n.apply(Down, &l), Effect::None);
        // Right wraps into the next row; left at the very start stays.
        let mut n = Nav::new();
        run(&mut n, &l, &[Right, Right, Right, Right, Right, Right, Right, Right]);
        assert_eq!(n.focused(&l), Some(Target::Tile(Gid::from(9))));
        let mut n = Nav::new();
        assert_eq!(n.apply(Left, &l), Effect::None);
    }

    #[test]
    fn up_from_first_row_reaches_hero_then_top_bar() {
        let l = lib_layout();
        let mut n = Nav::new();
        run(&mut n, &l, &[Right, Right, Up]);
        assert_eq!(n.focused(&l), Some(Target::Hero(0)));
        run(&mut n, &l, &[Right]);
        assert_eq!(n.focused(&l), Some(Target::Hero(1)));
        run(&mut n, &l, &[Up]);
        assert_eq!(n.focused(&l), Some(Target::Section(Section::Library)));
        run(&mut n, &l, &[Down, Down]);
        // Back in the grid, same column as before.
        assert_eq!(n.focused(&l), Some(Target::Tile(Gid::from(3))));
        // No hero: straight to the top bar.
        let l2 = Layout { hero_buttons: 0, ..lib_layout() };
        let mut n = Nav::new();
        run(&mut n, &l2, &[Up]);
        assert_eq!(n.zone, Zone::TopBar);
    }

    #[test]
    fn activating_a_tile_opens_the_game_and_back_returns_to_it() {
        let l = lib_layout();
        let mut n = Nav::new();
        run(&mut n, &l, &[Down, Right]);
        assert_eq!(n.focused(&l), Some(Target::Tile(Gid::from(10))));
        assert_eq!(n.apply(Activate, &l), Effect::Moved);
        assert_eq!(n.page, Page::Game(Gid::from(10)));
        assert_eq!(n.focused(&l), Some(Target::Action(0)));
        assert_eq!(n.apply(Activate, &l), Effect::Action(Gid::from(10), 0));
        assert_eq!(n.apply(Back, &l), Effect::Moved);
        assert_eq!(n.page, Page::Library);
        assert_eq!(n.focused(&l), Some(Target::Tile(Gid::from(10))));
    }

    #[test]
    fn side_list_moves_select_games_and_right_enters_the_page() {
        let l = lib_layout();
        let mut n = Nav::new();
        n.open_game(Gid::from(5));
        run(&mut n, &l, &[Left]);
        assert_eq!(n.focused(&l), Some(Target::SideRow(Gid::from(5))));
        run(&mut n, &l, &[Down, Down]);
        assert_eq!(n.page, Page::Game(Gid::from(7)));
        run(&mut n, &l, &[Up]);
        assert_eq!(n.page, Page::Game(Gid::from(6)));
        run(&mut n, &l, &[Right]);
        assert_eq!(n.focused(&l), Some(Target::Action(0)));
        // A on a side row jumps to Play too.
        run(&mut n, &l, &[Left, Activate]);
        assert_eq!(n.focused(&l), Some(Target::Action(0)));
        // Up from the first game reaches the top bar.
        n.open_game(Gid::from(1));
        run(&mut n, &l, &[Left, Up]);
        assert_eq!(n.zone, Zone::TopBar);
    }

    #[test]
    fn other_stores_games_have_overview_and_tweaks() {
        let l = lib_layout();
        let mut n = Nav::new();
        let steam = Gid(quiver_api::GameRef::steam(620));
        n.open_game(steam);
        run(&mut n, &l, &[NextTab]);
        assert_eq!(n.tab, Tab::Mods, "Steam games have mods");
        run(&mut n, &l, &[NextTab]);
        assert_eq!(n.tab, Tab::Tweaks);
        assert_eq!(n.apply(NextTab, &l), Effect::None, "the last tab");
        run(&mut n, &l, &[PrevTab, PrevTab]);
        assert_eq!(n.tab, Tab::Overview);
        assert_eq!(Tab::of(Gid::from(3539)).len(), Tab::ALL.len());
        let epic = Gid(quiver_api::GameRef::new(quiver_api::StoreId::Epic, "fn").unwrap());
        assert_eq!(Tab::of(epic), &Tab::EPIC, "Epic games also have Saves & progress");
    }

    #[test]
    fn tabs_follow_triggers_and_focus() {
        let l = lib_layout();
        let mut n = Nav::new();
        n.open_game(Gid::from(3));
        assert_eq!(n.apply(PrevTab, &l), Effect::None);
        run(&mut n, &l, &[NextTab, NextTab]);
        assert_eq!(n.tab, Tab::Mods);
        run(&mut n, &l, &[Down]);
        assert_eq!(n.focused(&l), Some(Target::Tab(Tab::Mods)));
        run(&mut n, &l, &[Right]);
        assert_eq!(n.tab, Tab::Tweaks);
        run(&mut n, &l, &[Down, Down]);
        assert_eq!(n.focused(&l), Some(Target::Body(1)));
        assert_eq!(n.apply(Activate, &l), Effect::Body(1));
        run(&mut n, &l, &[Up, Up]);
        assert_eq!(n.zone, Zone::Tabs);
        for _ in 0..10 {
            n.apply(NextTab, &l);
        }
        assert_eq!(n.tab, Tab::Logs);
        // Left from the first tab goes to the list.
        n.select_tab(Tab::Overview);
        run(&mut n, &l, &[Left]);
        assert_eq!(n.zone, Zone::Side);
    }

    #[test]
    fn bumpers_cycle_sections() {
        let l = lib_layout();
        let mut n = Nav::new();
        n.open_game(Gid::from(2));
        run(&mut n, &l, &[NextSection]);
        assert_eq!(n.page, Page::Downloads);
        assert_eq!(n.zone, Zone::Body);
        run(&mut n, &l, &[NextSection]);
        assert_eq!(n.page, Page::Activity);
        run(&mut n, &l, &[NextSection]);
        assert_eq!(n.page, Page::Library);
        assert_eq!(n.zone, Zone::Hero);
        run(&mut n, &l, &[PrevSection]);
        assert_eq!(n.page, Page::Activity);
        assert_eq!(n.apply(Back, &l), Effect::Moved);
        assert_eq!(n.page, Page::Library);
    }

    #[test]
    fn top_bar_activation_switches_sections() {
        let l = lib_layout();
        let mut n = Nav::new();
        run(&mut n, &l, &[Up, Up, Right, Right, Right]);
        assert_eq!(n.focused(&l), Some(Target::Section(Section::Activity)));
        run(&mut n, &l, &[Activate]);
        assert_eq!(n.page, Page::Activity);
        run(&mut n, &l, &[Down]);
        assert_eq!(n.focused(&l), Some(Target::Body(0)));
    }

    #[test]
    fn palette_and_menu_overlays_capture_input() {
        let l = lib_layout();
        let mut n = Nav::new();
        run(&mut n, &l, &[Right, Palette]);
        assert_eq!(n.focused(&l), Some(Target::PaletteItem(0)));
        run(&mut n, &l, &[Up]); // wraps
        assert_eq!(n.focused(&l), Some(Target::PaletteItem(5)));
        assert_eq!(n.apply(Activate, &l), Effect::Palette(5));
        assert!(n.overlay.is_none());
        // Focus is where it was.
        assert_eq!(n.focused(&l), Some(Target::Tile(Gid::from(2))));
        // Y opens the menu for the focused tile; B closes it.
        assert_eq!(n.apply(Menu, &l), Effect::Moved);
        assert_eq!(n.overlay, Some(Overlay::Menu { game: Gid::from(2), selected: 0 }));
        run(&mut n, &l, &[Down]);
        assert_eq!(n.apply(Activate, &l), Effect::Menu(Gid::from(2), 1));
        run(&mut n, &l, &[Menu, Back]);
        assert!(n.overlay.is_none());
        // Start toggles the palette.
        run(&mut n, &l, &[Palette, Palette]);
        assert!(n.overlay.is_none());
    }

    #[test]
    fn settings_side_list_and_body() {
        let l = Layout { body: 3, ..lib_layout() };
        let mut n = Nav::new();
        n.open_settings(SettingsSection::Accounts);
        assert_eq!(n.focused(&l), Some(Target::SettingsNav(SettingsSection::Accounts)));
        run(&mut n, &l, &[Down, Down]);
        assert_eq!(n.settings, SettingsSection::Compatibility);
        // Right (or A) enters the section's controls; Left and B come back.
        run(&mut n, &l, &[Right, Down]);
        assert_eq!(n.focused(&l), Some(Target::Body(1)));
        assert_eq!(n.apply(Activate, &l), Effect::Body(1));
        run(&mut n, &l, &[Left]);
        assert_eq!(n.zone, Zone::Side);
        run(&mut n, &l, &[Activate]);
        assert_eq!(n.focused(&l), Some(Target::Body(0)));
        assert_eq!(n.apply(Back, &l), Effect::Moved);
        assert_eq!(n.zone, Zone::Side);
        // LT / RT switch sections; the first section's Up reaches the top bar.
        run(&mut n, &l, &[NextTab]);
        assert_eq!(n.settings, SettingsSection::Performance);
        run(&mut n, &l, &[PrevTab, PrevTab, PrevTab, Up]);
        assert_eq!(n.zone, Zone::TopBar);
        run(&mut n, &l, &[Down]);
        assert_eq!(n.focused(&l), Some(Target::SettingsNav(SettingsSection::Accounts)));
        // No controls in a section: A stays on the list. B leaves Settings.
        let empty = Layout { body: 0, ..lib_layout() };
        assert_eq!(n.apply(Activate, &empty), Effect::None);
        assert_eq!(n.apply(Back, &empty), Effect::Moved);
        assert_eq!(n.page, Page::Library);
        // Clicking a section focuses it.
        n.focus(&l, Target::SettingsNav(SettingsSection::About));
        assert_eq!(n.page, Page::Settings);
        assert_eq!(n.settings, SettingsSection::About);
    }

    #[test]
    fn play_targets_focused_game() {
        let l = lib_layout();
        let mut n = Nav::new();
        run(&mut n, &l, &[Right]);
        assert_eq!(n.apply(Play, &l), Effect::Play(Gid::from(2)));
        n.open_game(Gid::from(7));
        assert_eq!(n.apply(Play, &l), Effect::Play(Gid::from(7)));
        n.open_section(Section::Downloads);
        assert_eq!(n.apply(Play, &l), Effect::None);
        assert_eq!(n.apply(Menu, &l), Effect::None);
    }

    #[test]
    fn layout_shrinking_clamps_focus() {
        let l = lib_layout();
        let mut n = Nav::new();
        run(&mut n, &l, &[Down, Down, Right]);
        let small = Layout { grid: vec![Gid::from(1), Gid::from(2), Gid::from(3)], ..lib_layout() };
        assert_eq!(n.focused(&small), Some(Target::Tile(Gid::from(3))));
        n.apply(Left, &small);
        assert_eq!(n.focused(&small), Some(Target::Tile(Gid::from(2))));
        let empty = Layout { grid: vec![], hero_buttons: 0, ..lib_layout() };
        assert_eq!(n.focused(&empty), None);
        assert_eq!(n.apply(Activate, &empty), Effect::None);
    }
}
use crate::model::Gid;
