//! Where a game's own data lives (design/STORES.md §2.2): `<data>/games/<dir>/` (settings
//! history, play time, save stores, profiles, layers, options, verify reports) and
//! `<data>/mods/<dir>/` (the built-in catalog's files), with `<dir>` from
//! [`GameRef::dir_name`]: `3539` for a Ubisoft product, exactly as before, and
//! `steam-620` for other stores.

use std::path::{Path, PathBuf};

use quiver_api::GameRef;

use crate::launcher::data_dir;

/// `<data>/games/<dir>`, under Quiver's data directory.
pub fn game_dir(game: &GameRef) -> PathBuf {
    game_dir_in(&data_dir(), game)
}

/// `<data>/games/<dir>`, under `data`.
pub fn game_dir_in(data: &Path, game: &GameRef) -> PathBuf {
    data.join("games").join(game.dir_name())
}

/// `<data>/mods/<dir>`, under `data`.
pub fn mods_dir_in(data: &Path, game: &GameRef) -> PathBuf {
    data.join("mods").join(game.dir_name())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ubisoft_folders_are_unchanged() {
        let data = Path::new("/d");
        assert_eq!(game_dir_in(data, &GameRef::ubisoft(3539)), Path::new("/d/games/3539"));
        assert_eq!(mods_dir_in(data, &GameRef::ubisoft(420)), Path::new("/d/mods/420"));
        assert_eq!(game_dir_in(data, &GameRef::steam(620)), Path::new("/d/games/steam-620"));
        assert_eq!(mods_dir_in(data, &GameRef::steam(292030)), Path::new("/d/mods/steam-292030"));
    }
}
