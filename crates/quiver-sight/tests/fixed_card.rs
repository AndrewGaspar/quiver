//! The HUD card follows the model's size and values never move it: every preset's demo model
//! in each content variant (long values; cells and rows gone, in a card quiverd fitted to
//! them) goes through the game presenter's real path (`GameHud`: off-screen GPUI render →
//! `hud.image`); the published images match the model's card, and base and long values give
//! the same image and graph rectangles (the vanilla variant the same graph). GPUI runs on the main thread, hence `harness = false`. Without a
//! GPU adapter the test says so and passes.

use std::path::PathBuf;
use std::time::Duration;

use gpui::App;
use quiver_api::perf::Presenter;
use quiver_perf::hud::HudPoll;
use quiver_perf::session::{create_hud_file, open_hud_reader};
use quiver_sight::demo::{VARIANTS, sample_model, vary};
use quiver_sight::game::{GameHud, fit_factor};
use quiver_ui::theme;

fn main() {
    let dir = tempfile::tempdir().unwrap();
    let image: PathBuf = dir.path().join("hud.image");
    create_hud_file(&image, quiver_perf::hud::buffer_bytes_for(1024, 1000).unwrap()).unwrap();
    let extent = [2560u32, 1600u32];
    let result = std::rc::Rc::new(std::cell::RefCell::new(Err::<(), String>("did not run".into())));
    let r = result.clone();
    gpui_platform::headless().with_assets(quiver_ui::assets::Assets).run(move |cx: &mut App| {
        quiver_ui::init(cx, theme::load(&theme::omarchy_state_dir()));
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_millis(20)).await;
            cx.update(|cx| {
                *r.borrow_mut() = check(&image, extent, cx);
                cx.quit();
            });
        })
        .detach();
    });
    match &*result.borrow() {
        Ok(()) => println!("fixed_card: ok"),
        Err(e) if e.starts_with("skip:") => println!("fixed_card: {e}"),
        Err(e) => {
            eprintln!("fixed_card: FAILED: {e}");
            std::process::exit(1);
        }
    }
}

/// An image's size and its graph rectangles.
type Bounds = (u32, u32, Vec<(i32, i32, i32, i32)>);

fn check(image: &std::path::Path, extent: [u32; 2], cx: &mut App) -> Result<(), String> {
    let mut reader = open_hud_reader(image).map_err(|e| e.to_string())?;
    let mut dst = vec![0u8; reader.info().buffer_bytes as usize];
    let mut rev = 0;
    for layout in quiver_sight::demo::LAYOUTS {
        let mut hud = GameHud::default();
        let mut first: Option<Bounds> = None;
        // Through the variants and back (base, long, short, short, long, base), on one view
        // that remembers the widest values.
        for variant in VARIANTS.iter().chain(VARIANTS.iter().rev()) {
            rev += 1;
            let mut m = vary(sample_model(layout, rev as u32, Presenter::Game, Some(extent)), variant);
            m.image = Some(image.to_path_buf());
            m.rev = rev;
            if let Err(e) = hud.update(&m, cx) {
                return Err(if rev == 1 { format!("skip: no off-screen rendering here ({e:#})") } else { format!("{layout}/{variant}: {e:#}") });
            }
            let HudPoll::Image { desc, .. } = reader.poll((extent[0], extent[1]), &mut dst) else {
                return Err(format!("{layout}/{variant}: no image"));
            };
            let f = fit_factor(&m, reader.info().buffer_bytes);
            let want = ((m.size[0] * f).ceil() as u32 + 1, (m.size[1] * f).ceil() as u32 + 1);
            if (desc.width, desc.height) != want {
                return Err(format!("{layout}/{variant}: image {}x{}, the model's card is {want:?}", desc.width, desc.height));
            }
            let got = (desc.width, desc.height, desc.graphs().iter().map(|g| (g.x, g.y, g.w, g.h)).collect::<Vec<_>>());
            // `short` is the vanilla game's model, whose card quiverd fits to its fewer rows (a
            // session of its own): only its graph must stay where it was.
            match &first {
                None => first = Some(got),
                Some(f) if *variant == "short" && f.2 != got.2 => return Err(format!("{layout}/{variant}: graphs {:?}, first {:?}", got.2, f.2)),
                Some(f) if *variant != "short" && *f != got => return Err(format!("{layout}/{variant}: {got:?}, first {f:?}")),
                Some(_) => {}
            }
        }
        hud.close(cx);
    }
    Ok(())
}
