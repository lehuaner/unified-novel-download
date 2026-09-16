//! unidbg 签名 sidecar 客户端。
//!
//! 调用 unidbg-boot-server 的 `POST /api/fq-signature/generateSignatureWithMap`，
//! 传入完整 URL + headerMap，返回 7 个签名头：
//! X-Ladon / X-Khronos / X-Soter / X-Argus / X-Gorgon / X-Helios / X-Medusa。

use anyhow::{Result, anyhow};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Serialize)]
struct SignRequest {
    url: String,
    #[serde(rename = "headerMap")]
    header_map: HashMap<String, String>,
}

#[derive(Deserialize)]
struct SignResponse {
    #[serde(default)]
    #[serde(rename = "X-Ladon")]
    x_ladon: Option<String>,
    #[serde(default)]
    #[serde(rename = "X-Khronos")]
    x_khronos: Option<String>,
    #[serde(default)]
    #[serde(rename = "X-Soter")]
    x_soter: Option<String>,
    #[serde(default)]
    #[serde(rename = "X-Argus")]
    x_argus: Option<String>,
    #[serde(default)]
    #[serde(rename = "X-Gorgon")]
    x_gorgon: Option<String>,
    #[serde(default)]
    #[serde(rename = "X-Helios")]
    x_helios: Option<String>,
    #[serde(default)]
    #[serde(rename = "X-Medusa")]
    x_medusa: Option<String>,
}

impl SignResponse {
    fn to_header_map(self) -> HashMap<String, String> {
        let mut m = HashMap::new();
        if let Some(v) = self.x_ladon { m.insert("X-Ladon".into(), v); }
        if let Some(v) = self.x_khronos { m.insert("X-Khronos".into(), v); }
        if let Some(v) = self.x_soter { m.insert("X-Soter".into(), v); }
        if let Some(v) = self.x_argus { m.insert("X-Argus".into(), v); }
        if let Some(v) = self.x_gorgon { m.insert("X-Gorgon".into(), v); }
        if let Some(v) = self.x_helios { m.insert("X-Helios".into(), v); }
        if let Some(v) = self.x_medusa { m.insert("X-Medusa".into(), v); }
        m
    }
}

/// unidbg 签名 sidecar 客户端。
#[derive(Clone)]
pub(crate) struct UnidbgSigner {
    client: Client,
    base_url: String,
}

impl UnidbgSigner {
    /// 返回 sidecar base URL（如 `http://127.0.0.1:8099`）。
    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `base_url` 如 `http://127.0.0.1:8099`。
    pub(crate) fn new(base_url: &str, timeout_ms: u64) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_millis(timeout_ms.max(100)))
            .build()?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    /// 调用 unidbg 生成签名头。
    ///
    /// 返回的 HashMap 包含 X-Helios / X-Medusa / X-Argus / X-Gorgon / X-Ladon / X-Khronos / X-Soter。
    pub(crate) fn sign(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<HashMap<String, String>> {
        let endpoint = format!("{}/api/fq-signature/generateSignatureWithMap", self.base_url);
        let req = SignRequest {
            url: url.to_string(),
            header_map: headers.clone(),
        };
        let resp = self.client.post(&endpoint).json(&req).send()?;
        if !resp.status().is_success() {
            return Err(anyhow!("signer HTTP {}", resp.status()));
        }
        let sr: SignResponse = resp.json()?;
        let map = sr.to_header_map();
        if map.is_empty() {
            return Err(anyhow!("signer returned empty signatures (URL may lack device params)"));
        }
        Ok(map)
    }

    /// 健康检查。
    #[allow(dead_code)]
    pub(crate) fn health(&self) -> Result<bool> {
        let endpoint = format!("{}/api/fq-signature/health", self.base_url);
        let resp = self.client.get(&endpoint).send()?;
        Ok(resp.status().is_success())
    }
}
