//! The bridge's side of the mod SDK channel: one mod connection on `QUIVER_MODS_PIPE`
//! relayed to quiverd over a broker connection (`BrokerRequest::ModChannel`).
//!
//! Written against `AsyncRead + AsyncWrite` rather than named pipes so it is tested on the
//! host; the bridge plugs in its `NamedPipeServer`. The game process is untrusted, so the
//! relay checks every frame ([`quiver_proto::modsdk::check_frame`]), caps sizes and rates, and
//! forwards only re-serialized JSON: whatever reaches quiverd is one well-formed object per
//! line. Semantic validation (ids, descriptors, values) is quiverd's.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::time::Instant;
use quiver_proto::modsdk::{self, check_frame};

use crate::{BrokerRequest, BrokerResponse};

#[derive(Debug, Clone)]
pub struct Limits {
    /// Largest frame from the mod; a larger one closes the connection.
    pub frame: usize,
    /// Largest line from quiverd.
    pub line: usize,
    /// How long a new connection has to send `hello`.
    pub hello_timeout: Duration,
    /// Frames per second forwarded from the mod (token bucket); excess frames are dropped.
    pub rate: u32,
    pub burst: u32,
    /// Malformed frames tolerated before the connection is closed.
    pub max_invalid: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            frame: modsdk::MAX_FRAME,
            line: modsdk::MAX_LINE,
            hello_timeout: Duration::from_secs(10),
            // The SDK sends `m` at most 10 Hz; marks, logs, acks and declarations come on top.
            rate: 50,
            burst: 100,
            max_invalid: 64,
        }
    }
}

/// What happened on one relayed connection, for the bridge's log.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    /// The mod's surface id from `hello`, if it got that far.
    pub surface: Option<String>,
    /// Frames forwarded mod -> quiverd and quiverd -> mod.
    pub up: u64,
    pub down: u64,
    pub dropped_invalid: u64,
    pub dropped_rate: u64,
    /// Why it ended.
    pub end: String,
}

/// Read one pipe frame: little-endian length, then the body (at most `max` bytes).
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, max: usize) -> io::Result<Vec<u8>> {
    let len = r.read_u32_le().await? as usize;
    if len > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{len}-byte frame (limit {max})")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(body)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) -> io::Result<()> {
    w.write_all(&modsdk::encode_frame(body)).await?;
    w.flush().await
}

/// Read one `\n`-terminated line of at most `max` bytes (without the newline). `None` at a
/// clean end of stream; an error for an over-long line.
pub async fn read_line_capped<R: AsyncBufRead + Unpin>(r: &mut R, max: usize) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let buf = r.fill_buf().await?;
        if buf.is_empty() {
            return if line.is_empty() { Ok(None) } else { Err(io::ErrorKind::UnexpectedEof.into()) };
        }
        if let Some(i) = buf.iter().position(|&b| b == b'\n') {
            if line.len() + i > max {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("line over {max} bytes")));
            }
            line.extend_from_slice(&buf[..i]);
            r.consume(i + 1);
            return Ok(Some(line));
        }
        let n = buf.len();
        if line.len() + n > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("line over {max} bytes")));
        }
        line.extend_from_slice(buf);
        r.consume(n);
    }
}

/// Open a mod channel on a fresh broker connection: send `req` (a
/// [`BrokerRequest::ModChannel`]), wait for `Ok`. The returned reader keeps anything the
/// broker sent after its reply.
pub async fn open_channel<S: AsyncRead + AsyncWrite + Unpin>(stream: S, req: &BrokerRequest) -> io::Result<BufReader<S>> {
    let mut s = BufReader::new(stream);
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    s.get_mut().write_all(&line).await?;
    let reply = read_line_capped(&mut s, 64 * 1024)
        .await?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "broker closed the connection"))?;
    match serde_json::from_slice::<BrokerResponse>(&reply) {
        Ok(BrokerResponse::Ok) => Ok(s),
        Ok(BrokerResponse::Error { message }) => Err(io::Error::other(format!("broker: {message}"))),
        Ok(other) => Err(io::Error::other(format!("unexpected broker reply {other:?}"))),
        Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e)),
    }
}

struct Bucket {
    tokens: f64,
    rate: f64,
    burst: f64,
    last: Instant,
}

impl Bucket {
    fn new(rate: u32, burst: u32) -> Self {
        Self { tokens: f64::from(burst), rate: f64::from(rate), burst: f64::from(burst), last: Instant::now() }
    }

