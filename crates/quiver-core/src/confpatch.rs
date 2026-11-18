//! Config patches (Mods v2): structured key edits to INI and XML files a game keeps in its
//! prefix, applied at each launch and undone when the mod that wants them is gone.
//!
//! The editors change only the lines (INI) or attribute values (XML) they're asked to and
//! keep everything else byte for byte: comments, order, unknown content, line endings, a
//! byte-order mark. UTF-16 files (some Unreal Engine 3 games write them) are read and
//! written back as UTF-16.
//!
//! **The record** (`<data>/mods/<product>/patches.json`): every key Quiver has changed, in
//! which file, the file's own values from before Quiver first touched it, the value applied
//! and the mods that want it. A launch reconciles the files it can see with what its stack
//! wants: wanted keys are set (re-applied every launch, since games rewrite these files),
//! and recorded keys no mod wants any more go back to the file's own values, unless the
//! game or the person has changed them since, in which case they're left alone.
//!
//! **What a launch sees** ([`View`]): the prefix, except folders the profile has its own
//! copy of (`Profile::profile_config`), which the launch overlays with
//! `<data>/mods/<product>/profiles/<id>/prefix/<dir>`. A patch to a file there edits the
//! profile's copy (copied up from the prefix first, as overlayfs would), so it changes the
//! file the game sees in that profile and nowhere else. Patching happens before the
//! launch's namespace is mounted, never under a live mount.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use quiver_api::mods::{AppliedKey, ConfigPatch, PatchFormat, PatchState, PatchStatus, PatchedFile};

// -- text ----------------------------------------------------------------------------------

/// How a file's text was encoded, to write it back the same way.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Encoding {
    Utf8 { bom: bool },
    Utf16 { le: bool },
}

/// A config file's bytes as text (UTF-8 with or without a BOM, or UTF-16), for readers that
/// don't write it back.
pub fn decode_text(bytes: &[u8]) -> Result<(String, bool)> {
    decode(bytes).map(|(t, e)| (t, matches!(e, Encoding::Utf16 { .. })))
}

fn decode(bytes: &[u8]) -> Result<(String, Encoding)> {
    let utf16 = |le: bool| -> Result<(String, Encoding)> {
        let body = &bytes[2..];
        if !body.len().is_multiple_of(2) {
            bail!("odd-length UTF-16 text");
        }
        let units: Vec<u16> =
            body.chunks(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
        Ok((String::from_utf16(&units).context("invalid UTF-16 text")?, Encoding::Utf16 { le }))
    };
    match bytes {
        [0xFF, 0xFE, ..] => utf16(true),
        [0xFE, 0xFF, ..] => utf16(false),
        [0xEF, 0xBB, 0xBF, rest @ ..] => {
            Ok((String::from_utf8(rest.to_vec()).context("not UTF-8 text")?, Encoding::Utf8 { bom: true }))
        }
        _ => Ok((String::from_utf8(bytes.to_vec()).context("not UTF-8 text")?, Encoding::Utf8 { bom: false })),
    }
}

fn encode(text: &str, enc: Encoding) -> Vec<u8> {
    match enc {
        Encoding::Utf8 { bom } => {
            let mut out = if bom { vec![0xEF, 0xBB, 0xBF] } else { Vec::new() };
            out.extend_from_slice(text.as_bytes());
            out
        }
        Encoding::Utf16 { le } => {
            let mut out = if le { vec![0xFF, 0xFE] } else { vec![0xFE, 0xFF] };
            for u in text.encode_utf16() {
                out.extend_from_slice(&if le { u.to_le_bytes() } else { u.to_be_bytes() });
            }
            out
        }
    }
}

/// Edit a config file in place with `f`, writing it back in its own encoding (through a
/// temporary file and a rename) only if the text changed (`quiver lab set`).
pub fn edit_file<T>(path: &Path, format: PatchFormat, f: impl FnOnce(&mut Doc) -> Result<T>) -> Result<T> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let (text, enc) = decode(&bytes).with_context(|| format!("decode {}", path.display()))?;
    let mut doc = Doc::parse(format, &text);
    let out = f(&mut doc)?;
    let new = doc.render();
    if new != text {
        let tmp = path.with_extension("quiver-tmp");
        std::fs::write(&tmp, encode(&new, enc)).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    }
    Ok(out)
}

// -- INI -----------------------------------------------------------------------------------

/// Where a key was in its section or element: after the entry (key or attribute) named
/// `after`, or first. Recorded when a patch deletes a key, so undoing puts it back there.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Anchor {
    #[serde(default)]
    pub after: Option<String>,
}

#[derive(Debug, Clone)]
struct Line {
    text: String,
    /// `\r\n`, `\n` or empty (the last line without a newline).
    eol: String,
}

/// An INI file as lines. Sections and keys match without case; a key may appear several
/// times in a section (Unreal Engine arrays such as `StartupMovies=`).
#[derive(Debug, Clone)]
pub struct Ini {
    lines: Vec<Line>,
    newline: String,
}

fn section_name(text: &str) -> Option<&str> {
    let t = text.trim();
    let inner = t.strip_prefix('[')?;
    let end = inner.rfind(']')?;
    Some(inner[..end].trim())
}

/// `(key, value)` of a `key=value` line (not a comment or section).
fn key_value(text: &str) -> Option<(&str, &str)> {
    let t = text.trim_start();
    if t.is_empty() || t.starts_with(';') || t.starts_with('#') || t.starts_with('[') {
        return None;
    }
    let (k, v) = t.split_once('=')?;
    let k = k.trim();
    (!k.is_empty()).then_some((k, v.trim()))
}

