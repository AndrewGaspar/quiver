//! Steam's text VDF (KeyValues), read-only: enough for `config/loginusers.vdf`.
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
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '{' => out.push(Tok::Open),
            '}' => out.push(Tok::Close),
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '"' => {
                let mut s = String::new();
                loop {
                    match chars.next() {
                        None => bail!("unterminated string in VDF"),
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some(c) => s.push(c),
                            None => bail!("unterminated string in VDF"),
                        },
                        Some(c) => s.push(c),
                    }
                }
                out.push(Tok::Str(s));
            }
            // Unquoted token (also valid KeyValues), e.g. `#include` or bare words.
            c => {
                let mut s = String::from(c);
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || c == '{' || c == '}' || c == '"' {
                        break;
                    }
                    s.push(c);
                    chars.next();
                }
                out.push(Tok::Str(s));
            }
        }
    }
    Ok(out)
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
}
