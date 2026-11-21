//! The first lines on a control connection, after TLS (design/REMOTE.md §4).
//!
//! The client says what it wants in a [`Hello`]; the server answers with an [`Answer`]:
//!
//! - `api`: a paired client's remote API. After `{"ok": …}` the connection carries the
//!   quiverd protocol (`quiver-api`: one JSON request per line, replies and events back).
//! - `ping`: a paired client checking the machine is up. Answered, then closed.
//! - `pair`: pairing with a code (`pair`); allowed from unpaired clients while a pairing
//!   window is open.
//!
//! Lines before the API starts are at most [`MAX_LINE`] bytes, so an unpaired client can't
//! make the server hold much.

use std::fmt;

use anyhow::{Context, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// The control plane's version; a server refuses another (code [`code::VERSION`]).
pub const VERSION: u32 = 1;
/// The longest line before the API starts.
pub const MAX_LINE: usize = 16 * 1024;

/// Refusal codes, in [`Answer::code`] and [`Refused::code`].
pub mod code {
    /// The client's key isn't a paired client's (never paired, or revoked).
    pub const NOT_PAIRED: &str = "not_paired";
    /// Pairing asked for while no pairing window is open on the server.
    pub const NO_PAIRING: &str = "no_pairing";
    /// The pairing code was wrong (or a machine in between changed the certificates).
    pub const WRONG_CODE: &str = "wrong_code";
    /// Another control-plane version.
    pub const VERSION: &str = "version";
    /// The connection didn't follow the protocol.
    pub const PROTOCOL: &str = "protocol";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Api,
    Ping,
    Pair,
}

/// The client's first line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub quiver_remote: u32,
    pub intent: Intent,
    /// The client machine's name.
    pub name: String,
    /// Pairing: the client's SPAKE2 message (base64).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pake: Option<String>,
}

/// Who answered: the server machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Welcome {
    pub name: String,
    pub id: String,
}

/// The server's answer to a [`Hello`]: `{"ok": Welcome}` or `{"error": …, "code": …}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Answer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<Welcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl Answer {
    pub fn refuse(code: &str, error: impl Into<String>) -> Answer {
        Answer { ok: None, error: Some(error.into()), code: Some(code.to_owned()) }
    }
}

/// The other side refused (an [`Answer`] or a pairing line with an `error`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub code: String,
    pub error: String,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.error)
    }
}

impl std::error::Error for Refused {}

/// The refusal code in an error chain, if the other side refused.
pub fn refused_code(e: &anyhow::Error) -> Option<&str> {
    e.chain().find_map(|c| c.downcast_ref::<Refused>()).map(|r| r.code.as_str())
}

/// Write one JSON line and flush it (TLS buffers).
pub async fn send<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await?;
    Ok(())
}

/// Read one line of at most [`MAX_LINE`] bytes. `None` at the end of the stream.
pub async fn read_line<R: AsyncBufRead + Unpin>(r: &mut R) -> Result<Option<String>> {
    let mut out = Vec::new();
    loop {
        let buf = r.fill_buf().await?;
        if buf.is_empty() {
            return if out.is_empty() { Ok(None) } else { bail!("connection closed mid-line") };
        }
        match buf.iter().position(|&b| b == b'\n') {
            Some(i) => {
                out.extend_from_slice(&buf[..i]);
                r.consume(i + 1);
                break;
            }
            None => {
                let n = buf.len();
                out.extend_from_slice(buf);
                r.consume(n);
            }
        }
        if out.len() > MAX_LINE {
            bail!("line too long");
        }
    }
    if out.len() > MAX_LINE {
        bail!("line too long");
    }
    Ok(Some(String::from_utf8(out).context("not UTF-8")?))
}

/// Read one JSON line as `T`; a line with an `error` (and `code`) is a [`Refused`].
pub async fn recv<R: AsyncBufRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<T> {
    let line = read_line(r).await?.ok_or_else(|| anyhow!("the other side closed the connection"))?;
    let v: serde_json::Value = serde_json::from_str(&line).context("not JSON")?;
    if let Some(error) = v.get("error").and_then(|e| e.as_str()) {
        let code = v.get("code").and_then(|c| c.as_str()).unwrap_or(code::PROTOCOL).to_owned();
        return Err(Refused { code, error: error.to_owned() }.into());
    }
    serde_json::from_value(v).context("unexpected message")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lines_are_limited() {
        let long = vec![b'a'; MAX_LINE + 10];
        let mut r = tokio::io::BufReader::with_capacity(64, &long[..]);
        assert!(read_line(&mut r).await.is_err());
        let mut r = tokio::io::BufReader::new(&b"{\"error\":\"no\",\"code\":\"not_paired\"}\nnext\n"[..]);
        let e = recv::<_, Welcome>(&mut r).await.unwrap_err();
        assert_eq!(refused_code(&e), Some(code::NOT_PAIRED));
        assert_eq!(read_line(&mut r).await.unwrap().as_deref(), Some("next"));
        assert_eq!(read_line(&mut r).await.unwrap(), None);
    }
}
