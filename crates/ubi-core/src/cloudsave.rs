//! Ubisoft cloud saves via `cloudsave_service`.
//!
//! The demux service never transfers save data itself: for each operation it returns a
//! pre-signed HTTP request (method, host, path, auth header) that the client then performs.

use anyhow::{Context, Result, bail};
use ubi_proto::cloudsave_service as cs;

use crate::demux::{Cloudsave, Demux, ServiceConnection};
use crate::library::Library;

/// A pre-signed HTTP request returned by `cloudsave_service`.
#[derive(Debug, Clone)]
pub struct SignedRequest {
    pub method: String,
    pub url: String,
    /// Raw header block as returned by the service ("Name: value" lines).
    pub headers: Vec<(String, String)>,
}

impl SignedRequest {
    fn from_parts(method: &str, host: &str, path: &str, header: &str) -> Self {
        let host = host.trim_end_matches('/');
        let base = if host.starts_with("http") { host.to_owned() } else { format!("https://{host}") };
        let url = if path.starts_with('/') || path.starts_with('?') || path.is_empty() {
            format!("{base}{path}")
        } else {
            format!("{base}/{path}")
        };
        Self { method: method.to_owned(), url, headers: parse_headers(header) }
    }
}

fn parse_headers(block: &str) -> Vec<(String, String)> {
    block
        .split(['\r', '\n'])
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

pub struct CloudSaves {
    conn: ServiceConnection<Cloudsave>,
    http: reqwest::Client,
}

impl CloudSaves {
    pub async fn open(demux: &Demux) -> Result<Self> {
        Ok(Self {
            conn: demux.open::<Cloudsave>().await?,
            http: reqwest::Client::builder().user_agent("Massgate").build()?,
        })
    }

    /// V2: signed requests to download items (by slot id and name).
    pub async fn get_requests_v2(&mut self, lib: &mut Library, uplay_id: u32, items: &[(u32, &str)]) -> Result<Vec<SignedRequest>> {
        let ownership_token = lib.ownership_token(uplay_id).await?;
        let rsp = self
            .conn
            .request(cs::Req {
                cloudsave_req_v2: Some(cs::CloudsaveReqV2 {
                    uplay_id,
                    ownership_token,
                    method: Some(cs::cloudsave_req_v2::Method::GetItems(cs::cloudsave_req_v2::GetItems {
                        items: items
                            .iter()
                            .map(|&(item_id, name)| cs::cloudsave_req_v2::get_items::Item { item_id, item_name: name.to_owned() })
                            .collect(),
                    })),
                }),
                ..Default::default()
            })
            .await?;
        let r = rsp.cloudsave_rsp_v2.context("no cloudsave_rsp_v2")?;
        if r.status != cs::cloudsave_rsp_v2::Status::Ok as i32 {
            bail!("cloudsave_service (v2) refused: status {}", r.status);
        }
        Ok(r.http_reqs.iter().map(|h| SignedRequest::from_parts(&h.method, &h.host, &h.path, &h.header)).collect())
    }

    /// V2: signed upload requests. `items` are (slot, compressed body); the service signs
    /// the length and MD5 of each body.
    pub async fn put_requests_v2(&mut self, lib: &mut Library, uplay_id: u32, items: &[(u32, &[u8])]) -> Result<Vec<SignedRequest>> {
        use base64::Engine;
        let ownership_token = lib.ownership_token(uplay_id).await?;
        let rsp = self
            .conn
            .request(cs::Req {
                cloudsave_req_v2: Some(cs::CloudsaveReqV2 {
                    uplay_id,
                    ownership_token,
                    method: Some(cs::cloudsave_req_v2::Method::PutItems(cs::cloudsave_req_v2::PutItems {
                        items: items
                            .iter()
                            .map(|&(item_id, body)| cs::cloudsave_req_v2::put_items::Item {
                                item_id,
                                write_length: body.len() as u32,
                                md5_base64: base64::engine::general_purpose::STANDARD.encode(md5::compute(body).0),
                                item_name: item_id.to_string(),
                            })
                            .collect(),
                    })),
                }),
                ..Default::default()
            })
            .await?;
        let r = rsp.cloudsave_rsp_v2.context("no cloudsave_rsp_v2")?;
        if r.status != cs::cloudsave_rsp_v2::Status::Ok as i32 {
            bail!("cloudsave_service (v2 put) refused: status {}", r.status);
        }
        if r.http_reqs.len() != items.len() {
            bail!("asked to sign {} uploads, got {}", items.len(), r.http_reqs.len());
        }
        Ok(r.http_reqs.iter().map(|h| SignedRequest::from_parts(&h.method, &h.host, &h.path, &h.header)).collect())
    }

    /// Perform a signed request and return status and body.
    pub async fn execute(&self, req: &SignedRequest, body: Option<Vec<u8>>) -> Result<(u16, Vec<u8>)> {
        let method = reqwest::Method::from_bytes(req.method.as_bytes()).context("bad method")?;
        let mut r = self.http.request(method, &req.url);
        for (k, v) in &req.headers {
            r = r.header(k, v);
        }
        if let Some(b) = body {
            r = r.body(b);
        }
        let resp = r.send().await.context("cloud save HTTP request failed")?;
        let status = resp.status().as_u16();
        Ok((status, resp.bytes().await?.to_vec()))
    }
}
