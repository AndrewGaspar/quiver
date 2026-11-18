//! Steam's text VDF (KeyValues): reading `config/loginusers.vdf`, app manifests and
//! `localconfig.vdf`, and [`set_string`], which changes one value and leaves every other
//! byte of the file as it was.
//!
//! ```text
//! "users"
//! {
//!     "76561198000000000"
//!     {
//!         "AccountName"    "name"
//!         "PersonaName"    "Shown Name"
//!         "MostRecent"     "1"
//!     }
//! }
//! ```

use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Str(String),
    Map(Vec<(String, Node)>),
}

impl Node {
    /// Child by key, case-insensitively (Steam isn't consistent about case).
    pub fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Node::Map(m) => m.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v),
            Node::Str(_) => None,
        }
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Node::Str(s) => Some(s),
            Node::Map(_) => None,
        }
    }

    pub fn entries(&self) -> &[(String, Node)] {
        match self {
            Node::Map(m) => m,
            Node::Str(_) => &[],
        }
    }
}

#[derive(Debug, PartialEq)]
enum Tok {
    Str(String),
    Open,
    Close,
}

fn tokens(text: &str) -> Result<Vec<Tok>> {
    Ok(spanned(text)?.into_iter().map(|(t, _)| t).collect())
}

type Span = std::ops::Range<usize>;

/// Tokens with their byte ranges in `text`.
fn spanned(text: &str) -> Result<Vec<(Tok, Span)>> {
    let mut out = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '{' => out.push((Tok::Open, start..start + 1)),
            '}' => out.push((Tok::Close, start..start + 1)),
            '/' if chars.peek().map(|&(_, c)| c) == Some('/') => {
                for (_, c) in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '"' => {
                let mut s = String::new();
                let end = loop {
                    match chars.next() {
                        None => bail!("unterminated string in VDF"),
                        Some((i, '"')) => break i + 1,
                        Some((_, '\\')) => match chars.next().map(|(_, c)| c) {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some(c) => s.push(c),
                            None => bail!("unterminated string in VDF"),
                        },
                        Some((_, c)) => s.push(c),
                    }
                };
                out.push((Tok::Str(s), start..end));
            }
            // Unquoted token (also valid KeyValues), e.g. `#include` or bare words.
            c => {
                let mut s = String::from(c);
                while let Some(&(_, c)) = chars.peek() {
                    if c.is_whitespace() || c == '{' || c == '}' || c == '"' {
                        break;
                    }
                    s.push(c);
                    chars.next();
                }
                let end = chars.peek().map(|&(i, _)| i).unwrap_or(text.len());
                out.push((Tok::Str(s), start..end));
            }
        }
    }
    Ok(out)
}

/// A string as Steam writes it, quotes included.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Entries of the section whose tokens are `toks[lo..hi]`: (key index, value index, the
/// matching close brace when the value is a section).
fn entries(toks: &[(Tok, Span)], lo: usize, hi: usize) -> Result<Vec<(usize, usize, Option<usize>)>> {
    let mut out = Vec::new();
    let mut i = lo;
    while i < hi {
        let Tok::Str(_) = toks[i].0 else { bail!("unexpected brace in VDF") };
        match toks.get(i + 1).map(|t| &t.0) {
            Some(Tok::Str(_)) => {
                out.push((i, i + 1, None));
                i += 2;
            }
            Some(Tok::Open) => {
                let mut depth = 0usize;
                let mut j = i + 1;
                loop {
                    match toks.get(j).map(|t| &t.0) {
                        Some(Tok::Open) => depth += 1,
                        Some(Tok::Close) => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        Some(Tok::Str(_)) => {}
                        None => bail!("unexpected end of VDF"),
                    }
                    j += 1;
                }
                out.push((i, i + 1, Some(j)));
                i = j + 1;
            }
            _ => bail!("expected a value in VDF"),
        }
    }
    Ok(out)
}

