use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;

use crate::model::Page;

pub const USER_AGENT: &str = concat!("yomu/", env!("CARGO_PKG_VERSION"), " (terminal reader)");

static CLIENT: OnceLock<Client> = OnceLock::new();

pub fn client() -> &'static Client {
    CLIENT.get_or_init(|| {
        Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client")
    })
}

pub fn cache_dir() -> PathBuf {
    let base = directories::ProjectDirs::from("", "", "yomu")
        .map(|d| d.cache_dir().to_path_buf())
        .unwrap_or_else(|| std::env::temp_dir().join("yomu"));
    let _ = std::fs::create_dir_all(&base);
    base
}

pub fn config_dir() -> PathBuf {
    let base = directories::ProjectDirs::from("", "", "yomu")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".yomu"));
    let _ = std::fs::create_dir_all(base.join("sources"));
    base
}

fn cache_path(url: &str) -> PathBuf {
    let mut h = DefaultHasher::new();
    url.hash(&mut h);
    let ext = url
        .rsplit('.')
        .next()
        .filter(|e| e.len() <= 4 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("img");
    cache_dir().join(format!("{:016x}.{}", h.finish(), ext))
}

/// Fetch page bytes, using the on-disk cache when we already have them.
pub async fn fetch_page(page: &Page) -> Result<Vec<u8>> {
    let path = cache_path(&page.url);
    if let Ok(bytes) = tokio::fs::read(&path).await {
        if !bytes.is_empty() {
            return Ok(bytes);
        }
    }

    let mut req = client().get(&page.url);
    for (k, v) in &page.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.with_context(|| format!("GET {}", page.url))?;
    let resp = resp.error_for_status().with_context(|| format!("GET {}", page.url))?;
    let bytes = resp.bytes().await?.to_vec();

    // Best-effort cache write; a failure here must not break reading.
    let _ = tokio::fs::write(&path, &bytes).await;
    Ok(bytes)
}

pub async fn get_text(url: &str, headers: &[(String, String)]) -> Result<String> {
    let mut req = client().get(url);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await?.error_for_status()?;
    Ok(resp.text().await?)
}
