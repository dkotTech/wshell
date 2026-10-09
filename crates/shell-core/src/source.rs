//! Where a package comes from: a local `.pkg` archive or package directory, or an OCI registry
//! (`oci://ghcr.io/acme/ping:1.0`, like Helm's OCI charts).
//!
//! In a registry a package is an OCI artifact with one layer, the `.pkg` archive, so any OCI
//! tool can publish it:
//!
//! ```sh
//! oras push ghcr.io/acme/ping:1.0 --artifact-type application/vnd.wshell.package.v1 \
//!     ping.pkg:application/vnd.wshell.package.v1.tar
//! ```

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

/// The artifact type of a package in a registry.
pub const ARTIFACT_TYPE: &str = "application/vnd.wshell.package.v1";
/// The media type of its layer: the `.pkg` tar archive.
pub const LAYER_MEDIA_TYPE: &str = "application/vnd.wshell.package.v1.tar";

const SCHEME: &str = "oci://";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Source {
    Path(PathBuf),
    Oci(OciRef),
}

impl Source {
    /// A relative path is taken relative to `base`.
    pub fn parse(s: &str, base: &Path) -> Result<Self> {
        Ok(match s.parse()? {
            Source::Path(p) if p.is_relative() => Source::Path(base.join(p)),
            source => source,
        })
    }
}

impl FromStr for Source {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.strip_prefix(SCHEME) {
            Some(r) => Ok(Source::Oci(r.parse().with_context(|| format!("{s}: not an OCI reference"))?)),
            None => {
                ensure!(!s.is_empty(), "empty package path");
                Ok(Source::Path(PathBuf::from(s)))
            }
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Path(p) => write!(f, "{}", p.display()),
            Source::Oci(r) => write!(f, "{r}"),
        }
    }
}

impl TryFrom<String> for Source {
    type Error = anyhow::Error;
    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<Source> for String {
    fn from(s: Source) -> String {
        s.to_string()
    }
}

/// `registry/repository[:tag][@sha256:…]`; the registry is always explicit (no Docker Hub
/// default). With a digest the tag is informational: the digest decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciRef {
    /// `host` or `host:port`.
    pub registry: String,
    pub repository: String,
    pub tag: Option<String>,
    /// `sha256:<64 hex>`.
    pub digest: Option<String>,
}

impl OciRef {
    /// The tag or digest to ask the registry for.
    pub fn reference(&self) -> &str {
        self.digest.as_deref().or(self.tag.as_deref()).unwrap_or("latest")
    }

    /// The same artifact, pinned to `digest`: what a tag pointed to when it was read.
    pub fn pinned(&self, digest: &str) -> OciRef {
        OciRef { digest: Some(digest.to_string()), ..self.clone() }
    }
}

impl FromStr for OciRef {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (rest, digest) = match s.split_once('@') {
            Some((rest, d)) => (rest, Some(d.to_string())),
            None => (s, None),
        };
        let (registry, path) = rest.split_once('/').context("expected registry/repository")?;
        ensure!(
            registry.contains(['.', ':']) || registry == "localhost",
            "{registry:?} is not a registry host (write it out, e.g. ghcr.io or docker.io)"
        );
        ensure!(
            registry.chars().all(|c| c.is_ascii_alphanumeric() || "-.:[]".contains(c)),
            "invalid registry {registry:?}"
        );
        // A colon after the last slash starts the tag.
        let (repository, tag) = match path.rfind(':').filter(|&i| !path[i..].contains('/')) {
            Some(i) => (&path[..i], Some(path[i + 1..].to_string())),
            None => (path, None),
        };
        ensure!(
            !repository.is_empty()
                && repository.split('/').all(|p| !p.is_empty()
                    && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))),
            "invalid repository {repository:?} (lowercase letters, digits, . _ - and /)"
        );
        if let Some(t) = &tag {
            ensure!(
                (1..=128).contains(&t.len())
                    && !t.starts_with(['.', '-'])
                    && t.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)),
                "invalid tag {t:?}"
            );
        }
        if let Some(d) = &digest {
            let Some(hex) = d.strip_prefix("sha256:") else { bail!("only sha256 digests are supported: {d}") };
            ensure!(
                hex.len() == 64 && hex.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "invalid digest {d}"
            );
        }
        Ok(OciRef { registry: registry.to_string(), repository: repository.to_string(), tag, digest })
    }
}

impl fmt::Display for OciRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{SCHEME}{}/{}", self.registry, self.repository)?;
        if let Some(t) = &self.tag {
            write!(f, ":{t}")?;
        }
        if let Some(d) = &self.digest {
            write!(f, "@{d}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn sources() {
        assert_eq!(Source::parse("ping.pkg", Path::new("/etc")).unwrap(), Source::Path("/etc/ping.pkg".into()));
        assert_eq!(Source::parse("/abs/x", Path::new("/etc")).unwrap(), Source::Path("/abs/x".into()));
        let Source::Oci(r) = Source::parse("oci://ghcr.io/acme/tools/ping:1.0", Path::new("/")).unwrap() else {
            panic!()
        };
        assert_eq!((r.registry.as_str(), r.repository.as_str()), ("ghcr.io", "acme/tools/ping"));
        assert_eq!((r.tag.as_deref(), r.reference()), (Some("1.0"), "1.0"));
        // Round trip through the string form (the control protocol, the state file).
        let s = format!("oci://localhost:5000/ping:1.0@{D}");
        let src: Source = s.parse().unwrap();
        assert_eq!(src.to_string(), s);
        let Source::Oci(r) = src else { panic!() };
        assert_eq!((r.registry.as_str(), r.reference()), ("localhost:5000", D));
        assert_eq!("localhost/ping".parse::<OciRef>().unwrap().reference(), "latest");
    }

    #[test]
    fn invalid_references() {
        for bad in [
            "acme/ping:1",                // no registry host
            "ghcr.io/Acme/ping",          // uppercase
            "ghcr.io/acme//ping",         // empty component
            "ghcr.io/",                   // no repository
            "ghcr.io/acme/ping:-x",       // tag
            "ghcr.io/acme/ping@md5:abcd", // digest algorithm
            "ghcr.io/acme/ping@sha256:12",
        ] {
            assert!(bad.parse::<OciRef>().is_err(), "{bad}");
        }
    }
}
