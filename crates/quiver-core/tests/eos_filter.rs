//! The lab's EOS filter (`contrib/lab/eos-filter`, design/LAB.md section 3.1).
//!
//! - `parser_and_allowlist`: `test_filter.c` (the request parser and the allowlist: split
//!   writes, keep-alive and pipelining, chunked bodies, other hosts, unknown paths), built
//!   natively with `cc` and run, under ASan and UBSan when the compiler has them.
//! - `dlls_build`: `build.sh` makes both DLLs (needs `x86_64-w64-mingw32-gcc` and
//!   `i686-w64-mingw32-gcc`, as the bridge does), each carrying the marker Quiver checks and
//!   all 80 of Wine's secur32 exports.
//! - `filter_under_wine` (opt-in): `test/client.c`, a schannel HTTPS client, against
//!   `test/server.py`, a TLS server on 127.0.0.1 (never Epic), with the filter as the
//!   client's secur32.dll (`secur32=n,b`), for both architectures, and once without the
//!   filter to show the server would have seen what the filter blocks:
//!
//! ```sh
//! QUIVER_TEST_PROTON=~/.local/share/Steam/compatibilitytools.d/GE-Proton11-12-x86_64 \
//!   cargo test -p quiver-core --test eos_filter -- --ignored --nocapture
//! ```
//!
//! Headless (no display), in a scratch prefix kept in `target/quiver-eos-filter-wine`.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

fn src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contrib/lab/eos-filter")
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{cmd:?} failed:\n{text}");
    text
}

#[test]
fn parser_and_allowlist() {
    let t = tempfile::tempdir().unwrap();
    let exe = t.path().join("test_filter");
    let base = ["-std=c11", "-Wall", "-Wextra", "-Werror", "-g", "-o"];
    let build = |sanitize: bool| {
        let mut c = Command::new("cc");
        if sanitize {
            c.arg("-fsanitize=address,undefined").arg("-fno-sanitize-recover=all");
        }
        c.args(base).arg(&exe).arg(src().join("test_filter.c")).arg(src().join("filter.c"));
        c.output().map(|o| o.status.success()).unwrap_or(false)
    };
    assert!(build(true) || build(false), "cc couldn't build test_filter.c");
    let out = run(&mut Command::new(&exe));
    println!("{out}");
    assert!(out.contains(", 0 failed"), "{out}");
}

fn build_dlls(out: &Path) {
    run(Command::new("bash").arg(src().join("build.sh")).arg(out));
}

#[test]
fn dlls_build() {
    let t = tempfile::tempdir().unwrap();
    build_dlls(t.path());
    for arch in ["x86_64", "i686"] {
        let dll = t.path().join(arch).join("secur32.dll");
        let bytes = std::fs::read(&dll).unwrap();
        assert!(quiver_core::lab::eosfilter::is_filter(&bytes), "{arch}: no marker");
        let marker = format!("QUIVER-EOS-FILTER/1 {arch}");
        assert!(bytes.windows(marker.len()).any(|w| w == marker.as_bytes()), "{arch}: wrong architecture's marker");
        let pe = quiver_core::lab::facts::parse_pe(&bytes).unwrap();
        assert_eq!(pe.arch, if arch == "x86_64" { "x64" } else { "x86" });
        // Every export Wine's secur32 has (exports.list), by name.
        let names = std::fs::read_to_string(src().join("exports.list")).unwrap();
        for n in names.lines().filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let z = format!("{n}\0");
            assert!(bytes.windows(z.len()).any(|w| w == z.as_bytes()), "{arch}: export {n} missing");
        }
    }
}

// --- the opt-in Wine test ---

fn win(p: &Path) -> String {
    format!("Z:{}", p.display()).replace('/', "\\")
}

struct Server {
    child: Child,
    port: u16,
    logs: PathBuf,
}

impl Server {
    fn start(dir: &Path) -> Server {
        let logs = dir.join("server");
        std::fs::create_dir_all(&logs).unwrap();
        let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
        run(Command::new("openssl")
            .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=api.epicgames.dev", "-keyout"])
            .arg(&key)
            .arg("-out")
            .arg(&cert));
        let mut child = Command::new("python3")
            .arg("-I")
            .arg(src().join("test/server.py"))
            .args([&cert, &key, &logs])
            .stdout(Stdio::piped())
            .spawn()
            .expect("python3");
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
        let port = line.trim().parse().unwrap_or_else(|_| panic!("server said {line:?}"));
        Server { child, port, logs }
    }

