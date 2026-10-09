//! Packages from OCI registries: the pull side of the distribution API. A manifest, then its one
//! `.pkg` layer, each checked against its sha256 digest. Blobs are cached by digest: a
//! reference pinned to a digest is downloaded once, and a desired-state file naming a tag only
//! reads the manifest again.
//!
//! Anonymous access only for now (public packages): token auth as ghcr.io, Docker Hub and
//! most registries do it for pulls.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail, ensure};
use reqwest::header::{ACCEPT, AUTHORIZATION, WWW_AUTHENTICATE};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use shell_core::config::RegistrySection;
use shell_core::source::{LAYER_MEDIA_TYPE, OciRef};

const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
/// Every manifest type: some registries (ghcr.io) answer 404 for a type not accepted, which
/// would read as "not found" for an image that is merely not a package.
const ACCEPT_MANIFESTS: &str = "application/vnd.oci.image.manifest.v1+json, \
    application/vnd.oci.image.index.v1+json, \
    application/vnd.docker.distribution.manifest.v2+json, \
    application/vnd.docker.distribution.manifest.list.v2+json";
const MAX_MANIFEST: usize = 1 << 20;
/// Cached blobs unused for this long are removed.
const KEEP: Duration = Duration::from_secs(30 * 24 * 3600);

#[derive(Deserialize)]
struct Manifest {
    layers: Vec<Descriptor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    media_type: String,
    digest: String,
    size: u64,
}

pub struct Pulled {
    /// The `.pkg` archive in the cache.
    pub path: PathBuf,
    /// The reference pinned to the manifest's digest.
    pub pinned: OciRef,
}