impl Ini {
    pub fn parse(text: &str) -> Ini {
        let mut lines = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            let (line, eol, next) = match rest.find('\n') {
                Some(i) if i > 0 && rest.as_bytes()[i - 1] == b'\r' => (&rest[..i - 1], "\r\n", &rest[i + 1..]),
                Some(i) => (&rest[..i], "\n", &rest[i + 1..]),
                None => (rest, "", ""),
            };
            lines.push(Line { text: line.to_owned(), eol: eol.to_owned() });
            rest = next;
        }
        let newline = lines.iter().map(|l| l.eol.as_str()).find(|e| !e.is_empty()).unwrap_or("\n").to_owned();
        Ini { lines, newline }
    }

    pub fn render(&self) -> String {
        self.lines.iter().flat_map(|l| [l.text.as_str(), l.eol.as_str()]).collect()
    }

    /// Line ranges `[start, end)` of every section named `section` (`""`: the lines before
    /// the first section). `start` is the header line (or 0 for the preamble).
    fn ranges(&self, section: &str) -> Vec<(usize, usize)> {
        let headers: Vec<usize> = (0..self.lines.len()).filter(|&i| section_name(&self.lines[i].text).is_some()).collect();
        let mut out = Vec::new();
        if section.is_empty() {
            out.push((0, headers.first().copied().unwrap_or(self.lines.len())));
            return out;
        }
        for (n, &h) in headers.iter().enumerate() {
            if section_name(&self.lines[h].text).is_some_and(|s| s.eq_ignore_ascii_case(section)) {
                out.push((h, headers.get(n + 1).copied().unwrap_or(self.lines.len())));
            }
        }
        out
    }

    fn key_lines(&self, section: &str, key: &str) -> Vec<usize> {
        let mut out = Vec::new();
        for (start, end) in self.ranges(section) {
            let first = if section.is_empty() { start } else { start + 1 };
            for i in first..end {
                if key_value(&self.lines[i].text).is_some_and(|(k, _)| k.eq_ignore_ascii_case(key)) {
                    out.push(i);
                }
            }
        }
        out
    }

    /// Every value of `key` in `section`, in file order.
    pub fn get(&self, section: &str, key: &str) -> Vec<String> {
        self.key_lines(section, key).into_iter().filter_map(|i| key_value(&self.lines[i].text).map(|(_, v)| v.to_owned())).collect()
    }

    fn line(&self, text: String) -> Line {
        Line { text, eol: self.newline.clone() }
    }

    /// Where `key` is in its section: the key of the entry before its first line (`None`:
    /// it's the section's first entry). `None` if the key isn't there.
    pub fn anchor(&self, section: &str, key: &str) -> Option<Anchor> {
        let first = *self.key_lines(section, key).first()?;
        let after = (0..first)
            .rev()
            .map_while(|i| (section_name(&self.lines[i].text).is_none()).then_some(i))
            .find_map(|i| key_value(&self.lines[i].text).map(|(k, _)| k.to_owned()));
        Some(Anchor { after })
    }

    pub fn set(&mut self, section: &str, key: &str, values: &[String]) {
        self.set_at(section, key, values, None)
    }

    /// Give `key` exactly `values` (one line each; none: the key is removed). Existing
    /// lines keep their spelling of the key and their spacing around `=`. New ones go where
    /// `anchor` says (where the key was before it was deleted), else after the section's
    /// last entry; a missing section is added at the end.
    pub fn set_at(&mut self, section: &str, key: &str, values: &[String], anchor: Option<&Anchor>) {
        let found = self.key_lines(section, key);
        if let Some(&first) = found.first() {
            let text = &self.lines[first].text;
            let eq = text.find('=').unwrap();
            let after = &text[eq + 1..];
            let pad = &after[..after.len() - after.trim_start().len()];
            let prefix = format!("{}{pad}", &text[..=eq]);
            let eol = self.lines[first].eol.clone();
            for &i in found.iter().rev() {
                self.lines.remove(i);
            }
            for (n, v) in values.iter().enumerate() {
                let mut l = self.line(format!("{prefix}{v}"));
                if n + 1 == values.len() {
                    l.eol = eol.clone();
                }
                self.lines.insert(first + n, l);
            }
            return;
        }
        if values.is_empty() {
            return;
        }
        let new: Vec<Line> = values.iter().map(|v| self.line(format!("{key}={v}"))).collect();
        let at = match self.ranges(section).first() {
            Some(&(start, end)) => {
                let floor = if section.is_empty() { start } else { start + 1 };
                let anchored = anchor.and_then(|a| match &a.after {
                    None => Some(floor),
                    Some(prev) => (floor..end)
                        .rev()
                        .find(|&i| key_value(&self.lines[i].text).is_some_and(|(k, _)| k.eq_ignore_ascii_case(prev)))
                        .map(|i| i + 1),
                });
                anchored.unwrap_or_else(|| {
                    // After the section's last non-blank line.
                    let mut at = end;
                    while at > floor && self.lines[at - 1].text.trim().is_empty() {
                        at -= 1;
                    }
                    at
                })
            }
            None => {
                if let Some(last) = self.lines.last()
                    && !last.text.trim().is_empty()
                {
                    let blank = self.line(String::new());
                    self.ensure_eol();
                    self.lines.push(blank);
                }
                self.ensure_eol();
                let header = self.line(format!("[{section}]"));
                self.lines.push(header);
                self.lines.len()
            }
        };
        if at > 0 && self.lines[at - 1].eol.is_empty() {
            self.lines[at - 1].eol = self.newline.clone();
        }
        for (n, l) in new.into_iter().enumerate() {
            self.lines.insert(at + n, l);
        }
    }

    fn ensure_eol(&mut self) {
        let nl = self.newline.clone();
        if let Some(last) = self.lines.last_mut()
            && last.eol.is_empty()
        {
            last.eol = nl;
        }
    }
}

// -- XML -----------------------------------------------------------------------------------

/// An XML file as text, edited in place at attribute level. Elements are found by a path of
/// element names from the root (`GamerProfile/ProfileSpecificGameProfile`); comments,
/// processing instructions, CDATA and doctypes are skipped. Not a validating parser: just
/// enough to change attributes without touching anything else.
#[derive(Debug, Clone)]
pub struct Xml {
    text: String,
}

#[derive(Debug)]
struct Attr {
    name: String,
    /// From the whitespace before the name to the closing quote (what removal cuts).
    span: (usize, usize),
    /// The value between the quotes.
    value: (usize, usize),
    quote: u8,
}

#[derive(Debug)]
struct Tag {
    attrs: Vec<Attr>,
    /// Where a new attribute goes: after the last attribute (or the name).
    insert_at: usize,
    name_end: usize,
}

fn skip_to(s: &[u8], from: usize, pat: &[u8]) -> Result<usize> {
    s[from..].windows(pat.len()).position(|w| w == pat).map(|p| from + p + pat.len()).context("unterminated XML construct")
}

fn xml_escape(v: &str, quote: u8) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if quote == b'"' => out.push_str("&quot;"),
            '\'' if quote == b'\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

fn xml_unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut rest = v;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';') else { break };
        let ent = &rest[1..end];
        let c = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

impl Xml {
    pub fn parse(text: &str) -> Xml {
        Xml { text: text.to_owned() }
    }

    pub fn render(&self) -> String {
        self.text.clone()
    }

    /// The start tag of the first element at `path`.
    fn find(&self, path: &str) -> Result<Tag> {
        let want: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        if want.is_empty() {
            bail!("empty element path");
        }
        let s = self.text.as_bytes();
        let mut stack: Vec<String> = Vec::new();
        let mut i = 0;
        while let Some(p) = s[i..].iter().position(|&b| b == b'<') {
            i += p;
            let rest = &s[i..];
            if rest.starts_with(b"<!--") {
                i = skip_to(s, i + 4, b"-->")?;
            } else if rest.starts_with(b"<![CDATA[") {
                i = skip_to(s, i + 9, b"]]>")?;
            } else if rest.starts_with(b"<?") {
                i = skip_to(s, i + 2, b"?>")?;
            } else if rest.starts_with(b"<!") {
                // A doctype, perhaps with an internal subset in brackets.
                let close = skip_to(s, i, b">")?;
                i = match s[i..close].contains(&b'[') {
                    true => skip_to(s, i, b"]>")?,
                    false => close,
                };
            } else if rest.starts_with(b"</") {
                i = skip_to(s, i, b">")?;
                stack.pop();
            } else {
                let (tag, end, self_closing, name) = self.start_tag(i)?;
                stack.push(name);
                if stack.len() == want.len() && stack.iter().zip(&want).all(|(a, b)| a == b) {
                    return Ok(tag);
                }
                if self_closing {
                    stack.pop();
                }
                i = end;
            }
        }
        bail!("no element {path}")
    }

