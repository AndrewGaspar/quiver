//! The system producer (`gpu.*`, `cpu.*`, `mem.*`, `bat.*`, `proc.*`), polled at 1 Hz only
//! while a session runs.
//!
//! GPUs: NVIDIA through NVML, AMD (and Intel, partly) through sysfs/hwmon. The laptop this
//! was built on has both (an RTX 5070 dGPU and a Radeon 890M iGPU); the metrics follow the
//! GPU the game renders on: the vendor in the Sight layer's status block when there is one,
//! else the NVIDIA GPU if it is already awake, else the integrated one. NVML is never used
//! to wake a runtime-suspended dGPU (SIGHT S9): without the layer saying the game is on it,
//! quiverd only reads NVML while the dGPU's `power/runtime_status` is `active`, and opens
//! and closes it around each reading so it doesn't keep the GPU awake itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::nvml::Nvml;

pub const VENDOR_NVIDIA: u32 = 0x10de;
pub const VENDOR_AMD: u32 = 0x1002;
pub const VENDOR_INTEL: u32 = 0x8086;

/// Where sysfs and procfs are (tests use a fake tree).
#[derive(Clone, Debug)]
pub struct Roots {
    pub sys: PathBuf,
    pub proc_: PathBuf,
}

impl Default for Roots {
    fn default() -> Self {
        Roots { sys: "/sys".into(), proc_: "/proc".into() }
    }
}

/// One reading. Values are in the descriptors' units.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Reading {
    pub nums: Vec<(&'static str, f64)>,
    pub texts: Vec<(&'static str, String)>,
    pub states: Vec<(&'static str, &'static str)>,
}

impl Reading {
    fn num(&mut self, id: &'static str, v: Option<f64>) {
        if let Some(v) = v.filter(|v| v.is_finite()) {
            self.nums.push((id, v));
        }
    }

    pub fn get(&self, id: &str) -> Option<f64> {
        self.nums.iter().find(|(k, _)| *k == id).map(|(_, v)| *v)
    }
}

/// What the sessions need sampled.
#[derive(Debug, Default, Clone)]
pub struct Want {
    /// Game processes (Linux pids) for `proc.*`.
    pub pids: Vec<u32>,
    /// The PCI vendor the game renders on (from the layer), if known.
    pub vendor: Option<u32>,
}

#[derive(Debug, Clone)]
struct Gpu {
    vendor: u32,
    /// The PCI device dir (`/sys/devices/pci…/0000:c1:00.0`).
    dev: PathBuf,
    slot: String,
    /// Integrated (AMD APU): `class` 0x038000 or no `boot_vga`… approximated by vendor.
    integrated: bool,
}

pub struct SystemProducer {
    roots: Roots,
    gpus: Vec<Gpu>,
    nvml: Option<Nvml>,
    nvml_tried: bool,
    /// NVML stays initialized for the session (the layer said the game is on NVIDIA).
    nvml_held: bool,
    ticks: u64,
    prev_cpu: Option<(u64, u64)>,
    prev_proc: HashMap<u32, (u64, Instant)>,
    clk_tck: f64,
    last_nvidia: Option<Reading>,
}

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned())
}

fn read_f(p: &Path) -> Option<f64> {
    read(p)?.parse().ok()
}

fn read_hex(p: &Path) -> Option<u32> {
    u32::from_str_radix(read(p)?.trim_start_matches("0x"), 16).ok()
}

impl SystemProducer {
    pub fn new(roots: Roots) -> SystemProducer {
        // SAFETY: sysconf has no preconditions.
        let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        let mut s = SystemProducer {
            roots,
            gpus: Vec::new(),
            nvml: None,
            nvml_tried: false,
            nvml_held: false,
            ticks: 0,
            prev_cpu: None,
            prev_proc: HashMap::new(),
            clk_tck: if tck > 0 { tck as f64 } else { 100.0 },
            last_nvidia: None,
        };
        s.gpus = s.find_gpus();
        s
    }

