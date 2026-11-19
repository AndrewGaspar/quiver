//! Shared by the D3D12-under-Proton tests: the test program and a screen grab of the
//! private Xvfb's window.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn build_exe(out: &Path) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wine/d3d12_present.c");
    let exe = out.join("d3d12_present.exe");
    let st = Command::new("x86_64-w64-mingw32-gcc")
        .args(["-O2", "-o"])
        .arg(&exe)
        .arg(&src)
        .args(["-ld3d12", "-ldxgi", "-luuid", "-ldxguid"])
        .status()
        .expect("x86_64-w64-mingw32-gcc");
    assert!(st.success());
    exe
}

pub fn win(p: &Path) -> String {
    format!("Z:{}", p.display()).replace('/', "\\")
}

/// The X server's screen as RGB rows (`magick import`, PPM).
pub struct Screen {
    pub w: usize,
    pub h: usize,
    pub rgb: Vec<u8>,
}

impl Screen {
    /// The window named `window` (its client area: the test's window is undecorated).
    pub fn grab(display: &str, window: &str) -> Option<Screen> {
        // `timeout`: import waits for a click when the window is gone.
        let out = Command::new("timeout")
            .args(["5", "magick", "import", "-silent", "-window", window, "-depth", "8", "ppm:-"])
            .env("DISPLAY", display)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        // P6\n<w> <h>\n<max>\n<data>
        let d = out.stdout;
        let mut fields = Vec::new();
        let mut i = 0;
        while fields.len() < 4 && i < d.len() {
            while i < d.len() && d[i].is_ascii_whitespace() {
                i += 1;
            }
            let s = i;
            while i < d.len() && !d[i].is_ascii_whitespace() {
                i += 1;
            }
            fields.push(String::from_utf8_lossy(&d[s..i]).into_owned());
        }
        i += 1;
        let (w, h): (usize, usize) = (fields.get(1)?.parse().ok()?, fields.get(2)?.parse().ok()?);
        (fields[0] == "P6" && fields[3] == "255" && d.len() >= i + w * h * 3).then(|| Screen { w, h, rgb: d[i..i + w * h * 3].to_vec() })
    }

    pub fn px(&self, x: usize, y: usize) -> [u8; 3] {
        let o = (y.min(self.h - 1) * self.w + x.min(self.w - 1)) * 3;
        [self.rgb[o], self.rgb[o + 1], self.rgb[o + 2]]
    }

    pub fn save(&self, path: &Path) {
        let mut ppm = format!("P6\n{} {}\n255\n", self.w, self.h).into_bytes();
        ppm.extend_from_slice(&self.rgb);
        let _ = std::fs::write(path, ppm);
    }
}