    /// Parse the start tag at `at`: (tag, index after it, self-closing, name).
    fn start_tag(&self, at: usize) -> Result<(Tag, usize, bool, String)> {
        let s = self.text.as_bytes();
        let ws = |b: u8| b.is_ascii_whitespace();
        let mut i = at + 1;
        while i < s.len() && !ws(s[i]) && s[i] != b'/' && s[i] != b'>' {
            i += 1;
        }
        let name = self.text[at + 1..i].to_owned();
        if name.is_empty() {
            bail!("malformed XML near byte {at}");
        }
        let mut attrs = Vec::new();
        let mut insert_at = i;
        let name_end = i;
        loop {
            let ws_start = i;
            while i < s.len() && ws(s[i]) {
                i += 1;
            }
            match s.get(i) {
                None => bail!("unterminated tag <{name}"),
                Some(b'>') => return Ok((Tag { attrs, insert_at, name_end }, i + 1, false, name)),
                Some(b'/') if s.get(i + 1) == Some(&b'>') => return Ok((Tag { attrs, insert_at, name_end }, i + 2, true, name)),
                _ => {}
            }
            let n0 = i;
            while i < s.len() && !ws(s[i]) && s[i] != b'=' && s[i] != b'>' && s[i] != b'/' {
                i += 1;
            }
            let aname = self.text[n0..i].to_owned();
            while i < s.len() && ws(s[i]) {
                i += 1;
            }
            if s.get(i) != Some(&b'=') || aname.is_empty() {
                bail!("malformed attribute in <{name}");
            }
            i += 1;
            while i < s.len() && ws(s[i]) {
                i += 1;
            }
            let quote = *s.get(i).context("unterminated tag")?;
            if quote != b'"' && quote != b'\'' {
                bail!("unquoted attribute {aname} in <{name}");
            }
            let v0 = i + 1;
            let v1 = v0 + s[v0..].iter().position(|&b| b == quote).context("unterminated attribute value")?;
            i = v1 + 1;
            attrs.push(Attr { name: aname, span: (ws_start, i), value: (v0, v1), quote });
            insert_at = i;
        }
    }

    pub fn get(&self, path: &str, attr: &str) -> Result<Vec<String>> {
        let tag = self.find(path)?;
        Ok(tag.attrs.iter().filter(|a| a.name == attr).map(|a| xml_unescape(&self.text[a.value.0..a.value.1])).take(1).collect())
    }

    /// The attribute before `attr` in its element (see [`Anchor`]); `None` if it isn't set.
    pub fn anchor(&self, path: &str, attr: &str) -> Result<Option<Anchor>> {
        let tag = self.find(path)?;
        let Some(n) = tag.attrs.iter().position(|a| a.name == attr) else { return Ok(None) };
        Ok(Some(Anchor { after: n.checked_sub(1).map(|p| tag.attrs[p].name.clone()) }))
    }

    pub fn set(&mut self, path: &str, attr: &str, values: &[String]) -> Result<()> {
        self.set_at(path, attr, values, None)
    }

    /// Give the attribute `values` (at most one; none removes it). A new attribute goes
    /// where `anchor` says, else last.
    pub fn set_at(&mut self, path: &str, attr: &str, values: &[String], anchor: Option<&Anchor>) -> Result<()> {
        if values.len() > 1 {
            bail!("an XML attribute has one value");
        }
        let tag = self.find(path)?;
        match (tag.attrs.iter().find(|a| a.name == attr), values.first()) {
            (Some(a), Some(v)) => self.text.replace_range(a.value.0..a.value.1, &xml_escape(v, a.quote)),
            (Some(a), None) => self.text.replace_range(a.span.0..a.span.1, ""),
            (None, Some(v)) => {
                let at = match anchor.map(|a| &a.after) {
                    Some(None) => tag.name_end,
                    Some(Some(prev)) => tag.attrs.iter().find(|a| &a.name == prev).map_or(tag.insert_at, |a| a.span.1),
                    None => tag.insert_at,
                };
                self.text.insert_str(at, &format!(" {attr}=\"{}\"", xml_escape(v, b'"')))
            }
            (None, None) => {}
        }
        Ok(())
    }
}

// -- documents -------------------------------------------------------------------------------

/// A config file in one of the supported formats.
#[derive(Debug, Clone)]
pub enum Doc {
    Ini(Ini),
    Xml(Xml),
}

impl Doc {
    pub fn parse(format: PatchFormat, text: &str) -> Doc {
        match format {
            PatchFormat::Ini => Doc::Ini(Ini::parse(text)),
            PatchFormat::Xml => Doc::Xml(Xml::parse(text)),
        }
    }

    pub fn get(&self, target: &str, key: &str) -> Result<Vec<String>> {
        match self {
            Doc::Ini(d) => Ok(d.get(target, key)),
            Doc::Xml(d) => d.get(target, key),
        }
    }

    pub fn anchor(&self, target: &str, key: &str) -> Result<Option<Anchor>> {
        match self {
            Doc::Ini(d) => Ok(d.anchor(target, key)),
            Doc::Xml(d) => d.anchor(target, key),
        }
    }

    pub fn set(&mut self, target: &str, key: &str, values: &[String], anchor: Option<&Anchor>) -> Result<()> {
        match self {
            Doc::Ini(d) => {
                d.set_at(target, key, values, anchor);
                Ok(())
            }
            Doc::Xml(d) => d.set_at(target, key, values, anchor),
        }
    }

    pub fn render(&self) -> String {
        match self {
            Doc::Ini(d) => d.render(),
            Doc::Xml(d) => d.render(),
        }
    }
}

/// Check a patch on its own: a prefix-relative file, the right target field for its
/// format, at least one edit, no newlines in values.
pub fn validate(p: &ConfigPatch) -> Result<()> {
    let file = &p.file;
    if file.is_empty() || file.starts_with('/') || file.contains('\\') || file.split('/').any(|c| c == ".." || c == ".") {
        bail!("patch file {file:?}: give a path relative to the prefix, with '/' and no '..'");
    }
    if file.split('/').any(|c| c.is_empty()) {
        bail!("patch file {file:?}: empty path component");
    }
    match p.format {
        PatchFormat::Ini if !p.element.is_empty() => bail!("{file}: an INI patch names a `section`, not an `element`"),
        PatchFormat::Xml if !p.section.is_empty() => bail!("{file}: an XML patch names an `element`, not a `section`"),
        PatchFormat::Xml if p.element.trim_matches('/').is_empty() => bail!("{file}: an XML patch needs an `element` path"),
        _ => {}
    }
    if p.set.is_empty() && p.delete.is_empty() {
        bail!("{file}: the patch neither sets nor deletes anything");
    }
    for (k, v) in &p.set {
        if k.trim().is_empty() || k.contains(['\n', '\r', '=']) || v.contains(['\n', '\r']) {
            bail!("{file}: bad key or value {k:?} = {v:?}");
        }
        if p.delete.iter().any(|d| d.eq_ignore_ascii_case(k)) {
            bail!("{file}: {k} is both set and deleted");
        }
    }
    Ok(())
}