    fn find_gpus(&self) -> Vec<Gpu> {
        let mut out: Vec<Gpu> = Vec::new();
        let Ok(rd) = std::fs::read_dir(self.roots.sys.join("class/drm")) else { return out };
        let mut names: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        for n in names {
            if !n.starts_with("card") || n.contains('-') {
                continue;
            }
            let link = self.roots.sys.join("class/drm").join(&n).join("device");
            let dev = std::fs::canonicalize(&link).unwrap_or(link);
            let (Some(vendor), Some(class)) = (read_hex(&dev.join("vendor")), read_hex(&dev.join("class"))) else { continue };
            if class >> 16 != 0x03 || out.iter().any(|g| g.dev == dev) {
                continue;
            }
            let slot = dev.file_name().map(|f| f.to_string_lossy().trim_start_matches("0000:").to_owned()).unwrap_or_default();
            out.push(Gpu { vendor, integrated: vendor != VENDOR_NVIDIA && class == 0x038000 || vendor == VENDOR_INTEL, dev, slot });
        }
        out
    }

    fn runtime_active(g: &Gpu) -> bool {
        // No runtime PM (a desktop): treat as awake.
        match read(&g.dev.join("power/runtime_status")).as_deref() {
            Some("suspended") | Some("suspending") => false,
            _ => true,
        }
    }

    /// The GPU the metrics follow.
    fn pick(&self, want: &Want) -> Option<&Gpu> {
        if let Some(v) = want.vendor
            && let Some(g) = self.gpus.iter().find(|g| g.vendor == v)
        {
            return Some(g);
        }
        if let Some(g) = self.gpus.iter().find(|g| g.vendor == VENDOR_NVIDIA && Self::runtime_active(g)) {
            return Some(g);
        }
        self.gpus.iter().find(|g| g.integrated).or_else(|| self.gpus.iter().find(|g| g.vendor != VENDOR_NVIDIA))
    }

    /// Releases NVML (no session wants it any more).
    pub fn idle(&mut self) {
        self.nvml_held = false;
        if let Some(n) = &mut self.nvml {
            n.shutdown();
        }
        self.prev_cpu = None;
        self.prev_proc.clear();
    }

    pub fn sample(&mut self, want: &Want) -> Reading {
        self.ticks += 1;
        let mut r = Reading::default();
        self.cpu(&mut r);
        self.memory(&mut r);
        self.battery(&mut r);
        self.process(&mut r, &want.pids);
        match self.pick(want).cloned() {
            Some(g) if g.vendor == VENDOR_NVIDIA => self.nvidia(&mut r, &g, want.vendor == Some(VENDOR_NVIDIA)),
            Some(g) => self.sysfs_gpu(&mut r, &g),
            None => {}
        }
        r
    }

    fn nvidia(&mut self, r: &mut Reading, g: &Gpu, confirmed: bool) {
        if !self.nvml_tried {
            self.nvml_tried = true;
            self.nvml = Nvml::load();
            if self.nvml.is_none() {
                tracing::info!("perf: NVML not available; NVIDIA GPU metrics off");
            }
        }
        let Some(nvml) = self.nvml.as_mut() else { return };
        if confirmed {
            self.nvml_held = true;
        } else if self.nvml_held {
            // The layer no longer says NVIDIA (another swapchain, or the layer went away).
            self.nvml_held = false;
            nvml.shutdown();
        }
        if !self.nvml_held {
            // Unconfirmed: only while something else keeps it awake, every 5 s, open/close.
            if !Self::runtime_active(g) {
                self.last_nvidia = None;
                return;
            }
            if !self.ticks.is_multiple_of(5) {
                if let Some(last) = &self.last_nvidia {
                    r.nums.extend(last.nums.iter().cloned());
                    r.texts.extend(last.texts.iter().cloned());
                }
                return;
            }
        }
        if !nvml.init() {
            return;
        }
        let s = nvml.sample(0);
        if !self.nvml_held {
            nvml.shutdown();
        }
        let Some(s) = s else { return };
        let mut g_r = Reading::default();
        g_r.num("gpu.load", s.load);
        g_r.num("gpu.temp", s.temp);
        g_r.num("gpu.power", s.power_w);
        g_r.num("gpu.clock", s.clock_mhz);
        g_r.num("gpu.vram.used", s.vram_used);
        g_r.num("gpu.vram.total", s.vram_total);
        if let Some(n) = s.name {
            g_r.texts.push(("gpu.name", n));
        }
        r.nums.extend(g_r.nums.iter().cloned());
        r.texts.extend(g_r.texts.iter().cloned());
        self.last_nvidia = Some(g_r);
    }

