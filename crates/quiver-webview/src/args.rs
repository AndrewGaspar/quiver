//! The command line, which pages the window may open, and reading Epic's redirect page.
//! Everything here is plain Rust, unit-tested without a display.

use crate::ffi::WEBKIT_LIB;

/// Exit codes (the caller, `quiver_api::epic::webview`, maps them).
pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_CANCELLED: i32 = 2;
pub const EXIT_TIMED_OUT: i32 = 3;
pub const EXIT_UNAVAILABLE: i32 = 4;
pub const EXIT_USAGE: i32 = 64;

/// Where Epic's sign-in ends: a page of JSON holding `authorizationCode`.
pub const EPIC_REDIRECT: &str = "https://www.epicgames.com/id/api/redirect";
const EPIC_ORIGIN: &str = "https://www.epicgames.com/";

/// Seconds the window stays open by default.
pub const DEFAULT_TIMEOUT: u32 = 600;

/// The library to load instead of WebKit's, for tests (honoured only with a loopback
/// `--url`).
pub const TEST_LIB_ENV: &str = "QUIVER_WEBVIEW_TEST_LIB";

pub const USAGE: &str = "usage: quiver-webview epic-login --url <Epic sign-in URL> [--timeout <seconds>]

Opens Epic's sign-in page in a window of its own. When the sign-in ends on Epic's
redirect page, prints the authorization code on stdout and exits 0.
Exit codes: 0 signed in, 1 failed, 2 window closed, 3 timed out, 4 no WebKitGTK or no display.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub url: String,
    pub timeout: u32,
    pub redirect_prefix: String,
    /// Test-only: close the window once the first page has loaded.
    pub close_after_load: bool,
    pub lib: String,
}

/// `--url` may be Epic's site, or a loopback test server.
pub fn allowed_url(url: &str) -> bool {
    url.starts_with(EPIC_ORIGIN) || is_loopback(url)
}

/// `http://127.0.0.1…`, `http://localhost…` or `http://[::1]…`, with no user info.
pub fn is_loopback(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let host = if let Some(v6) = authority.strip_prefix('[') {
        match v6.split_once(']') {
            Some((h, port)) if port.is_empty() || port.starts_with(':') => h,
            _ => return false,
        }
    } else {
        authority.split(':').next().unwrap_or("")
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Whether `uri` is the redirect page: `prefix` itself, or it followed by `?`, `/` or `#`.
pub fn is_redirect(uri: &str, prefix: &str) -> bool {
    match uri.strip_prefix(prefix) {
        Some(rest) => rest.is_empty() || rest.starts_with(['?', '/', '#']),
        None => false,
    }
}

/// Parse the arguments after the program name. `test_lib` is [`TEST_LIB_ENV`]'s value.
pub fn parse(args: &[String], test_lib: Option<String>) -> Result<Options, String> {
    let mut it = args.iter();
    match it.next().map(String::as_str) {
        Some("epic-login") => {}
        Some("-h" | "--help") | None => return Err(USAGE.into()),
        Some(other) => return Err(format!("unknown command {other:?}\n\n{USAGE}")),
    }
    let mut url = None;
    let mut timeout = DEFAULT_TIMEOUT;
    let mut redirect = None;
    let mut close_after_load = false;
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--url" => url = Some(value("--url")?),
            "--timeout" => {
                timeout = value("--timeout")?.parse().ok().filter(|t| (1..=86_400).contains(t)).ok_or("--timeout takes 1 to 86400 seconds")?;
            }
            "--redirect-prefix" => redirect = Some(value("--redirect-prefix")?),
            "--close-after-load" => close_after_load = true,
            "-h" | "--help" => return Err(USAGE.into()),
            other => return Err(format!("unknown option {other:?}\n\n{USAGE}")),
        }
    }
    let url = url.ok_or_else(|| format!("--url is required\n\n{USAGE}"))?;
    if !allowed_url(&url) {
        return Err(format!("--url must be on {EPIC_ORIGIN}"));
    }
    let testing = is_loopback(&url);
    if !testing && (redirect.is_some() || close_after_load) {
        return Err("--redirect-prefix and --close-after-load are for tests against a local server only".into());
    }
    if let Some(r) = &redirect
        && !is_loopback(r)
    {
        return Err("--redirect-prefix must be a loopback URL".into());
    }
    let lib = match test_lib.filter(|l| testing && !l.is_empty()) {
        Some(l) => l,
        None => WEBKIT_LIB.to_owned(),
    };
    Ok(Options { url, timeout, redirect_prefix: redirect.unwrap_or_else(|| EPIC_REDIRECT.to_owned()), close_after_load, lib })
}

/// What the redirect page said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redirect {
    Code(String),
    /// `"authorizationCode": null`: Epic finished the sign-in on another page first.
    Null,
    /// Not the JSON Epic serves there.
    Invalid,
}

/// Read the redirect page's body.
pub fn read_redirect(body: &[u8]) -> Redirect {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else { return Redirect::Invalid };
    let Some(obj) = v.as_object() else { return Redirect::Invalid };
    match obj.get("authorizationCode") {
        Some(serde_json::Value::String(c)) if valid_code(c) => Redirect::Code(c.clone()),
        Some(serde_json::Value::String(c)) if c.is_empty() => Redirect::Null,
        Some(serde_json::Value::Null) | None if obj.contains_key("redirectUrl") || obj.contains_key("authorizationCode") => Redirect::Null,
        _ => Redirect::Invalid,
    }
}