pub fn client() -> Result<Client> {
    Ok(Client::builder()
        // Registries hand blobs out from a CDN; reqwest drops `Authorization` on the way there.
        .redirect(reqwest::redirect::Policy::limited(5))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .user_agent(concat!("wshell/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

pub async fn pull(client: &Client, cfg: &RegistrySection, cache: &Path, r: &OciRef) -> Result<Pulled> {
    let mut session = Session::new(client, cfg, r);
    let cached = r.digest.as_deref().and_then(|d| fs::read(blob_path(cache, d)).ok());
    let (bytes, digest) = match cached {
        Some(bytes) => (bytes, r.digest.clone().unwrap_or_default()),
        None => session.manifest().await?,
    };
    let manifest: Manifest = serde_json::from_slice(&bytes).context("not an OCI image manifest")?;
    let mut layers = manifest.layers.iter().filter(|l| l.media_type == LAYER_MEDIA_TYPE);
    let (Some(layer), None) = (layers.next(), layers.next()) else {
        bail!("not a wshell package: expected one layer of type {LAYER_MEDIA_TYPE}");
    };
    // The manifest comes from the network: its digest becomes a file name.
    ensure!(valid_digest(&layer.digest), "invalid layer digest {}", layer.digest);
    let max = u64::from(cfg.max_package_mb) << 20;
    ensure!(layer.size <= max, "the package is {} MB, over registry.max_package_mb", layer.size >> 20);

    let path = blob_path(cache, &layer.digest);
    if path.exists() {
        touch(&path);
    } else {
        session.blob(layer, &path, max).await?;
    }
    write_blob(cache, &digest, &bytes)?;
    Ok(Pulled { path, pinned: r.pinned(&digest) })
}

/// Removes the cached blobs nobody used for a month.
pub fn gc(cache: &Path) {
    let Ok(entries) = fs::read_dir(cache.join("sha256")) else { return };
    for e in entries.flatten() {
        let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > KEEP));
        if old {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// `sha256:<hex>` → `<cache>/sha256/<hex>`. The digest is validated before it gets here.
fn blob_path(cache: &Path, digest: &str) -> PathBuf {
    let (algo, hex) = digest.split_once(':').unwrap_or(("sha256", digest));
    cache.join(algo).join(hex)
}

fn valid_digest(d: &str) -> bool {
    d.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)))
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{}", crate::util::hex(&Sha256::digest(bytes)))
}

fn touch(path: &Path) {
    if let Ok(f) = fs::File::options().write(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

fn write_blob(cache: &Path, digest: &str, bytes: &[u8]) -> Result<()> {
    let path = blob_path(cache, digest);
    fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension(format!("part-{}", crate::util::token()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Requests to one repository, with the token once the registry asked for one.
struct Session<'a> {
    client: &'a Client,
    base: String,
    reference: &'a OciRef,
    token: Option<String>,
}

impl<'a> Session<'a> {
    fn new(client: &'a Client, cfg: &RegistrySection, r: &'a OciRef) -> Self {
        let host = r
            .registry
            .rsplit_once(':')
            .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
            .map_or(r.registry.as_str(), |(host, _)| host);
        let plain = matches!(host, "localhost" | "127.0.0.1" | "[::1]") || cfg.insecure.contains(&r.registry);
        // Docker Hub's API lives on another host, and its official images under `library/`.
        let (registry, repository) = match r.registry.as_str() {
            "docker.io" if !r.repository.contains('/') => ("registry-1.docker.io", format!("library/{}", r.repository)),
            "docker.io" => ("registry-1.docker.io", r.repository.clone()),
            other => (other, r.repository.clone()),
        };
        let scheme = if plain { "http" } else { "https" };
        Session { client, base: format!("{scheme}://{registry}/v2/{repository}"), reference: r, token: None }
    }

    async fn get(&mut self, url: &str, accept: &str) -> Result<reqwest::Response> {
        for _ in 0..2 {
            let mut req = self.client.get(url).header(ACCEPT, accept);
            if let Some(t) = &self.token {
                req = req.header(AUTHORIZATION, format!("Bearer {t}"));
            }
            let resp = req.send().await.with_context(|| format!("{}: request failed", self.reference.registry))?;
            if resp.status() == StatusCode::UNAUTHORIZED && self.token.is_none() {
                let challenge = resp.headers().get(WWW_AUTHENTICATE).and_then(|v| v.to_str().ok()).unwrap_or("");
                self.token = Some(self.authenticate(challenge).await?);
                continue;
            }
            return match resp.status() {
                s if s.is_success() => Ok(resp),
                StatusCode::NOT_FOUND => bail!("{} not found", self.reference),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                    bail!("{}: access denied (private packages are not supported yet)", self.reference)
                }
                s => bail!("{}: registry answered {s}", self.reference),
            };
        }
        bail!("{}: access denied", self.reference)
    }

    /// An anonymous pull token from the realm the registry named.
    async fn authenticate(&self, challenge: &str) -> Result<String> {
        let Some(params) = challenge.strip_prefix("Bearer ") else {
            bail!("{}: the registry needs a login (not supported yet)", self.reference.registry);
        };
        let param = |name: &str| {
            params.split(',').find_map(|kv| {
                let (k, v) = kv.trim().split_once('=')?;
                (k == name).then(|| v.trim_matches('"').to_string())
            })
        };
        let realm = param("realm").context("no realm in the registry's challenge")?;
        let mut query = Vec::new();
        if let Some(service) = param("service") {
            query.push(("service", service));
        }
        query.push(("scope", param("scope").unwrap_or_else(|| format!("repository:{}:pull", self.reference.repository))));
        #[derive(Deserialize)]
        struct Token {
            token: Option<String>,
            access_token: Option<String>,
        }
        let body = self
            .client
            .get(&realm)
            .query(&query)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .with_context(|| format!("token from {realm}"))?
            .bytes()
            .await?;
        let t: Token = serde_json::from_slice(&body).context("the registry's token answer")?;
        t.token.or(t.access_token).context("no token in the registry's answer")
    }

    /// The manifest and its digest; with a pinned reference the digest must match.
    async fn manifest(&mut self) -> Result<(Vec<u8>, String)> {
        let url = format!("{}/manifests/{}", self.base, self.reference.reference());
        let resp = self.get(&url, ACCEPT_MANIFESTS).await?;
        let kind = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
        ensure!(
            kind.starts_with(MANIFEST),
            "{}: not a wshell package (got {kind}; push it with oras, see README)",
            self.reference
        );
        let bytes = resp.bytes().await?;
        ensure!(bytes.len() <= MAX_MANIFEST, "manifest too large");
        let digest = sha256(&bytes);
        if let Some(pinned) = &self.reference.digest {
            ensure!(&digest == pinned, "{}: the registry returned a manifest with digest {digest}", self.reference);
        }
        Ok((bytes.to_vec(), digest))
    }

    /// Streams the layer to the cache, checking its size and digest.
    async fn blob(&mut self, layer: &Descriptor, dest: &Path, max: u64) -> Result<()> {
        fs::create_dir_all(dest.parent().unwrap())?;
        let url = format!("{}/blobs/{}", self.base, layer.digest);
        let mut resp = self.get(&url, "*/*").await?;
        let tmp = dest.with_extension(format!("part-{}", crate::util::token()));
        let result = async {
            let mut file = fs::File::create(&tmp)?;
            let mut hasher = Sha256::new();
            let mut size = 0u64;
            while let Some(chunk) = resp.chunk().await? {
                size += chunk.len() as u64;
                ensure!(size <= layer.size.min(max), "the layer is larger than its manifest says");
                hasher.update(&chunk);
                file.write_all(&chunk)?;
            }
            ensure!(size == layer.size, "the layer is {size} bytes, its manifest says {}", layer.size);
            let digest = format!("sha256:{}", crate::util::hex(&hasher.finalize()));
            ensure!(digest == layer.digest, "the layer's digest is {digest}, its manifest says {}", layer.digest);
            file.sync_all()?;
            fs::rename(&tmp, dest)?;
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result.with_context(|| format!("{}: downloading the package", self.reference))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests() {
        assert!(valid_digest(&sha256(b"")));
        assert!(!valid_digest("sha256:../../etc"));
        assert_eq!(
            blob_path(Path::new("/c"), &sha256(b"")),
            Path::new("/c/sha256/e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
    }
}