// -- what a launch sees ------------------------------------------------------------------------

/// The prefix as a launch sees it: the prefix itself, except the profile's own folders.
#[derive(Debug, Clone)]
pub struct View {
    pub prefix: PathBuf,
    /// The profile (id) whose own folders (`dirs`, prefix-relative) live under `base`
    /// (`<profile dir>/prefix`, laid out like the prefix).
    pub profile: Option<(String, PathBuf, Vec<String>)>,
}

/// `name` against a pattern with `*` and `?`.
pub(crate) fn wildcard(pat: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pat.chars().collect(), name.chars().collect());
    let (mut pi, mut ni, mut star, mut mark) = (0, 0, None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

fn is_whiteout(p: &Path) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    p.symlink_metadata().is_ok_and(|m| m.file_type().is_char_device() && m.rdev() == 0)
}

fn under(rel: &str, dir: &str) -> bool {
    rel == dir || rel.strip_prefix(dir).is_some_and(|r| r.starts_with('/'))
}

impl View {
    pub fn prefix_only(prefix: &Path) -> View {
        View { prefix: prefix.to_owned(), profile: None }
    }

    /// The scope a prefix-relative path is in: `profile:<id>` inside the profile's own
    /// folders, else `prefix`.
    pub fn scope(&self, rel: &str) -> String {
        match &self.profile {
            Some((id, _, dirs)) if dirs.iter().any(|d| under(rel, d)) => format!("profile:{id}"),
            _ => "prefix".into(),
        }
    }

    fn base(&self) -> Option<&Path> {
        self.profile.as_ref().map(|p| p.1.as_path())
    }

    /// Where the file `rel` the launch sees really is (`None`: it doesn't exist).
    pub fn locate(&self, rel: &str) -> Option<PathBuf> {
        if self.scope(rel) != "prefix" {
            let own = self.base()?.join(rel);
            if is_whiteout(&own) {
                return None;
            }
            if own.symlink_metadata().is_ok() {
                return Some(own);
            }
        }
        let p = self.prefix.join(rel);
        p.symlink_metadata().is_ok().then_some(p)
    }

    /// Where writing `rel` goes: the profile's copy for its own folders, else the prefix.
    pub fn write_path(&self, rel: &str) -> PathBuf {
        match (self.scope(rel).as_str(), self.base()) {
            ("prefix", _) | (_, None) => self.prefix.join(rel),
            (_, Some(base)) => base.join(rel),
        }
    }

    fn list(&self, rel_dir: &str) -> Vec<String> {
        let mut names = std::collections::BTreeSet::new();
        let read = |d: PathBuf| -> Vec<String> {
            std::fs::read_dir(d).into_iter().flatten().flatten().filter_map(|e| e.file_name().into_string().ok()).collect()
        };
        let join = |a: &Path| if rel_dir.is_empty() { a.to_owned() } else { a.join(rel_dir) };
        names.extend(read(join(&self.prefix)));
        if let Some(base) = self.base() {
            names.extend(read(join(base)));
        }
        names.into_iter().filter(|n| self.locate(&if rel_dir.is_empty() { n.clone() } else { format!("{rel_dir}/{n}") }).is_some()).collect()
    }

    /// Files matching `pattern` (prefix-relative, `*` in components) the launch sees.
    pub fn glob(&self, pattern: &str) -> Vec<String> {
        let mut cands = vec![String::new()];
        for comp in pattern.split('/') {
            let mut next = Vec::new();
            for c in &cands {
                let join = |n: &str| if c.is_empty() { n.to_owned() } else { format!("{c}/{n}") };
                if comp.contains(['*', '?']) {
                    next.extend(self.list(c).into_iter().filter(|n| wildcard(comp, n)).map(|n| join(&n)));
                } else {
                    next.push(join(comp));
                }
            }
            cands = next;
        }
        cands.into_iter().filter(|r| self.locate(r).is_some_and(|p| p.is_file())).collect()
    }
}

// -- the record ----------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    #[serde(flatten)]
    key: AppliedKey,
    format: PatchFormat,
    at: chrono::DateTime<chrono::Utc>,
    /// Where the key was, so putting a deleted key back keeps the file's order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anchor: Option<Anchor>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    #[serde(default)]
    keys: Vec<Entry>,
}

fn load(path: &Path) -> Result<Record> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).with_context(|| format!("corrupt {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Record::default()),
        Err(e) => Err(e.into()),
    }
}

fn save(path: &Path, r: &Record) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("record path")?)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(r)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Keys Quiver has changed (all of them), for display.
pub fn applied(record: &Path) -> Result<Vec<AppliedKey>> {
    Ok(load(record)?.keys.into_iter().map(|e| e.key).collect())
}

fn same_key(format: PatchFormat, a: &str, b: &str) -> bool {
    match format {
        PatchFormat::Ini => a.eq_ignore_ascii_case(b),
        PatchFormat::Xml => a == b,
    }
}

fn matches(e: &Entry, scope: &str, file: &str, target: &str, key: &str) -> bool {
    e.key.scope == scope && e.key.file == file && same_key(e.format, &e.key.target, target) && same_key(e.format, &e.key.key, key)
}

/// Read a file as a document.
fn read_doc(path: &Path, format: PatchFormat) -> Result<(Doc, Encoding)> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let (text, enc) = decode(&bytes).with_context(|| path.display().to_string())?;
    Ok((Doc::parse(format, &text), enc))
}

/// Write `doc` to `dst` (atomically), with the permissions of `like` (the file it came
/// from, which is `dst` itself unless this is a copy-up).
fn write_doc(dst: &Path, like: &Path, doc: &Doc, enc: Encoding) -> Result<()> {
    std::fs::create_dir_all(dst.parent().context("file path")?)?;
    let tmp = dst.with_file_name(format!(".{}.quiver-tmp", dst.file_name().unwrap_or_default().to_string_lossy()));
    std::fs::write(&tmp, encode(&doc.render(), enc))?;
    if let Ok(m) = like.metadata() {
        let _ = std::fs::set_permissions(&tmp, m.permissions());
    }
    std::fs::rename(&tmp, dst).with_context(|| format!("writing {}", dst.display()))?;
    Ok(())
}