/// Set the string at `path` (keys matched case-insensitively, the first match wins),
/// changing only that value's text. Missing keys are added at the end of their section,
/// tab-indented the way Steam writes them. Fails if a key on the way is a string, or the
/// last one is a section.
pub fn set_string(text: &str, path: &[&str], value: &str) -> Result<String> {
    if path.is_empty() {
        bail!("empty VDF path");
    }
    let bom = if text.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
    let (head, body) = text.split_at(bom);
    let toks = spanned(body)?;
    let (mut lo, mut hi, mut close) = (0, toks.len(), None::<usize>);
    for (depth, key) in path.iter().enumerate() {
        let last = depth + 1 == path.len();
        let found = entries(&toks, lo, hi)?
            .into_iter()
            .find(|(k, _, _)| matches!(&toks[*k].0, Tok::Str(s) if s.eq_ignore_ascii_case(key)));
        match found {
            Some((_, v, None)) if last => {
                let r = toks[v].1.clone();
                return Ok(format!("{head}{}{}{}", &body[..r.start], quote(value), &body[r.end..]));
            }
            Some((_, _, Some(_))) if last => bail!("\"{key}\" is a section, not a value"),
            Some((_, _, None)) => bail!("\"{key}\" is a value, not a section"),
            Some((_, open, Some(c))) => (lo, hi, close) = (open + 1, c, Some(c)),
            None => {
                let block = block(&path[depth..], value, depth);
                let out = match close {
                    Some(c) => {
                        let pos = toks[c].1.start;
                        let line = body[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
                        if body[line..pos].trim().is_empty() {
                            format!("{}{block}{}", &body[..line], &body[line..])
                        } else {
                            format!("{}\n{block}{}", &body[..pos], &body[pos..])
                        }
                    }
                    None if body.is_empty() || body.ends_with('\n') => format!("{body}{block}"),
                    None => format!("{body}\n{block}"),
                };
                return Ok(format!("{head}{out}"));
            }
        }
    }
    unreachable!("the loop returns at the last key")
}

/// Take the entry at `path` (a value or a whole section) out of the text, with its line when
/// it has one to itself; every other byte stays. The inverse of [`set_string`] adding it.
/// `None`: there's no such entry.
pub fn remove(text: &str, path: &[&str]) -> Result<Option<String>> {
    if path.is_empty() {
        bail!("empty VDF path");
    }
    let bom = if text.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
    let (head, body) = text.split_at(bom);
    let toks = spanned(body)?;
    let (mut lo, mut hi) = (0, toks.len());
    for (depth, key) in path.iter().enumerate() {
        let last = depth + 1 == path.len();
        let found = entries(&toks, lo, hi)?
            .into_iter()
            .find(|(k, _, _)| matches!(&toks[*k].0, Tok::Str(s) if s.eq_ignore_ascii_case(key)));
        match found {
            None => return Ok(None),
            Some((k, v, close)) if last => {
                let mut start = toks[k].1.start;
                let mut end = close.map_or(toks[v].1.end, |c| toks[c].1.end);
                let line = body[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
                let eol = body[end..].find('\n').map(|i| end + i + 1).unwrap_or(body.len());
                if body[line..start].trim().is_empty() && body[end..eol].trim().is_empty() {
                    (start, end) = (line, eol);
                }
                return Ok(Some(format!("{head}{}{}", &body[..start], &body[end..])));
            }
            Some((_, _, None)) => bail!("\"{key}\" is a value, not a section"),
            Some((_, open, Some(c))) => (lo, hi) = (open + 1, c),
        }
    }
    unreachable!("the loop returns at the last key")
}

/// `keys` (a section path ending in a value's key) as new text at `indent` tabs.
fn block(keys: &[&str], value: &str, indent: usize) -> String {
    let tabs = "\t".repeat(indent);
    match keys {
        [k] => format!("{tabs}{}\t\t{}\n", quote(k), quote(value)),
        [k, rest @ ..] => format!("{tabs}{}\n{tabs}{{\n{}{tabs}}}\n", quote(k), block(rest, value, indent + 1)),
        [] => String::new(),
    }
}

/// Parse a whole file into a map of its top-level keys.
pub fn parse(text: &str) -> Result<Node> {
    let toks = tokens(text.trim_start_matches('\u{feff}'))?;
    let mut it = toks.into_iter().peekable();
    fn map(it: &mut std::iter::Peekable<std::vec::IntoIter<Tok>>, top: bool) -> Result<Node> {
        let mut out = Vec::new();
        loop {
            let key = match it.next() {
                None if top => return Ok(Node::Map(out)),
                None => bail!("unexpected end of VDF"),
                Some(Tok::Close) if !top => return Ok(Node::Map(out)),
                Some(Tok::Str(k)) => k,
                Some(t) => bail!("unexpected {t:?} in VDF"),
            };
            let value = match it.next() {
                Some(Tok::Str(v)) => Node::Str(v),
                Some(Tok::Open) => map(it, false)?,
                t => bail!("expected a value for \"{key}\", got {t:?}"),
            };
            out.push((key, value));
        }
    }
    map(&mut it, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loginusers() {
        let text = r#""users"
{
	"76561198000000001"
	{
		"AccountName"		"alice"
		"PersonaName"		"Alice \"A\" Smith"
		"RememberPassword"		"1"
		"MostRecent"		"1"
		"Timestamp"		"1794527998"
	}
	// a comment
	"76561198000000002"
	{
		"AccountName"		"bob"
		"PersonaName"		"Bob's Pills"
		"mostrecent"		"0"
	}
}
"#;
        let root = parse(text).unwrap();
        let users = root.get("users").unwrap();
        assert_eq!(users.entries().len(), 2);
        let a = users.get("76561198000000001").unwrap();
        assert_eq!(a.str("PersonaName"), Some(r#"Alice "A" Smith"#));
        assert_eq!(a.str("mostrecent"), Some("1"));
        assert_eq!(users.get("76561198000000002").unwrap().str("AccountName"), Some("bob"));
        assert!(parse("\"a\" {").is_err());
        assert!(parse("\"a\" \"b").is_err());
    }

    /// The shape of `userdata/<id>/config/localconfig.vdf` (trimmed from a real one).
    const LOCALCONFIG: &str = "\"UserLocalConfigStore\"\n{\n\t\"Broadcast\"\n\t{\n\t\t\"Permissions\"\t\t\"1\"\n\t}\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"apps\"\n\t\t\t\t{\n\t\t\t\t\t\"1478500\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"Playtime\"\t\t\"1806\"\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"DXVK_CONFIG=\\\"dxgi.maxFrameRate=240\\\" %command%\"\n\t\t\t\t\t}\n\t\t\t\t\t\"3751950\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LastPlayed\"\t\t\"1794793719\"\n\t\t\t\t\t\t\"perfreporting\"\n\t\t\t\t\t\t{\n\t\t\t\t\t\t\t\"weekstart\"\t\t\"1794548663\"\n\t\t\t\t\t\t}\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n\t\"depots\"\n\t{\n\t\t\"3751950\"\n\t\t{\n\t\t\t\"x\"\t\t\"y\"\n\t\t}\n\t}\n}\n";

    const APP: [&str; 5] = ["UserLocalConfigStore", "Software", "Valve", "Steam", "apps"];

    fn path<'a>(app: &'a str, key: &'a str) -> Vec<&'a str> {
        let mut p = APP.to_vec();
        p.extend([app, key]);
        p
    }

    fn get(text: &str, app: &str) -> Option<String> {
        let mut n = &parse(text).unwrap();
        for k in APP.iter().chain([&app]) {
            n = n.get(k)?;
        }
        n.str("LaunchOptions").map(str::to_owned)
    }

    #[test]
    fn edit_in_place() {
        assert_eq!(get(LOCALCONFIG, "1478500").as_deref(), Some("DXVK_CONFIG=\"dxgi.maxFrameRate=240\" %command%"));
        // Replace an existing value: one line changes, nothing else.
        let new = "DXVK_CONFIG=\"dxgi.maxFrameRate=240\" '/h/q u/quiver' --source steam steam-run %command%";
        let out = set_string(LOCALCONFIG, &path("1478500", "launchoptions"), new).unwrap();
        assert_eq!(get(&out, "1478500").as_deref(), Some(new));
        let changed: Vec<_> = LOCALCONFIG.lines().zip(out.lines()).filter(|(a, b)| a != b).collect();
        assert_eq!(out.lines().count(), LOCALCONFIG.lines().count());
        assert_eq!(changed.len(), 1);
        assert_eq!(
            changed[0].1,
            "\t\t\t\t\t\t\"LaunchOptions\"\t\t\"DXVK_CONFIG=\\\"dxgi.maxFrameRate=240\\\" '/h/q u/quiver' --source steam steam-run %command%\""
        );
        // And back, byte for byte.
        let back = set_string(&out, &path("1478500", "LaunchOptions"), "DXVK_CONFIG=\"dxgi.maxFrameRate=240\" %command%").unwrap();
        assert_eq!(back, LOCALCONFIG);
    }

    #[test]
    fn removing_what_set_string_added_restores_the_text() {
        // A key added to an existing app's section, then taken out: byte for byte.
        let out = set_string(LOCALCONFIG, &path("3751950", "LaunchOptions"), "q %command%").unwrap();
        assert_eq!(remove(&out, &path("3751950", "LaunchOptions")).unwrap().unwrap(), LOCALCONFIG);
        // A whole app section added, then taken out.
        let out = set_string(LOCALCONFIG, &path("870780", "LaunchOptions"), "q %command%").unwrap();
        let mut sec = APP.to_vec();
        sec.push("870780");
        assert_eq!(remove(&out, &sec).unwrap().unwrap(), LOCALCONFIG);
        // An existing entry: only its line goes.
        let gone = remove(LOCALCONFIG, &path("1478500", "LaunchOptions")).unwrap().unwrap();
        assert_eq!(get(&gone, "1478500"), None);
        assert_eq!(gone.lines().count() + 1, LOCALCONFIG.lines().count());
        // Not there: nothing to do. A value on the way: an error.
        assert_eq!(remove(LOCALCONFIG, &path("42", "LaunchOptions")).unwrap(), None);
        assert!(remove(LOCALCONFIG, &["UserLocalConfigStore", "Broadcast", "Permissions", "x"]).is_err());
        // Several entries on one line: only the entry's own text goes.
        assert_eq!(remove("\"a\" { \"b\" \"1\" \"c\" \"2\" }\n", &["a", "b"]).unwrap().unwrap(), "\"a\" {  \"c\" \"2\" }\n");
    }

    #[test]
    fn edit_adds_keys() {
        // A missing value goes at the end of its section, indented like its siblings.
        let out = set_string(LOCALCONFIG, &path("3751950", "LaunchOptions"), "q %command%").unwrap();
        assert_eq!(get(&out, "3751950").as_deref(), Some("q %command%"));
        assert!(out.contains("\t\t\t\t\t\t}\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"q %command%\"\n\t\t\t\t\t}\n\t\t\t\t}\n"), "{out}");
        // The depots section's "3751950" is untouched.
        assert_eq!(parse(&out).unwrap().get("UserLocalConfigStore").unwrap().get("depots"), parse(LOCALCONFIG).unwrap().get("UserLocalConfigStore").unwrap().get("depots"));
        // A missing app section.
        let out = set_string(LOCALCONFIG, &path("42", "LaunchOptions"), "-x").unwrap();
        assert_eq!(get(&out, "42").as_deref(), Some("-x"));
        assert!(out.contains("\t\t\t\t\t\"42\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"-x\"\n\t\t\t\t\t}\n\t\t\t\t}\n"), "{out}");
        assert_eq!(get(&out, "1478500"), get(LOCALCONFIG, "1478500"));
        // From nothing at all.
        let out = set_string("", &path("7", "LaunchOptions"), "a\\b").unwrap();
        assert_eq!(get(&out, "7").as_deref(), Some("a\\b"));
        assert!(out.starts_with("\"UserLocalConfigStore\"\n{\n\t\"Software\"\n\t{\n"));
        // A byte-order mark stays.
        let out = set_string(&format!("\u{feff}{LOCALCONFIG}"), &path("7", "LaunchOptions"), "x").unwrap();
        assert!(out.starts_with('\u{feff}'));
        assert_eq!(get(&out, "7").as_deref(), Some("x"));
        // Wrong shapes are refused, not guessed.
        assert!(set_string(LOCALCONFIG, &path("3751950", "perfreporting"), "x").is_err());
        assert!(set_string(LOCALCONFIG, &["UserLocalConfigStore", "Broadcast", "Permissions", "x"], "x").is_err());
        assert!(set_string("\"a\" {", &["a", "b"], "x").is_err());
    }
}