    fn hwmon_dirs(&self, under: &Path) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(under.join("hwmon")).into_iter().flatten().filter_map(|e| e.ok()).map(|e| e.path()).collect();
        v.sort();
        v
    }

    fn sysfs_gpu(&mut self, r: &mut Reading, g: &Gpu) {
        let d = &g.dev;
        r.num("gpu.load", read_f(&d.join("gpu_busy_percent")));
        r.num("gpu.vram.used", read_f(&d.join("mem_info_vram_used")));
        r.num("gpu.vram.total", read_f(&d.join("mem_info_vram_total")));
        if let Some(h) = self.hwmon_dirs(d).into_iter().next() {
            r.num("gpu.temp", read_f(&h.join("temp1_input")).map(|m| m / 1000.0));
            let p = read_f(&h.join("power1_average")).or_else(|| read_f(&h.join("power1_input")));
            r.num("gpu.power", p.map(|uw| uw / 1e6));
            r.num("gpu.clock", read_f(&h.join("freq1_input")).map(|hz| hz / 1e6));
        }
        if r.get("gpu.clock").is_none()
            && let Some(sclk) = read(&d.join("pp_dpm_sclk"))
        {
            let mhz = sclk.lines().find(|l| l.ends_with('*')).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.to_lowercase().trim_end_matches("mhz").parse().ok());
            r.num("gpu.clock", mhz);
        }
        let vendor = match g.vendor {
            VENDOR_AMD => "AMD Radeon",
            VENDOR_INTEL => "Intel GPU",
            _ => "GPU",
        };
        r.texts.push(("gpu.name", format!("{vendor} ({})", g.slot)));
    }

    fn cpu(&mut self, r: &mut Reading) {
        if let Some(stat) = read(&self.roots.proc_.join("stat"))
            && let Some(line) = stat.lines().find(|l| l.starts_with("cpu "))
        {
            let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
            if v.len() >= 4 {
                let idle = v[3] + v.get(4).copied().unwrap_or(0);
                let total: u64 = v.iter().take(8).sum();
                if let Some((pi, pt)) = self.prev_cpu
                    && total > pt
                {
                    let busy = 1.0 - (idle.saturating_sub(pi)) as f64 / (total - pt) as f64;
                    r.num("cpu.load", Some((busy * 100.0).clamp(0.0, 100.0)));
                }
                self.prev_cpu = Some((idle, total));
            }
        }
        r.num("cpu.temp", self.cpu_temp());
        // Mean scaling_cur_freq (kHz).
        let cpus = self.roots.sys.join("devices/system/cpu");
        let mut sum = 0.0;
        let mut n = 0;
        for e in std::fs::read_dir(&cpus).into_iter().flatten().filter_map(|e| e.ok()) {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("cpu") && name[3..].bytes().all(|b| b.is_ascii_digit()) && name.len() > 3
                && let Some(khz) = read_f(&e.path().join("cpufreq/scaling_cur_freq"))
            {
                sum += khz;
                n += 1;
            }
        }
        if n > 0 {
            r.num("cpu.clock", Some(sum / n as f64 / 1000.0));
        }
    }

    fn cpu_temp(&self) -> Option<f64> {
        let hw = self.roots.sys.join("class/hwmon");
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&hw).into_iter().flatten().filter_map(|e| e.ok()).map(|e| e.path()).collect();
        dirs.sort();
        for (driver, labels) in [("k10temp", &["Tctl", "Tdie"][..]), ("zenpower", &["Tdie", "Tctl"][..]), ("coretemp", &["Package id 0"][..])] {
            for d in &dirs {
                if read(&d.join("name")).as_deref() != Some(driver) {
                    continue;
                }
                for i in 1..=16 {
                    let label = read(&d.join(format!("temp{i}_label")));
                    if label.as_deref().is_some_and(|l| labels.contains(&l)) || (label.is_none() && i == 1) {
                        if let Some(m) = read_f(&d.join(format!("temp{i}_input"))) {
                            return Some(m / 1000.0);
                        }
                    }
                }
            }
        }
        None
    }

    fn memory(&self, r: &mut Reading) {
        let Some(mi) = read(&self.roots.proc_.join("meminfo")) else { return };
        let kb = |key: &str| mi.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<f64>().ok()).map(|k| k * 1024.0);
        let (total, avail) = (kb("MemTotal:"), kb("MemAvailable:"));
        r.num("mem.total", total);
        if let (Some(t), Some(a)) = (total, avail) {
            r.num("mem.used", Some(t - a));
        }
    }

    fn battery(&self, r: &mut Reading) {
        let ps = self.roots.sys.join("class/power_supply");
        let mut ac = false;
        let mut bat: Option<PathBuf> = None;
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&ps).into_iter().flatten().filter_map(|e| e.ok()).map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            match read(&p.join("type")).as_deref() {
                Some("Mains") if read(&p.join("online")).as_deref() == Some("1") => ac = true,
                Some("Battery") if bat.is_none() && read(&p.join("present")).as_deref() != Some("0") => bat = Some(p),
                _ => {}
            }
        }
        let Some(b) = bat else { return };
        r.num("bat.level", read_f(&b.join("capacity")));
        let power = read_f(&b.join("power_now"))
            .map(|uw| uw / 1e6)
            .or_else(|| Some(read_f(&b.join("current_now"))? * read_f(&b.join("voltage_now"))? / 1e12));
        r.num("bat.power", power);
        let state = match read(&b.join("status")).as_deref() {
            Some("Discharging") => "discharging",
            Some("Charging") => "charging",
            Some("Full") => "full",
            _ if ac => "ac",
            _ => "discharging",
        };
        r.states.push(("bat.status", state));
    }

    fn process(&mut self, r: &mut Reading, pids: &[u32]) {
        if pids.is_empty() {
            return;
        }
        let mut rss = 0.0;
        let mut cpu = 0.0;
        let mut any = false;
        let now = Instant::now();
        for &pid in pids {
            let dir = self.roots.proc_.join(pid.to_string());
            if let Some(st) = read(&dir.join("status"))
                && let Some(kb) = st.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<f64>().ok())
            {
                rss += kb * 1024.0;
                any = true;
            }
            // utime and stime are fields 14 and 15, after the parenthesized comm.
            if let Some(stat) = read(&dir.join("stat"))
                && let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r)
            {
                let f: Vec<&str> = rest.split_whitespace().collect();
                if let (Some(u), Some(s)) = (f.get(11).and_then(|v| v.parse::<u64>().ok()), f.get(12).and_then(|v| v.parse::<u64>().ok())) {
                    let ticks = u + s;
                    if let Some((pt, at)) = self.prev_proc.insert(pid, (ticks, now)) {
                        let dt = now.duration_since(at).as_secs_f64();
                        if dt > 0.0 {
                            cpu += ticks.saturating_sub(pt) as f64 / self.clk_tck / dt * 100.0;
                        }
                    }
                }
            }
        }
        self.prev_proc.retain(|p, _| pids.contains(p));
        if any {
            r.num("proc.rss", Some(rss));
            r.num("proc.cpu", Some(cpu));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn w(root: &Path, rel: &str, v: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, v).unwrap();
    }

    /// The machine this was built for, as a fake tree: NVIDIA dGPU (suspended), AMD iGPU,
    /// k10temp, a battery and AC.
    fn tree() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        let nv = "sys/devices/pci0000:00/0000:00:03.1/0000:c1:00.0";
        let amd = "sys/devices/pci0000:00/0000:00:08.1/0000:c2:00.0";
        w(r, &format!("{nv}/vendor"), "0x10de\n");
        w(r, &format!("{nv}/class"), "0x030000\n");
        w(r, &format!("{nv}/power/runtime_status"), "suspended\n");
        w(r, &format!("{amd}/vendor"), "0x1002\n");
        w(r, &format!("{amd}/class"), "0x038000\n");
        w(r, &format!("{amd}/gpu_busy_percent"), "42\n");
        w(r, &format!("{amd}/mem_info_vram_used"), "1073741824\n");
        w(r, &format!("{amd}/mem_info_vram_total"), "4294967296\n");
        w(r, &format!("{amd}/hwmon/hwmon5/temp1_input"), "51000\n");
        w(r, &format!("{amd}/hwmon/hwmon5/power1_average"), "12500000\n");
        w(r, &format!("{amd}/pp_dpm_sclk"), "0: 600Mhz \n1: 785Mhz *\n2: 2900Mhz \n");
        fs::create_dir_all(r.join("sys/class/drm")).unwrap();
        std::os::unix::fs::symlink(r.join(nv), r.join("sys/class/drm/card1")).unwrap();
        fs::create_dir_all(r.join("sys/class/drm/card2")).unwrap();
        std::os::unix::fs::symlink(r.join(amd), r.join("sys/class/drm/card2/device")).unwrap();
        // card1 is a link to the device itself in this fake: give it a device link too.
        std::os::unix::fs::symlink(r.join(nv), r.join(nv).join("device")).unwrap();
        w(r, "sys/class/hwmon/hwmon6/name", "k10temp\n");
        w(r, "sys/class/hwmon/hwmon6/temp1_label", "Tctl\n");
        w(r, "sys/class/hwmon/hwmon6/temp1_input", "64250\n");
        w(r, "sys/class/power_supply/ACAD/type", "Mains\n");
        w(r, "sys/class/power_supply/ACAD/online", "1\n");
        w(r, "sys/class/power_supply/BAT1/type", "Battery\n");
        w(r, "sys/class/power_supply/BAT1/capacity", "87\n");
        w(r, "sys/class/power_supply/BAT1/status", "Not charging\n");
        w(r, "sys/class/power_supply/BAT1/current_now", "1000000\n");
        w(r, "sys/class/power_supply/BAT1/voltage_now", "15000000\n");
        w(r, "sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq", "3000000\n");
        w(r, "sys/devices/system/cpu/cpu1/cpufreq/scaling_cur_freq", "2000000\n");
        w(r, "proc/stat", "cpu  100 0 100 800 0 0 0 0 0 0\n");
        w(r, "proc/meminfo", "MemTotal:       32000000 kB\nMemFree:  1 kB\nMemAvailable:   20000000 kB\n");
        w(r, "proc/4242/status", "Name:\tgame.exe\nVmRSS:\t  2048000 kB\n");
        w(r, "proc/4242/stat", "4242 (game.exe (x)) S 1 2 3 4 5 6 7 8 9 10 500 100 0 0\n");
        t
    }

    #[test]
    fn reads_the_laptop_tree_without_waking_the_dgpu() {
        let t = tree();
        let roots = Roots { sys: t.path().join("sys"), proc_: t.path().join("proc") };
        let mut p = SystemProducer::new(roots);
        assert_eq!(p.gpus.len(), 2, "{:?}", p.gpus);
        let want = Want { pids: vec![4242], vendor: None };
        let r1 = p.sample(&want);
        // The dGPU is suspended and the layer said nothing: the iGPU's numbers.
        assert_eq!(r1.get("gpu.load"), Some(42.0));
        assert_eq!(r1.get("gpu.temp"), Some(51.0));
        assert_eq!(r1.get("gpu.power"), Some(12.5));
        assert_eq!(r1.get("gpu.clock"), Some(785.0));
        assert_eq!(r1.get("gpu.vram.used"), Some(1073741824.0));
        assert!(r1.texts.iter().any(|(k, v)| *k == "gpu.name" && v.starts_with("AMD Radeon")));
        assert!(p.nvml.is_none() && !p.nvml_tried, "NVML untouched");
        assert_eq!(r1.get("cpu.temp"), Some(64.25));
        assert_eq!(r1.get("cpu.clock"), Some(2500.0));
        assert_eq!(r1.get("cpu.load"), None, "needs two readings");
        assert_eq!(r1.get("mem.used"), Some(12_000_000.0 * 1024.0));
        assert_eq!(r1.get("bat.level"), Some(87.0));
        assert_eq!(r1.get("bat.power"), Some(15.0));
        assert_eq!(r1.states, vec![("bat.status", "ac")]);
        assert_eq!(r1.get("proc.rss"), Some(2048000.0 * 1024.0));
        // Second reading: CPU load from the delta (200 busy of 400).
        w(t.path(), "proc/stat", "cpu  200 0 200 1000 0 0 0 0 0 0\n");
        let r2 = p.sample(&want);
        assert_eq!(r2.get("cpu.load"), Some(50.0));
        assert!(r2.get("proc.cpu").is_some());
    }

    #[test]
    fn missing_everything_is_fine() {
        let t = tempfile::tempdir().unwrap();
        let mut p = SystemProducer::new(Roots { sys: t.path().join("sys"), proc_: t.path().join("proc") });
        let r = p.sample(&Want::default());
        assert!(r.nums.is_empty() && r.texts.is_empty() && r.states.is_empty());
    }
}