    fn take(&mut self) -> bool {
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.last).as_secs_f64() * self.rate).min(self.burst);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Relay one mod connection until either side closes. `connect` opens the broker side
/// (normally [`open_channel`] on a new TCP connection) once the mod has said `hello`, so a
/// connection that never does costs quiverd nothing.
pub async fn relay<P, B, F, Fut>(pipe: P, connect: F, limits: &Limits) -> Stats
where
    P: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = io::Result<B>>,
{
    let mut stats = Stats::default();
    let (mut pr, mut pw) = tokio::io::split(pipe);
    let hello = match tokio::time::timeout(limits.hello_timeout, read_frame(&mut pr, limits.frame)).await {
        Err(_) => {
            stats.end = "no hello in time".into();
            return stats;
        }
        Ok(Err(e)) => {
            stats.end = format!("closed before hello: {e}");
            return stats;
        }
        Ok(Ok(body)) => body,
    };
    let hello = match check_frame(&hello) {
        Ok((t, v)) if t == "hello" => v,
        Ok((t, _)) => {
            stats.end = format!("first message was `{t}`, not hello");
            return stats;
        }
        Err(e) => {
            stats.end = format!("bad hello: {e}");
            return stats;
        }
    };
    stats.surface = hello.get("mod").and_then(|m| m.as_str()).map(|s| modsdk::sanitize(s, 48));
    let broker = match connect().await {
        Ok(b) => b,
        Err(e) => {
            stats.end = format!("no channel to quiverd: {e}");
            return stats;
        }
    };
    let (br, mut bw) = tokio::io::split(broker);
    let mut br = BufReader::new(br);
    let send_line = |v: &serde_json::Value| {
        let mut line = serde_json::to_vec(v).unwrap_or_default();
        line.push(b'\n');
        line
    };
    if let Err(e) = bw.write_all(&send_line(&hello)).await {
        stats.end = format!("quiverd: {e}");
        return stats;
    }
    stats.up += 1;

    let mut bucket = Bucket::new(limits.rate, limits.burst);
    let (mut up, mut down, mut invalid, mut limited) = (0u64, 0u64, 0u64, 0u64);
    let up_dir = async {
        loop {
            let body = match read_frame(&mut pr, limits.frame).await {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return "the mod closed the pipe".to_owned(),
                Err(e) => return format!("mod: {e}"),
            };
            let (t, v) = match check_frame(&body) {
                Ok(x) => x,
                Err(_) => {
                    invalid += 1;
                    if invalid > limits.max_invalid {
                        return "too many malformed frames".to_owned();
                    }
                    continue;
                }
            };
            if t == "hello" {
                invalid += 1;
                continue;
            }
            if !bucket.take() {
                limited += 1;
                continue;
            }
            if let Err(e) = bw.write_all(&send_line(&v)).await {
                return format!("quiverd: {e}");
            }
            up += 1;
            if t == "bye" {
                return "the mod said bye".to_owned();
            }
        }
    };
    let down_dir = async {
        loop {
            let line = match read_line_capped(&mut br, limits.line).await {
                Ok(Some(l)) => l,
                Ok(None) => return "quiverd closed the channel".to_owned(),
                Err(e) => return format!("quiverd: {e}"),
            };
            // quiverd is trusted, but the SDK only ever gets one compact object per frame.
            let Ok((_, v)) = check_frame(&line) else { continue };
            let body = serde_json::to_vec(&v).unwrap_or_default();
            if body.len() > modsdk::MAX_FRAME_TO_MOD {
                continue;
            }
            if let Err(e) = write_frame(&mut pw, &body).await {
                return format!("mod: {e}");
            }
            down += 1;
        }
    };
    let end = tokio::select! {
        e = up_dir => e,
        e = down_dir => e,
    };
    stats.end = end;
    stats.up += up;
    stats.down = down;
    stats.dropped_invalid = invalid;
    stats.dropped_rate = limited;
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tokio::io::{DuplexStream, duplex};

    async fn send(w: &mut DuplexStream, v: &Value) {
        write_frame(w, &serde_json::to_vec(v).unwrap()).await.unwrap();
    }

    async fn recv(r: &mut DuplexStream) -> Value {
        serde_json::from_slice(&read_frame(r, 1 << 20).await.unwrap()).unwrap()
    }

    /// A fake broker side: the relay's B end, and the test's view of it.
    fn broker_pair() -> (DuplexStream, BufReader<DuplexStream>) {
        let (a, b) = duplex(1 << 16);
        (a, BufReader::new(b))
    }

    async fn line(r: &mut BufReader<DuplexStream>) -> Value {
        serde_json::from_slice(&read_line_capped(r, 1 << 20).await.unwrap().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn relays_both_ways_and_filters() {
        let (mut game, pipe) = duplex(1 << 16);
        let (b_relay, mut quiverd) = broker_pair();
        let task = tokio::spawn(async move { relay(pipe, || async { Ok(b_relay) }, &Limits::default()).await });
        send(&mut game, &json!({"t":"hello","proto":2,"mod":"test-mod","caps":["metrics"]})).await;
        assert_eq!(line(&mut quiverd).await["t"], "hello");
        // Junk is dropped, a second hello too; good frames go through re-serialized.
        write_frame(&mut game, b"not json").await.unwrap();
        write_frame(&mut game, br#"{"no_t":1}"#).await.unwrap();
        send(&mut game, &json!({"t":"hello","proto":2,"mod":"other"})).await;
        send(&mut game, &json!({"t":"m","ts":1,  "v":{"0":1.5}})).await;
        let m = line(&mut quiverd).await;
        assert_eq!(m, json!({"t":"m","ts":1,"v":{"0":1.5}}));
        // quiverd -> mod: framed.
        quiverd.get_mut().write_all(b"{\"t\":\"welcome\",\"proto\":2}\n").await.unwrap();
        assert_eq!(recv(&mut game).await["t"], "welcome");
        quiverd.get_mut().write_all(b"garbage\n{\"t\":\"set\",\"seq\":1,\"values\":{}}\n").await.unwrap();
        assert_eq!(recv(&mut game).await["t"], "set");
        send(&mut game, &json!({"t":"bye"})).await;
        assert_eq!(line(&mut quiverd).await["t"], "bye");
        let stats = task.await.unwrap();
        assert_eq!(stats.surface.as_deref(), Some("test-mod"));
        assert_eq!((stats.up, stats.down, stats.dropped_invalid), (3, 2, 3));
        assert_eq!(stats.end, "the mod said bye");
    }

    #[tokio::test]
    async fn no_hello_no_channel() {
        let (mut game, pipe) = duplex(1 << 16);
        let task = tokio::spawn(async move {
            relay(pipe, || async { Err::<DuplexStream, _>(io::Error::other("must not connect")) }, &Limits::default()).await
        });
        send(&mut game, &json!({"t":"m","v":{}})).await;
        let s = task.await.unwrap();
        assert!(s.end.contains("not hello"), "{s:?}");
        // An oversized frame ends the connection without allocating it.
        let (mut game, pipe) = duplex(1 << 16);
        let task = tokio::spawn(async move { relay(pipe, || async { Ok(broker_pair().0) }, &Limits::default()).await });
        game.write_all(&(u32::MAX).to_le_bytes()).await.unwrap();
        assert!(task.await.unwrap().end.contains("limit"));
    }

    #[tokio::test(start_paused = true)]
    async fn hello_times_out() {
        let (_game, pipe) = duplex(1 << 16);
        let s = relay(pipe, || async { Ok(broker_pair().0) }, &Limits::default()).await;
        assert_eq!(s.end, "no hello in time");
    }

    #[tokio::test]
    async fn rate_limited() {
        let (mut game, pipe) = duplex(1 << 20);
        let (b_relay, mut quiverd) = broker_pair();
        let limits = Limits { rate: 1, burst: 5, ..Limits::default() };
        let task = tokio::spawn(async move { relay(pipe, || async { Ok(b_relay) }, &limits).await });
        send(&mut game, &json!({"t":"hello","proto":2,"mod":"m"})).await;
        for i in 0..20 {
            send(&mut game, &json!({"t":"m","ts":i,"v":{}})).await;
        }
        send(&mut game, &json!({"t":"bye"})).await;
        drop(game);
        let mut seen = 0;
        while let Ok(Some(_)) = read_line_capped(&mut quiverd, 1 << 20).await {
            seen += 1;
        }
        let s = task.await.unwrap();
        // hello + the burst; the rest (and the bye, over budget) dropped.
        assert_eq!(seen, 6, "{s:?}");
        assert_eq!(s.dropped_rate, 16);
    }

    #[tokio::test]
    async fn channel_handshake() {
        let (a, b) = duplex(1 << 16);
        let server = tokio::spawn(async move {
            let mut b = BufReader::new(b);
            let req: BrokerRequest = serde_json::from_slice(&read_line_capped(&mut b, 4096).await.unwrap().unwrap()).unwrap();
            assert!(matches!(req, BrokerRequest::ModChannel { uplay_id: 7, conn: 3, .. }));
            b.get_mut().write_all(b"\"ok\"\n").await.unwrap();
            b
        });
        let req = BrokerRequest::ModChannel { uplay_id: 7, account_id: None, session: None, profile: None, conn: 3, client_pid: Some(9), game: None };
        open_channel(a, &req).await.unwrap();
        server.await.unwrap();
        let (a, b) = duplex(1 << 16);
        tokio::spawn(async move {
            let mut b = BufReader::new(b);
            let _ = read_line_capped(&mut b, 4096).await;
            b.get_mut().write_all(b"{\"error\":{\"message\":\"no\"}}\n").await.unwrap();
        });
        assert!(open_channel(a, &req).await.unwrap_err().to_string().contains("no"));
    }
}