/// One key edit a stack wants, and the mod it comes from.
#[derive(Debug, Clone)]
struct Want {
    target: String,
    key: String,
    /// `None`: delete.
    value: Option<String>,
    mods: Vec<String>,
    /// Patch statuses (indexes) this edit belongs to.
    statuses: Vec<usize>,
    format: PatchFormat,
}

/// A config patch a launch wants, from a mod (store id), in stack order.
#[derive(Debug, Clone)]
pub struct Wanted {
    pub mod_id: String,
    pub patch: ConfigPatch,
}

/// Bring the files `view` shows in line with `wanted` (see the module docs), recording
/// what changed in `record`. Returns one status per wanted patch, then one per recorded
/// key that was undone. Errors only for a broken record; problems with single files are
/// statuses.
pub fn reconcile(record: &Path, view: &View, wanted: &[Wanted]) -> Result<Vec<PatchStatus>> {
    let mut rec = load(record)?;
    if rec.keys.is_empty() && wanted.is_empty() {
        return Ok(Vec::new());
    }
    let mut statuses: Vec<PatchStatus> = Vec::new();
    // file -> wanted edits, in stack order (a later mod overrides an earlier one).
    let mut files: BTreeMap<String, Vec<Want>> = BTreeMap::new();
    for w in wanted {
        let p = &w.patch;
        let idx = statuses.len();
        statuses.push(PatchStatus { mod_id: w.mod_id.clone(), file: p.file.clone(), target: p.target().to_owned(), ..Default::default() });
        if let Err(e) = validate(p) {
            statuses[idx].state = PatchState::Failed;
            statuses[idx].detail = format!("{e:#}");
            continue;
        }
        let found = view.glob(&p.file);
        if found.is_empty() {
            statuses[idx].state = PatchState::Pending;
            statuses[idx].detail = "pending until first run: the game hasn't written this file yet".into();
            continue;
        }
        for rel in found {
            statuses[idx].files.push(PatchedFile { rel: rel.clone(), path: view.write_path(&rel), scope: view.scope(&rel) });
            let edits = files.entry(rel).or_default();
            let ops = p.set.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).chain(p.delete.iter().map(|k| (k.clone(), None)));
            for (key, value) in ops {
                match edits.iter_mut().find(|e| e.format == p.format && same_key(p.format, &e.target, p.target()) && same_key(p.format, &e.key, &key)) {
                    Some(e) => {
                        e.value = value;
                        e.mods.push(w.mod_id.clone());
                        e.statuses.push(idx);
                    }
                    None => edits.push(Want {
                        target: p.target().to_owned(),
                        key,
                        value,
                        mods: vec![w.mod_id.clone()],
                        statuses: vec![idx],
                        format: p.format,
                    }),
                }
            }
        }
    }
    // Recorded keys in files this launch sees, wanted or not.
    for e in &rec.keys {
        if view.scope(&e.key.file) == e.key.scope {
            files.entry(e.key.file.clone()).or_default();
        }
    }
    let mut changed_status = vec![false; statuses.len()];
    let mut failed: BTreeMap<usize, String> = BTreeMap::new();
    for (rel, wants) in files {
        let scope = view.scope(&rel);
        let recorded: Vec<usize> = (0..rec.keys.len()).filter(|&i| rec.keys[i].key.scope == scope && rec.keys[i].key.file == rel).collect();
        let Some(format) = wants.first().map(|w| w.format).or_else(|| recorded.first().map(|&i| rec.keys[i].format)) else { continue };
        if wants.iter().any(|w| w.format != format) {
            for w in &wants {
                for &s in &w.statuses {
                    failed.insert(s, format!("{rel}: patched both as INI and as XML"));
                }
            }
            continue;
        }
        let Some(src) = view.locate(&rel) else {
            // Gone (the game deleted it): what was recorded no longer applies.
            rec.keys.retain(|e| !(e.key.scope == scope && e.key.file == rel));
            continue;
        };
        let (mut doc, enc) = match read_doc(&src, format) {
            Ok(d) => d,
            Err(e) => {
                for w in &wants {
                    for &s in &w.statuses {
                        failed.insert(s, format!("{e:#}"));
                    }
                }
                continue;
            }
        };
        let mut changed = false;
        let mut keep = vec![false; recorded.len()];
        for w in &wants {
            let cur = match doc.get(&w.target, &w.key) {
                Ok(c) => c,
                Err(e) => {
                    for &s in &w.statuses {
                        failed.insert(s, format!("{rel}: {e:#}"));
                    }
                    continue;
                }
            };
            let want: Vec<String> = w.value.iter().cloned().collect();
            let pos = recorded.iter().position(|&i| matches(&rec.keys[i], &scope, &rel, &w.target, &w.key));
            match pos {
                Some(n) => {
                    keep[n] = true;
                    let e = &mut rec.keys[recorded[n]];
                    e.key.applied = w.value.clone();
                    e.key.mods = w.mods.clone();
                }
                None => {
                    // The file's own value; for a profile's copy, what the prefix had before
                    // Quiver if it changed that too.
                    let inherited = (scope != "prefix")
                        .then(|| {
                            rec.keys
                                .iter()
                                .find(|e| matches(e, "prefix", &rel, &w.target, &w.key))
                                .map(|e| (e.key.original.clone(), e.anchor.clone()))
                        })
                        .flatten();
                    let (original, anchor) = inherited.unwrap_or_else(|| (cur.clone(), doc.anchor(&w.target, &w.key).ok().flatten()));
                    rec.keys.push(Entry {
                        key: AppliedKey {
                            scope: scope.clone(),
                            file: rel.clone(),
                            target: w.target.clone(),
                            key: w.key.clone(),
                            original,
                            applied: w.value.clone(),
                            mods: w.mods.clone(),
                        },
                        format,
                        at: chrono::Utc::now(),
                        anchor,
                    });
                }
            }
            if cur != want {
                if let Err(e) = doc.set(&w.target, &w.key, &want, None) {
                    for &s in &w.statuses {
                        failed.insert(s, format!("{rel}: {e:#}"));
                    }
                    continue;
                }
                changed = true;
                for &s in &w.statuses {
                    changed_status[s] = true;
                }
            }
        }
        // Recorded keys no mod here wants: back to the file's own values.
        let mut drop = Vec::new();
        for (n, &i) in recorded.iter().enumerate() {
            if keep[n] {
                continue;
            }
            let e = &rec.keys[i];
            let applied: Vec<String> = e.key.applied.iter().cloned().collect();
            let mut st = PatchStatus {
                mod_id: e.key.mods.join(","),
                file: rel.clone(),
                target: e.key.target.clone(),
                state: PatchState::Undone,
                files: vec![PatchedFile { rel: rel.clone(), path: view.write_path(&rel), scope: scope.clone() }],
                detail: format!("{}: back to {}", e.key.key, show_values(&e.key.original)),
            };
            match doc.get(&e.key.target, &e.key.key) {
                Ok(cur) if cur == applied => match doc.set(&e.key.target, &e.key.key, &e.key.original, e.anchor.as_ref()) {
                    Ok(()) => changed |= cur != e.key.original,
                    Err(err) => {
                        st.state = PatchState::Failed;
                        st.detail = format!("undoing {}: {err:#}", e.key.key);
                    }
                },
                Ok(_) => st.detail = format!("{}: changed since Quiver set it; left as it is", e.key.key),
                Err(err) => st.detail = format!("{}: {err:#}; nothing to undo", e.key.key),
            }
            statuses.push(st);
            changed_status.push(false);
            drop.push(i);
        }
        if changed && let Err(e) = write_doc(&view.write_path(&rel), &src, &doc, enc) {
            for w in &wants {
                for &s in &w.statuses {
                    failed.insert(s, format!("{e:#}"));
                }
            }
            // Nothing was written: forget what this launch would have recorded anew.
            continue;
        }
        drop.sort_unstable();
        for i in drop.into_iter().rev() {
            rec.keys.remove(i);
        }
    }
    for (i, st) in statuses.iter_mut().enumerate() {
        if let Some(e) = failed.get(&i) {
            st.state = PatchState::Failed;
            st.detail = e.clone();
        } else if st.state == PatchState::Unknown {
            st.state = if changed_status[i] { PatchState::Applied } else { PatchState::Unchanged };
        }
    }
    save(record, &rec)?;
    Ok(statuses)
}