/// Epic's codes are 32 hex digits; accept any short run of URL-safe characters.
fn valid_code(c: &str) -> bool {
    !c.is_empty() && c.len() <= 512 && c.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    const LOGIN: &str = "https://www.epicgames.com/id/login?redirectUrl=https%3A%2F%2Fwww.epicgames.com%2Fid%2Fapi%2Fredirect%3FclientId%3Dx%26responseType%3Dcode";

    #[test]
    fn epic_urls_and_defaults() {
        let o = parse(&args(&["epic-login", "--url", LOGIN]), Some("/tmp/evil.so".into())).unwrap();
        assert_eq!(o.url, LOGIN);
        assert_eq!(o.timeout, DEFAULT_TIMEOUT);
        assert_eq!(o.redirect_prefix, EPIC_REDIRECT);
        assert!(!o.close_after_load);
        // The library override is ignored for Epic's real page.
        assert_eq!(o.lib, WEBKIT_LIB);
        assert_eq!(parse(&args(&["epic-login", "--url", LOGIN, "--timeout", "30"]), None).unwrap().timeout, 30);
    }

    #[test]
    fn refuses_other_sites_and_test_options_on_epic() {
        for bad in ["https://evil.example/", "https://www.epicgames.com.evil.example/", "http://www.epicgames.com/id/login", "file:///etc/passwd", "http://127.0.0.1@evil.example/"] {
            assert!(parse(&args(&["epic-login", "--url", bad]), None).is_err(), "{bad}");
        }
        assert!(parse(&args(&["epic-login", "--url", LOGIN, "--close-after-load"]), None).is_err());
        assert!(parse(&args(&["epic-login", "--url", LOGIN, "--redirect-prefix", "http://127.0.0.1:1/r"]), None).is_err());
        assert!(parse(&args(&["epic-login", "--url", "http://127.0.0.1:5/login", "--redirect-prefix", "https://evil.example/"]), None).is_err());
        assert!(parse(&args(&["epic-login", "--url", LOGIN, "--timeout", "0"]), None).is_err());
        assert!(parse(&args(&["epic-login"]), None).is_err());
        assert!(parse(&args(&["sign-in", "--url", LOGIN]), None).is_err());
        assert!(parse(&args(&["epic-login", "--url", LOGIN, "--bogus"]), None).is_err());
        assert!(parse(&[], None).is_err());
    }

    #[test]
    fn loopback_tests_may_override() {
        let o = parse(&args(&["epic-login", "--url", "http://127.0.0.1:4321/login", "--redirect-prefix", "http://127.0.0.1:4321/id/api/redirect", "--close-after-load"]), Some("libnope.so".into())).unwrap();
        assert!(o.close_after_load);
        assert_eq!(o.redirect_prefix, "http://127.0.0.1:4321/id/api/redirect");
        assert_eq!(o.lib, "libnope.so");
        assert!(is_loopback("http://localhost/x") && is_loopback("http://[::1]:80/") && is_loopback("http://127.0.0.1"));
        assert!(!is_loopback("http://127.0.0.2/") && !is_loopback("https://127.0.0.1/") && !is_loopback("http://[::1]x/"));
    }

    #[test]
    fn redirect_matching() {
        assert!(is_redirect("https://www.epicgames.com/id/api/redirect?clientId=a&responseType=code", EPIC_REDIRECT));
        assert!(is_redirect(EPIC_REDIRECT, EPIC_REDIRECT));
        assert!(!is_redirect("https://www.epicgames.com/id/api/redirector", EPIC_REDIRECT));
        assert!(!is_redirect("https://www.epicgames.com/id/login?redirectUrl=https://www.epicgames.com/id/api/redirect", EPIC_REDIRECT));
    }

    #[test]
    fn redirect_bodies() {
        let page = br#"{"warning":"Do not share this code with any 3rd party service. It allows full access to your Epic account.","redirectUrl":"https://localhost/launcher/authorized?code=0123456789abcdef0123456789abcdef","authorizationCode":"0123456789abcdef0123456789abcdef","exchangeCode":null,"sid":null}"#;
        assert_eq!(read_redirect(page), Redirect::Code("0123456789abcdef0123456789abcdef".into()));
        let null = br#"{"warning":"...","redirectUrl":"https://www.epicgames.com/id/api/redirect","authorizationCode":null,"exchangeCode":null,"sid":null}"#;
        assert_eq!(read_redirect(null), Redirect::Null);
        assert_eq!(read_redirect(br#"{"authorizationCode":""}"#), Redirect::Null);
        assert_eq!(read_redirect(b"<html>sign in</html>"), Redirect::Invalid);
        assert_eq!(read_redirect(br#"["x"]"#), Redirect::Invalid);
        assert_eq!(read_redirect(br#"{"other":1}"#), Redirect::Invalid);
        assert_eq!(read_redirect(br#"{"authorizationCode":"has space"}"#), Redirect::Invalid);
        assert_eq!(read_redirect(br#"{"authorizationCode":42}"#), Redirect::Invalid);
    }
}
