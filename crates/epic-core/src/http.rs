//! The HTTP seam: Epic's calls go through a [`Transport`], so tests run the whole flow
//! (sign-in, refresh, library, catalog) on recorded JSON with [`Fake`], and nothing in
//! the test suite ever reaches Epic.
//!
//! Requests carry a `label` ("oauth token", "library items") for logs: a request's URL
//! can hold a token (`…/sessions/kill/<token>`) and its headers and form always do, so
//! only the label and the status are ever logged.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use quiver_core::stores::BoxFuture;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Delete,
    Put,
}

#[derive(Clone)]
pub struct Request {
    pub label: &'static str,
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// `application/x-www-form-urlencoded` body.
    pub form: Option<Vec<(String, String)>>,
    /// Any other body, with its content type (JSON, a save's chunk).
    pub body: Option<(String, Vec<u8>)>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} {}", self.method, self.label)
    }
}

impl Request {
    pub fn new(label: &'static str, method: Method, url: impl Into<String>) -> Self {
        Request { label, method, url: url.into(), headers: Vec::new(), form: None, body: None }
    }

    pub fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.to_owned(), v.into()));
        self
    }

    pub fn bearer(self, token: &str) -> Self {
        self.header("Authorization", format!("bearer {token}"))
    }

    pub fn form(mut self, pairs: &[(&str, &str)]) -> Self {
        self.form = Some(pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect());
        self
    }

    /// A JSON body.
    pub fn json(self, v: &serde_json::Value) -> Self {
        self.body("application/json", v.to_string().into_bytes())
    }

    pub fn body(mut self, content_type: &str, data: Vec<u8>) -> Self {
        self.body = Some((content_type.to_owned(), data));
        self
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

pub trait Transport: Send + Sync {
    fn send(&self, req: Request) -> BoxFuture<'_, Result<Response>>;
}

/// The real transport: rustls, a timeout, and the configured user agent.
pub struct Reqwest {
    client: reqwest::Client,
}

impl Reqwest {
    pub fn new(user_agent: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("build the Epic HTTP client")?;
        Ok(Reqwest { client })
    }
}

impl Transport for Reqwest {
    fn send(&self, req: Request) -> BoxFuture<'_, Result<Response>> {
        Box::pin(async move {
            let label = req.label;
            let mut b = match req.method {
                Method::Get => self.client.get(&req.url),
                Method::Post => self.client.post(&req.url),
                Method::Delete => self.client.delete(&req.url),
                Method::Put => self.client.put(&req.url),
            };
            for (k, v) in &req.headers {
                b = b.header(k, v);
            }
            if let Some(form) = &req.form {
                b = b.form(form);
            }
            if let Some((ct, data)) = req.body {
                b = b.header(reqwest::header::CONTENT_TYPE, ct).body(data);
            }
            // reqwest's errors name the URL, which can hold a token: keep only the kind.
            let r = b.send().await.map_err(|e| failed(label, &e))?;
            let status = r.status().as_u16();
            let body = r.bytes().await.map_err(|e| failed(label, &e))?.to_vec();
            tracing::debug!(status, bytes = body.len(), "Epic {label}");
            Ok(Response { status, body })
        })
    }
}

/// A request that got no reply: a network failure, worth trying again later
/// ([`quiver_core::retry::Transient`]); anything else (a request reqwest couldn't build)
/// isn't.
fn failed(label: &str, e: &reqwest::Error) -> anyhow::Error {
    let msg = format!("Epic {label}: {}", describe(e));
    if e.is_timeout() || e.is_connect() || e.is_body() || e.is_decode() || e.is_request() {
        quiver_core::retry::transient(msg)
    } else {
        anyhow::anyhow!(msg)
    }
}

fn describe(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "couldn't connect"
    } else if e.is_body() || e.is_decode() {
        "the reply was cut off"
    } else {
        "the request failed"
    }
}

/// A scripted transport for tests: answers in order, records what was asked.
#[derive(Default)]
pub struct Fake {
    replies: Mutex<VecDeque<(String, Response)>>,
    pub seen: Mutex<Vec<Request>>,
}

impl Fake {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a reply for the next request whose URL contains `url_part`.
    pub fn reply(&self, url_part: &str, status: u16, body: &str) -> &Self {
        self.replies.lock().unwrap().push_back((url_part.to_owned(), Response { status, body: body.as_bytes().to_vec() }));
        self
    }

    /// [`Fake::reply`] with a binary body (a save's manifest or chunk).
    pub fn reply_bytes(&self, url_part: &str, status: u16, body: Vec<u8>) -> &Self {
        self.replies.lock().unwrap().push_back((url_part.to_owned(), Response { status, body }));
        self
    }

    pub fn requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }

    /// Replies queued and not asked for.
    pub fn unused(&self) -> Vec<String> {
        self.replies.lock().unwrap().iter().map(|(u, _)| u.clone()).collect()
    }
}

impl Transport for Fake {
    fn send(&self, req: Request) -> BoxFuture<'_, Result<Response>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(req.clone());
            let mut q = self.replies.lock().unwrap();
            let i = q.iter().position(|(part, _)| req.url.contains(part.as_str())).with_context(|| format!("fake Epic: nothing scripted for {} {}", req.label, req.url))?;
            Ok(q.remove(i).unwrap().1)
        })
    }
}