fn show_values(v: &[String]) -> String {
    match v {
        [] => "(not set)".into(),
        [one] => format!("{one:?}"),
        many => format!("{many:?}"),
    }
}

/// Put back the file's own values for recorded keys, now: all of them, or those wanted
/// only by `mod_id` (a key other mods want too just loses that mod). `physical` maps a
/// scope and prefix-relative file to the real file. Keys changed since Quiver set them
/// are left as they are.
pub fn undo(record: &Path, mod_id: Option<&str>, physical: impl Fn(&str, &str) -> Option<PathBuf>) -> Result<Vec<PatchStatus>> {
    let mut rec = load(record)?;
    let mut out = Vec::new();
    let mut by_file: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for (i, e) in rec.keys.iter_mut().enumerate() {
        match mod_id {
            Some(m) if !e.key.mods.iter().any(|x| x == m) => continue,
            Some(m) if e.key.mods.len() > 1 => {
                e.key.mods.retain(|x| x != m);
                continue;
            }
            _ => {}
        }
        by_file.entry((e.key.scope.clone(), e.key.file.clone())).or_default().push(i);
    }
    let mut remove = Vec::new();
    for ((scope, file), idx) in by_file {
        remove.extend(idx.iter().copied());
        let Some(path) = physical(&scope, &file).filter(|p| p.is_file()) else { continue };
        let format = rec.keys[idx[0]].format;
        let (mut doc, enc) = match read_doc(&path, format) {
            Ok(d) => d,
            Err(e) => {
                out.push(PatchStatus { file: file.clone(), state: PatchState::Failed, detail: format!("{e:#}"), ..Default::default() });
                continue;
            }
        };
        let mut changed = false;
        for &i in &idx {
            let anchor = rec.keys[i].anchor.clone();
            let e = &rec.keys[i].key;
            let applied: Vec<String> = e.applied.iter().cloned().collect();
            let mut st = PatchStatus {
                mod_id: e.mods.join(","),
                file: file.clone(),
                target: e.target.clone(),
                state: PatchState::Undone,
                files: vec![PatchedFile { rel: file.clone(), path: path.clone(), scope: scope.clone() }],
                detail: format!("{}: back to {}", e.key, show_values(&e.original)),
            };
            match doc.get(&e.target, &e.key) {
                Ok(cur) if cur == applied => {
                    if doc.set(&e.target, &e.key, &e.original, anchor.as_ref()).is_ok() {
                        changed |= cur != e.original;
                    }
                }
                _ => st.detail = format!("{}: changed since Quiver set it; left as it is", e.key),
            }
            out.push(st);
        }
        if changed {
            write_doc(&path, &path, &doc, enc)?;
        }
    }
    remove.sort_unstable();
    for i in remove.into_iter().rev() {
        rec.keys.remove(i);
    }
    save(record, &rec)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/mods-v2");

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{FIX}/{name}")).unwrap()
    }

    fn v(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ini_edits_keep_everything_else() {
        let text = fixture("PCConsole-ACCEngine.ini");
        let mut ini = Ini::parse(&text);
        assert_eq!(ini.render(), text, "parse and render round-trip byte for byte");
        let movies = ini.get("FullScreenMovie", "StartupMovies");
        assert_eq!(movies.len(), 4);
        assert_eq!(ini.get("fullscreenmovie", "startupmovies"), movies, "no case");
        // Delete every StartupMovies line; nothing else changes.
        let anchor = ini.anchor("FullScreenMovie", "StartupMovies").unwrap();
        assert_eq!(anchor.after.as_deref(), Some("bForceNoMovies"));
        ini.set("FullScreenMovie", "StartupMovies", &[]);
        let out = ini.render();
        assert!(!out.contains("StartupMovies="));
        let removed: Vec<&str> = text.split("\r\n").filter(|l| l.starts_with("StartupMovies=")).collect();
        let mut expect = text.clone();
        for l in removed {
            expect = expect.replacen(&format!("{l}\r\n"), "", 1);
        }
        assert_eq!(out, expect);
        // Restoring at the anchor puts them back in place, in order.
        ini.set_at("FullScreenMovie", "StartupMovies", &movies, Some(&anchor));
        assert_eq!(ini.render(), text);
        // A key that was first in its section goes back first.
        let mut first = Ini::parse("[S]\nA=1\nB=2\n");
        let a = first.anchor("S", "A").unwrap();
        assert_eq!(a.after, None);
        first.set("S", "A", &[]);
        first.set_at("S", "A", &v(&["1"]), Some(&a));
        assert_eq!(first.render(), "[S]\nA=1\nB=2\n");
        // Setting keeps CRLF and the line's own spacing.
        let mut ini = Ini::parse("; comment\r\n[A]\r\nKey = 1\r\nOther=2\r\n\r\n[B]\r\nx=1\r\n");
        ini.set("a", "KEY", &v(&["5"]));
        assert_eq!(ini.render(), "; comment\r\n[A]\r\nKey = 5\r\nOther=2\r\n\r\n[B]\r\nx=1\r\n");
        // New keys go after the section's last entry, before the blank line.
        ini.set("A", "New", &v(&["n"]));
        assert_eq!(ini.render(), "; comment\r\n[A]\r\nKey = 5\r\nOther=2\r\nNew=n\r\n\r\n[B]\r\nx=1\r\n");
        // A missing section is added at the end; the file's newline style is used.
        ini.set("C", "y", &v(&["2"]));
        assert!(ini.render().ends_with("x=1\r\n\r\n[C]\r\ny=2\r\n"), "{:?}", ini.render());
        // Keys before the first section.
        let mut ini = Ini::parse("top=1\n[S]\nk=v");
        assert_eq!(ini.get("", "top"), ["1"]);
        ini.set("S", "k2", &v(&["w"]));
        assert_eq!(ini.render(), "top=1\n[S]\nk=v\nk2=w\n");
        ini.set("", "top2", &v(&["2"]));
        assert_eq!(ini.render(), "top=1\ntop2=2\n[S]\nk=v\nk2=w\n");
        // Comments and `[` lines are never keys.
        let ini = Ini::parse("[S]\n;k=1\n#k=2\nk=3\n");
        assert_eq!(ini.get("S", "k"), ["3"]);
    }

    #[test]
    fn xml_attribute_edits() {
        let text = fixture("GamerProfile.xml");
        let mut x = Xml::parse(&text);
        let el = "GamerProfile/ProfileSpecificGameProfile";
        assert_eq!(x.get(el, "MouseAcceleration").unwrap(), ["1"]);
        assert_eq!(x.get(el, "Smoothness_Ironsight").unwrap(), ["1"]);
        assert!(x.get(el, "Nope").unwrap().is_empty());
        for k in ["MouseAcceleration", "Smoothness", "Smoothness_Ironsight"] {
            x.set(el, k, &v(&["0"])).unwrap();
        }
        let out = x.render();
        // Exactly the three values changed; same length, same everything else.
        assert_eq!(out.len(), text.len());
        let diff = out.bytes().zip(text.bytes()).filter(|(a, b)| a != b).count();
        assert_eq!(diff, 3);
        assert!(out.contains(r#" MouseAcceleration="0" MouseAccelerationOn="0""#));
        assert!(out.contains(r#" Smoothness="0" Smoothness_Ironsight="0" "#));
        // A child element with the same attribute name isn't touched.
        assert_eq!(Xml::parse(&out).get("GamerProfile/RenderProfile", "Version").unwrap(), x.get("GamerProfile/RenderProfile", "Version").unwrap());
        // Add and remove attributes; escaping both ways.
        x.set(el, "Added", &v(&["a<b & \"c\""])).unwrap();
        assert_eq!(x.get(el, "Added").unwrap(), ["a<b & \"c\""]);
        assert!(x.render().contains(r#" Added="a&lt;b &amp; &quot;c&quot;">"#));
        x.set(el, "Added", &[]).unwrap();
        assert_eq!(x.render(), out);
        // Removing and putting back at the anchor keeps the attribute order.
        let a = x.anchor(el, "Smoothness").unwrap().unwrap();
        assert_eq!(a.after.as_deref(), Some("UseMouseSmooth"));
        x.set(el, "Smoothness", &[]).unwrap();
        x.set_at(el, "Smoothness", &v(&["0"]), Some(&a)).unwrap();
        assert_eq!(x.render(), out);
        assert!(x.set("GamerProfile/Missing", "a", &v(&["1"])).unwrap_err().to_string().contains("no element"));
        // Self-closing elements, comments, declarations, single quotes.
        let mut x = Xml::parse("<?xml version='1.0'?>\n<!-- <Root a='x'/> -->\n<Root><Child k='1'/><Child k='2'/></Root>");
        assert_eq!(x.get("Root/Child", "k").unwrap(), ["1"]);
        x.set("Root/Child", "k", &v(&["it's"])).unwrap();
        assert!(x.render().contains("<Child k='it&apos;s'/><Child k='2'/>"));
        x.set("Root/Child", "n", &v(&["1"])).unwrap();
        assert!(x.render().contains(r#"<Child k='it&apos;s' n="1"/>"#));
        x.set("Root", "top", &v(&["t"])).unwrap();
        assert!(x.render().contains(r#"<Root top="t"><Child"#));
    }

    #[test]
    fn encodings_round_trip() {
        for text in ["[S]\r\nk=1\r\n", "<a b=\"1\"/>"] {
            for enc in [Encoding::Utf8 { bom: false }, Encoding::Utf8 { bom: true }, Encoding::Utf16 { le: true }, Encoding::Utf16 { le: false }] {
                let bytes = encode(text, enc);
                assert_eq!(decode(&bytes).unwrap(), (text.to_owned(), enc));
            }
        }
        assert!(decode(&[0xff, 0x00, 0xc3]).is_err());
    }

    #[test]
    fn wildcards() {
        assert!(wildcard("*", "abc") && wildcard("a*c", "abbc") && wildcard("a?c", "abc") && wildcard("*.xml", "x.xml"));
        assert!(!wildcard("a*d", "abc") && !wildcard("?", ""));
    }

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    fn patch(file: &str, format: PatchFormat, target: &str, set: &[(&str, &str)], delete: &[&str]) -> ConfigPatch {
        let mut p = ConfigPatch { file: file.into(), format, ..Default::default() };
        match format {
            PatchFormat::Ini => p.section = target.into(),
            PatchFormat::Xml => p.element = target.into(),
        }
        p.set = set.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        p.delete = v(delete);
        p
    }

    #[test]
    fn apply_is_idempotent_and_undoes() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let prefix = t.join("pfx");
        let record = t.join("patches.json");
        let gp = "drive_c/users/steamuser/Documents/My Games/Far Cry 4/USER-ID/GamerProfile.xml";
        let ini = "drive_c/users/steamuser/Documents/My Games/UnrealEngine3/ACCGame/ACCGame/Config/PCConsole-ACCEngine.ini";
        let gp_text = fixture("GamerProfile.xml");
        let ini_text = fixture("PCConsole-ACCEngine.ini");
        write(&prefix.join(gp), &gp_text);
        write(&prefix.join(ini), &ini_text);
        let view = View::prefix_only(&prefix);
        let mouse = Wanted {
            mod_id: "m-mouse".into(),
            patch: patch(
                "drive_c/users/steamuser/Documents/My Games/Far Cry 4/*/GamerProfile.xml",
                PatchFormat::Xml,
                "GamerProfile/ProfileSpecificGameProfile",
                &[("MouseAcceleration", "0"), ("Smoothness", "0"), ("Smoothness_Ironsight", "0")],
                &[],
            ),
        };
        let intro = Wanted { mod_id: "m-intro".into(), patch: patch(ini, PatchFormat::Ini, "FullScreenMovie", &[], &["StartupMovies"]) };
        let later = Wanted {
            mod_id: "m-pending".into(),
            patch: patch("drive_c/users/steamuser/Documents/My Games/Nope/x.ini", PatchFormat::Ini, "S", &[("k", "v")], &[]),
        };
        let st = reconcile(&record, &view, &[mouse.clone(), intro.clone(), later.clone()]).unwrap();
        let states: Vec<PatchState> = st.iter().map(|s| s.state).collect();
        assert_eq!(states, [PatchState::Applied, PatchState::Applied, PatchState::Pending], "{st:#?}");
        assert!(st[2].detail.contains("pending until first run"));
        assert_eq!(st[0].files[0].rel, gp, "the per-user folder is matched");
        let patched_gp = std::fs::read_to_string(prefix.join(gp)).unwrap();
        assert!(patched_gp.contains(r#"MouseAcceleration="0""#) && patched_gp.contains(r#"Smoothness="0" Smoothness_Ironsight="0""#));
        assert!(!std::fs::read_to_string(prefix.join(ini)).unwrap().contains("StartupMovies="));
        // Again: nothing to do (and the files aren't rewritten).
        let mtime = std::fs::metadata(prefix.join(gp)).unwrap().modified().unwrap();
        let st = reconcile(&record, &view, &[mouse.clone(), intro.clone(), later.clone()]).unwrap();
        assert_eq!(st.iter().map(|s| s.state).collect::<Vec<_>>(), [PatchState::Unchanged, PatchState::Unchanged, PatchState::Pending]);
        assert_eq!(std::fs::metadata(prefix.join(gp)).unwrap().modified().unwrap(), mtime);
        // The game rewrites the file: the next launch applies the patch again.
        write(&prefix.join(gp), &gp_text);
        let st = reconcile(&record, &view, &[mouse.clone(), intro.clone()]).unwrap();
        assert_eq!(st[0].state, PatchState::Applied);
        // The originals are what the file had before Quiver, not the values re-applied.
        let rec = applied(&record).unwrap();
        let movies = rec.iter().find(|k| k.key == "StartupMovies").unwrap();
        assert_eq!(movies.original.len(), 4);
        assert_eq!(movies.applied, None);
        assert_eq!(rec.iter().find(|k| k.key == "MouseAcceleration").unwrap().original, ["1"]);

        // The mouse mod leaves the stack: its keys go back, byte for byte.
        let st = reconcile(&record, &view, &[intro.clone()]).unwrap();
        assert_eq!(st[0].state, PatchState::Unchanged);
        assert_eq!(st.iter().filter(|s| s.state == PatchState::Undone).count(), 3, "{st:#?}");
        assert_eq!(std::fs::read_to_string(prefix.join(gp)).unwrap(), gp_text);
        // A key the person changed since isn't put back.
        let edited = std::fs::read_to_string(prefix.join(ini)).unwrap().replace("[FullScreenMovie]\r\n", "[FullScreenMovie]\r\nStartupMovies=Mine\r\n");
        write(&prefix.join(ini), &edited);
        // (the launch with the mod deletes it again…)
        reconcile(&record, &view, &[intro.clone()]).unwrap();
        assert!(!std::fs::read_to_string(prefix.join(ini)).unwrap().contains("StartupMovies="));
        // …and without it, restores the originals.
        let st = reconcile(&record, &view, &[]).unwrap();
        assert_eq!(st.len(), 1);
        assert_eq!(std::fs::read_to_string(prefix.join(ini)).unwrap(), ini_text);
        assert!(applied(&record).unwrap().is_empty());
        // Changed since: left alone, and forgotten.
        reconcile(&record, &view, &[intro.clone()]).unwrap();
        write(&prefix.join(ini), &edited);
        let st = reconcile(&record, &view, &[]).unwrap();
        assert!(st[0].detail.contains("left as it is"), "{st:?}");
        assert_eq!(std::fs::read_to_string(prefix.join(ini)).unwrap(), edited);
        assert!(applied(&record).unwrap().is_empty());
    }

    #[test]
    fn later_mods_win_and_undo_by_mod() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let prefix = t.join("pfx");
        let record = t.join("patches.json");
        let f = "drive_c/c.ini";
        write(&prefix.join(f), "[S]\nA=1\nB=1\n");
        let view = View::prefix_only(&prefix);
        let w = |m: &str, set: &[(&str, &str)]| Wanted { mod_id: m.into(), patch: patch(f, PatchFormat::Ini, "S", set, &[]) };
        reconcile(&record, &view, &[w("x", &[("A", "2"), ("B", "2")]), w("y", &[("A", "3")])]).unwrap();
        assert_eq!(std::fs::read_to_string(prefix.join(f)).unwrap(), "[S]\nA=3\nB=2\n");
        // Undo y only: A is still wanted by x, so it stays until the next launch.
        undo(&record, Some("y"), |_, rel| Some(prefix.join(rel))).unwrap();
        assert_eq!(applied(&record).unwrap().iter().find(|k| k.key == "A").unwrap().mods, ["x"]);
        // Undo everything.
        let st = undo(&record, None, |_, rel| Some(prefix.join(rel))).unwrap();
        assert_eq!(st.len(), 2);
        assert_eq!(std::fs::read_to_string(prefix.join(f)).unwrap(), "[S]\nA=1\nB=1\n");
        // Bad patches are reported, not applied.
        let bad = Wanted { mod_id: "z".into(), patch: patch("../etc/passwd", PatchFormat::Ini, "S", &[("a", "b")], &[]) };
        assert_eq!(reconcile(&record, &view, &[bad]).unwrap()[0].state, PatchState::Failed);
    }

    #[test]
    fn profile_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let prefix = t.join("pfx");
        let base = t.join("profiles/p1/prefix");
        let record = t.join("patches.json");
        let dir = "drive_c/users/steamuser/Documents/My Game";
        let f = format!("{dir}/settings.ini");
        write(&prefix.join(&f), "[G]\nAA=1\n");
        let view = View { prefix: prefix.clone(), profile: Some(("p1".into(), base.clone(), vec![dir.into()])) };
        assert_eq!(view.scope(&f), "profile:p1");
        assert_eq!(view.scope("drive_c/other.ini"), "prefix");
        let w = Wanted { mod_id: "m".into(), patch: patch(&f, PatchFormat::Ini, "G", &[("AA", "16")], &[]) };
        let st = reconcile(&record, &view, std::slice::from_ref(&w)).unwrap();
        assert_eq!(st[0].state, PatchState::Applied);
        assert_eq!(st[0].files[0].path, base.join(&f));
        // Copied up: the profile's copy has the patch, the prefix keeps its own.
        assert_eq!(std::fs::read_to_string(base.join(&f)).unwrap(), "[G]\nAA=16\n");
        assert_eq!(std::fs::read_to_string(prefix.join(&f)).unwrap(), "[G]\nAA=1\n");
        // Vanilla sees the prefix's file and has nothing to undo there.
        let st = reconcile(&record, &View::prefix_only(&prefix), &[]).unwrap();
        assert!(st.is_empty());
        assert_eq!(applied(&record).unwrap().len(), 1);
        // A whiteout in the profile's copy hides the file.
        std::fs::remove_file(base.join(&f)).unwrap();
        let c = std::ffi::CString::new(base.join(&f).as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mknod(c.as_ptr(), libc::S_IFCHR | 0o600, 0) }, 0);
        assert!(view.locate(&f).is_none());
        assert_eq!(reconcile(&record, &view, &[w]).unwrap()[0].state, PatchState::Pending);
    }
}
