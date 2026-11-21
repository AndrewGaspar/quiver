//! Just enough HTTP/1.1 for Moonlight's GET requests: read a request head, answer, close.

use anyhow::{Result, anyhow, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Moonlight's longest request is stage 1 of pairing (the client certificate, hex-encoded
/// PEM: about 2.5 KB).
const MAX_HEAD: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub path: String,
    pub query: Vec<(String, String)>,
}

impl HttpRequest {
    pub fn arg(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// Parses a request head (`GET /path?query HTTP/1.1` and headers).
    pub fn parse(head: &str) -> Result<Self> {
        let line = head.lines().next().ok_or_else(|| anyhow!("empty request"))?;
        let mut parts = line.split_whitespace();
        let (Some(method), Some(target)) = (parts.next(), parts.next()) else { bail!("bad request line {line:?}") };
        if method != "GET" {
            bail!("{method} isn't supported");
        }
        let (path, q) = target.split_once('?').unwrap_or((target, ""));
        let query = q
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                (percent_decode(k), percent_decode(v))
            })
            .collect();
        Ok(Self { path: path.to_owned(), query })
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = b
                    .get(i + 1..i + 3)
                    .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
                    .and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
                match hex {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reads a request head from `io`.
pub async fn read_request<S: AsyncRead + Unpin>(io: &mut S) -> Result<HttpRequest> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = io.read(&mut chunk).await?;
        if n == 0 {
            bail!("connection closed before the request ended");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = std::str::from_utf8(&buf[..end]).map_err(|_| anyhow!("request head isn't UTF-8"))?;
            return HttpRequest::parse(head);
        }
        if buf.len() > MAX_HEAD {
            bail!("request head over {MAX_HEAD} bytes");
        }
    }
}

/// Writes a whole response and shuts the stream down.
pub async fn write_response<S: AsyncWrite + Unpin>(io: &mut S, status: u16, content_type: &str, body: &[u8]) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    io.write_all(head.as_bytes()).await?;
    io.write_all(body).await?;
    io.flush().await?;
    let _ = io.shutdown().await;
    Ok(())
}

/// GameStream's XML answers: `<root status_code="…">` with child elements.
pub fn xml(status: u32, message: Option<&str>, fields: &[(&str, String)]) -> String {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"");
    s.push_str(&status.to_string());
    s.push('"');
    if let Some(m) = message {
        s.push_str(" status_message=\"");
        s.push_str(&escape(m));
        s.push('"');
    }
    s.push('>');
    for (k, v) in fields {
        s.push_str(&format!("<{k}>{}</{k}>", escape(v)));
    }
    s.push_str("</root>\n");
    s
}

pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_moonlight_request() {
        let r = HttpRequest::parse(
            "GET /pair?uniqueid=0123456789ABCDEF&uuid=ab12&devicename=roth&updateState=1&phrase=getservercert&salt=00ff HTTP/1.1\r\nHost: 127.0.0.1:47989\r\n",
        )
        .unwrap();
        assert_eq!(r.path, "/pair");
        assert_eq!(r.arg("phrase"), Some("getservercert"));
        assert_eq!(r.arg("salt"), Some("00ff"));
        assert_eq!(r.arg("nope"), None);
        assert!(HttpRequest::parse("POST /x HTTP/1.1").is_err());
        assert_eq!(HttpRequest::parse("GET /serverinfo HTTP/1.1").unwrap().query, vec![]);
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%20b+c%2"), "a b c%2");
        assert_eq!(percent_decode("%41%42"), "AB");
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn xml_answers() {
        assert_eq!(
            xml(200, None, &[("paired", "1".into()), ("hostname", "a<b".into())]),
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\"><paired>1</paired><hostname>a&lt;b</hostname></root>\n"
        );
        assert!(xml(401, Some("not \"paired\""), &[]).contains("status_message=\"not &quot;paired&quot;\""));
    }
}
