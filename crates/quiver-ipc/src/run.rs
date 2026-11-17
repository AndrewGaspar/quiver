//! The bridge's launcher mode: `quiver-bridge.exe --run <context.json> [--log <file>] -- <game.exe> [args…]`.
//!
//! In this mode the bridge owns the SDK pipes and then starts the game itself as its child,
//! inside the same Wine session, with `UPLAY_ARGUMENTS` set the way Connect sets it. Used by
//! `quiver steam-run`, which swaps the game executable in Steam's Proton command for the
//! bridge, so Steam's own Proton, container and prefix run both and nothing in the prefix
//! has to change. Pure helpers live here so they are tested on the host.

use base64::Engine;

/// Parsed bridge command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeArgs {
    /// Serve the pipes until killed (`quiver-bridge.exe [context.json]`); the context may also
    /// come from `UBI_BRIDGE_CONTEXT`.
    Serve { context: Option<String> },
    /// Serve the pipes, start `exe` with `args`, exit with its exit code once it and every
    /// SDK connection are gone.
    Run { context: String, log: Option<String>, exe: String, args: Vec<String> },
}

impl BridgeArgs {
    /// Parse the arguments after the program name.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        if args.first().map(String::as_str) != Some("--run") {
            return Ok(Self::Serve { context: args.first().cloned() });
        }
        let mut it = args[1..].iter();
        let context = it.next().filter(|c| !c.starts_with("--")).ok_or("--run needs a context file")?.clone();
        let mut log = None;
        loop {
            match it.next().map(String::as_str) {
                Some("--log") => log = Some(it.next().ok_or("--log needs a file")?.clone()),
                Some("--") => break,
                Some(other) => return Err(format!("unexpected bridge argument '{other}' (missing --?)")),
                None => return Err("--run needs `-- <game.exe> [args…]`".into()),
            }
        }
        let exe = it.next().ok_or("--run needs a game executable after --")?.clone();
        Ok(Self::Run { context, log, exe, args: it.cloned().collect() })
    }

    /// The command line `quiver steam-run` puts in place of the game executable.
    pub fn run_command(context: &str, log: Option<&str>, exe: &str, args: &[String]) -> Vec<String> {
        let mut out = vec!["--run".to_owned(), context.to_owned()];
        if let Some(l) = log {
            out.extend(["--log".to_owned(), l.to_owned()]);
        }
        out.push("--".into());
        out.push(exe.to_owned());
        out.extend(args.iter().cloned());
        out
    }
}

/// `UPLAY_ARGUMENTS` as Connect sets it for a game it starts. The SDK reads the uplay id
/// from it. Without it the aux DLL relaunches the game through `UbisoftGameLauncher.exe`,
/// and the R1/R2 API DLLs start Connect (`UbisoftGameLauncher.exe … -upc_game_starter`)
/// when no pipe answers; with it they fail instead of starting Connect.
pub fn uplay_arguments(uplay_id: u32, exe: &str, workdir: &str) -> String {
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
    format!(
        "-upc_uplay_id {uplay_id} -upc_game_version 1 -upc_exe_path {} -upc_working_directory {}",
        b64(exe),
        b64(workdir)
    )
}

/// The directory part of a Windows path (`C:\a\b.exe` -> `C:\a`), or the path itself.
pub fn win_parent(path: &str) -> &str {
    match path.rfind(['\\', '/']) {
        Some(0) | None => path,
        Some(i) if path[..i].ends_with(':') => &path[..=i],
        Some(i) => &path[..i],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parse_serve() {
        assert_eq!(BridgeArgs::parse(&[]).unwrap(), BridgeArgs::Serve { context: None });
        assert_eq!(
            BridgeArgs::parse(&s(&[r"Z:\out\context.json"])).unwrap(),
            BridgeArgs::Serve { context: Some(r"Z:\out\context.json".into()) }
        );
    }

    #[test]
    fn parse_run_round_trip() {
        let game = s(&["-uplay_steam_mode", "--", "x y"]);
        let cmd = BridgeArgs::run_command(r"Z:\c.json", Some(r"Z:\b.log"), r"Z:\g\ACBlackFlag.exe", &game);
        assert_eq!(
            BridgeArgs::parse(&cmd).unwrap(),
            BridgeArgs::Run {
                context: r"Z:\c.json".into(),
                log: Some(r"Z:\b.log".into()),
                exe: r"Z:\g\ACBlackFlag.exe".into(),
                args: game,
            }
        );
        let cmd = BridgeArgs::run_command("c.json", None, "g.exe", &[]);
        assert_eq!(cmd, s(&["--run", "c.json", "--", "g.exe"]));
        assert!(matches!(BridgeArgs::parse(&cmd).unwrap(), BridgeArgs::Run { log: None, .. }));
    }

    #[test]
    fn parse_run_errors() {
        assert!(BridgeArgs::parse(&s(&["--run"])).is_err());
        assert!(BridgeArgs::parse(&s(&["--run", "--log", "x"])).is_err());
        assert!(BridgeArgs::parse(&s(&["--run", "c.json", "g.exe"])).is_err());
        assert!(BridgeArgs::parse(&s(&["--run", "c.json", "--"])).is_err());
        assert!(BridgeArgs::parse(&s(&["--run", "c.json", "--log"])).is_err());
    }

    #[test]
    fn uplay_args() {
        let exe = r"Z:\home\u\Steam\steamapps\common\AC\ACBlackFlag.exe";
        let a = uplay_arguments(66088, exe, win_parent(exe));
        let parts: Vec<&str> = a.split(' ').collect();
        assert_eq!(&parts[..5], ["-upc_uplay_id", "66088", "-upc_game_version", "1", "-upc_exe_path"]);
        let dec = |s: &str| String::from_utf8(base64::engine::general_purpose::STANDARD.decode(s).unwrap()).unwrap();
        assert_eq!(dec(parts[5]), exe);
        assert_eq!(parts[6], "-upc_working_directory");
        assert_eq!(dec(parts[7]), r"Z:\home\u\Steam\steamapps\common\AC");
    }

    #[test]
    fn parents() {
        assert_eq!(win_parent(r"C:\a\b.exe"), r"C:\a");
        assert_eq!(win_parent(r"C:\b.exe"), r"C:\");
        assert_eq!(win_parent("b.exe"), "b.exe");
    }
}