    fn read(&self, name: &str) -> String {
        String::from_utf8_lossy(&std::fs::read(self.logs.join(name)).unwrap_or_default()).into_owned()
    }

    fn clear(&self) {
        let _ = std::fs::remove_file(self.logs.join("raw.log"));
        let _ = std::fs::remove_file(self.logs.join("requests.log"));
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What the client does, one connection after another (test/client.c's scenario format).
fn scenario(port: u16) -> (String, Vec<&'static str>) {
    let api = format!("connect {port} api.epicgames.dev");
    let lines = [
        api.clone(),
        r"write POST /epic/oauth/v1/token HTTP/1.1\r\nHost: api.epicgames.dev\r\nContent-Length: 2\r\n\r\nhi".into(),
        "read".into(),
        // A head split across writes, on the same keep-alive connection.
        r"write GET /sdk/v1/default?platformId=WIN HTTP/1.1\r\nHost: api.epicgames.dev\r\n".into(),
        r"write \r\n".into(),
        "read".into(),
        // A cloud save upload: blocked, and the connection with it.
        r"write PUT /playerdata/v1/dep/puid/data/save1.sav HTTP/1.1\r\nHost: api.epicgames.dev\r\nContent-Length: 13\r\n\r\nSECRET-SAVE-1".into(),
        // An achievement, through the exported EncryptMessage rather than the table.
        format!("connect {port} api.epicgames.dev direct"),
        r"write POST /achievements/v1/dep/achievements/puid/unlock HTTP/1.1\r\nHost: api.epicgames.dev\r\nContent-Length: 12\r\n\r\nUNLOCK-ACH-1".into(),
        // A request line split across writes: can't be judged in time, so blocked.
        api.clone(),
        r"write POST /stats/v1/de".into(),
        // Another host: passed untouched, whatever the path.
        format!("connect {port} example.test"),
        r"write PUT /playerdata/v1/other/data/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: 7\r\n\r\nOTHER-1".into(),
        "read".into(),
        // Ownership, after all that, on a fresh connection.
        api,
        r"write GET /epic/ecom/v1/platforms/EPIC/identities/abc/ownership?nsCatalogItemId=ns:item HTTP/1.1\r\nHost: api.epicgames.dev\r\n\r\n".into(),
        "read".into(),
    ];
    let want = vec![
        "connect ok",
        "write ok",
        "read HTTP/1.1 200 OK body=ok /epic/oauth/v1/token",
        "write ok",
        "write ok",
        "read HTTP/1.1 200 OK body=ok /sdk/v1/default?platformId=WIN",
        "write failed 80090329 (zeroed)",
        "connect ok",
        "write failed 80090329 (zeroed)",
        "connect ok",
        "write failed 80090329 (zeroed)",
        "connect ok",
        "write ok",
        "read HTTP/1.1 200 OK body=ok /playerdata/v1/other/data/x",
        "connect ok",
        "write ok",
        "read HTTP/1.1 200 OK body=ok /epic/ecom/v1/platforms/EPIC/identities/abc/ownership?nsCatalogItemId=ns:item",
    ];
    (lines.join("\n") + "\n", want)
}

struct Wine {
    proton: PathBuf,
    prefix: PathBuf,
    home: PathBuf,
}

impl Wine {
    fn run(&self, exe: &Path, args: &[String], overrides: &str, log: &Path) -> String {
        let out = Command::new(self.proton.join("files/bin/wine"))
            .arg(exe)
            .args(args)
            .env("WINEPREFIX", &self.prefix)
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("WINEDEBUG", "-all")
            .env("WINEDLLOVERRIDES", format!("winemenubuilder.exe=d;mscoree,mshtml=;{overrides}"))
            .env("QUIVER_EOS_FILTER_LOG", win(log))
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("QUIVER_LAB_RUN_DIR")
            .output()
            .expect("wine");
        String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
    }

    fn kill(&self) {
        let _ = Command::new(self.proton.join("files/bin/wineserver")).arg("-k").env("WINEPREFIX", &self.prefix).status();
    }
}

fn result_lines(out: &str) -> Vec<&str> {
    out.lines().filter(|l| ["connect", "write", "read", "close"].iter().any(|p| l.starts_with(p))).collect()
}

#[test]
#[ignore = "needs QUIVER_TEST_PROTON, mingw-w64, openssl and python3; see the module docs"]
fn filter_under_wine() {
    let proton = PathBuf::from(std::env::var_os("QUIVER_TEST_PROTON").expect("set QUIVER_TEST_PROTON to a Proton build directory"));
    let scratch = tempfile::tempdir().unwrap();
    let t = scratch.path();
    let dlls = t.join("dlls");
    build_dlls(&dlls);
    let keep = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/quiver-eos-filter-wine");
    std::fs::create_dir_all(keep.join("home")).unwrap();
    let wine = Wine { proton, prefix: keep.join("pfx"), home: keep.join("home") };
    let server = Server::start(t);
    let (text, want) = scenario(server.port);
    let scen = t.join("scenario.txt");
    std::fs::write(&scen, text).unwrap();

    for arch in ["x86_64", "i686"] {
        // The client and the filter side by side, as the game and its secur32.dll.
        let dir = t.join(format!("game-{arch}"));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("client.exe");
        run(Command::new(format!("{arch}-w64-mingw32-gcc"))
            .args(["-std=gnu11", "-O2", "-Wall", "-Wextra", "-Werror", "-o"])
            .arg(&exe)
            .arg(src().join("test/client.c"))
            .args(["-lsecur32", "-lws2_32"]));
        std::fs::copy(dlls.join(arch).join("secur32.dll"), dir.join("secur32.dll")).unwrap();
        let log = t.join(format!("eos-filter-{arch}.log"));
        server.clear();
        let out = wine.run(&exe, &[win(&scen)], "secur32=n,b", &log);
        wine.kill();
        println!("--- {arch} client:\n{out}");
        let filter_log = std::fs::read_to_string(&log).unwrap_or_default();
        println!("--- {arch} filter log:\n{filter_log}");
        let raw = server.read("raw.log");
        let reqs = server.read("requests.log");
        println!("--- {arch} server requests:\n{reqs}");
        assert_eq!(result_lines(&out), want, "{arch}: the client's results");

        // What reached the server: the allowed requests and the other host's, nothing else.
        assert_eq!(
            reqs.lines().collect::<Vec<_>>(),
            [
                "api.epicgames.dev POST /epic/oauth/v1/token HTTP/1.1",
                "api.epicgames.dev GET /sdk/v1/default?platformId=WIN HTTP/1.1",
                "example.test PUT /playerdata/v1/other/data/x HTTP/1.1",
                "api.epicgames.dev GET /epic/ecom/v1/platforms/EPIC/identities/abc/ownership?nsCatalogItemId=ns:item HTTP/1.1",
            ],
            "{arch}"
        );
        // Not a byte of a blocked request, not even its start.
        for never in ["SECRET-SAVE", "/playerdata/v1/dep", "UNLOCK-ACH", "achievements", "/stats/"] {
            assert!(!raw.contains(never), "{arch}: {never:?} reached the server:\n{raw}");
        }
        assert!(raw.contains("OTHER-1"));

        // The log: each decision, the method and path, never a body.
        assert!(filter_log.contains(&format!("--- EOS filter ({arch}) loaded")), "{filter_log}");
        for line in [
            "allow POST /epic/oauth/v1/token -- Epic account sign-in (token)",
            "allow GET /sdk/v1/default?platformId=\u{2026} -- SDK config (default)",
            "block PUT /playerdata/v1/dep/puid/data/save1.sav -- not on the allowlist",
            "block POST /achievements/v1/dep/achievements/puid/unlock -- not on the allowlist",
            "block POST (incomplete) -- a request line split across writes",
            "allow GET /epic/ecom/v1/platforms/EPIC/identities/abc/ownership?nsCatalogItemId=\u{2026} -- ownership check",
        ] {
            assert!(filter_log.contains(line), "{arch}: no {line:?} in the filter log:\n{filter_log}");
        }
        for never in ["example.test", "SECRET", "UNLOCK", "Content-Length", "ns:item", "hi\n"] {
            assert!(!filter_log.contains(never), "{arch}: {never:?} in the filter log");
        }
    }

    // Without the filter (Wine's own secur32), the same scenario puts everything on the wire:
    // the checks above would have seen it.
    let dir = t.join("game-x86_64");
    std::fs::remove_file(dir.join("secur32.dll")).unwrap();
    server.clear();
    let out = wine.run(&dir.join("client.exe"), &[win(&scen)], "secur32=b", &t.join("unused.log"));
    wine.kill();
    println!("--- without the filter:\n{out}");
    let raw = server.read("raw.log");
    assert!(raw.contains("SECRET-SAVE-1") && raw.contains("UNLOCK-ACH-1"), "{raw}");
    assert!(!t.join("unused.log").exists());
}
