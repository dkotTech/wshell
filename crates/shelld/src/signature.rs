//! Package signatures: cosign's, made with a key (`cosign generate-key-pair`, ECDSA P-256).
//!
//! `cosign sign --key` (cosign 3) attaches a sigstore bundle to the package's manifest as an
//! OCI referrer. The bundle holds a DSSE envelope whose payload, an in-toto statement, names the
//! manifest's sha256; the signature covers the envelope's pre-authentication encoding (PAE).
//! The manifest names the layer by digest, so a valid signature covers the whole package.
//!
//! Only the key is checked, not a transparency log: trust comes from the keys in the config.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
use serde::Deserialize;

pub const BUNDLE_TYPE: &str = "application/vnd.dev.sigstore.bundle.v0.3+json";
const PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
const PREDICATE_TYPE: &str = "https://sigstore.dev/cosign/sign/v1";

/// DER prefix of a P-256 public key's SubjectPublicKeyInfo: the 65-byte point follows.
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce,
    0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

#[derive(Clone)]
pub struct TrustedKey {
    /// The key file: how the signer is named in the output and the audit log.
    pub file: PathBuf,
    point: Vec<u8>,
}

impl std::fmt::Debug for TrustedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TrustedKey({})", self.file.display())
    }
}

/// `cosign.pub`: a PEM `PUBLIC KEY` (ECDSA P-256).
pub fn load_key(file: &Path) -> Result<TrustedKey> {
    let pem = fs::read_to_string(file).with_context(|| format!("{}", file.display()))?;
    let point = parse_pem(&pem).with_context(|| format!("{}: expected a cosign public key (ECDSA P-256)", file.display()))?;
    Ok(TrustedKey { file: file.to_path_buf(), point })
}

fn parse_pem(pem: &str) -> Result<Vec<u8>> {
    let body = pem
        .trim()
        .strip_prefix("-----BEGIN PUBLIC KEY-----")
        .and_then(|b| b.strip_suffix("-----END PUBLIC KEY-----"))
        .context("not a PEM PUBLIC KEY")?;
    let der = B64.decode(body.split_whitespace().collect::<String>())?;
    let point = der.strip_prefix(&P256_SPKI_PREFIX).context("not a P-256 key")?;
    ensure!(point.len() == 65 && point[0] == 4, "not an uncompressed P-256 point");
    Ok(point.to_vec())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bundle {
    media_type: String,
    dsse_envelope: Envelope,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    payload: String,
    payload_type: String,
    signatures: Vec<EnvelopeSignature>,
}

#[derive(Deserialize)]
struct EnvelopeSignature {
    sig: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Statement {
    #[serde(rename = "_type")]
    kind: String,
    subject: Vec<Subject>,
    predicate_type: String,
}

#[derive(Deserialize)]
struct Subject {
    digest: std::collections::HashMap<String, String>,
}

/// The key that signed `manifest_digest` (`sha256:…`) in this bundle, if a trusted one did.
pub fn verify_bundle<'k>(bundle: &[u8], manifest_digest: &str, keys: &'k [TrustedKey]) -> Result<&'k TrustedKey> {
    let bundle: Bundle = serde_json::from_slice(bundle).context("not a sigstore bundle")?;
    ensure!(bundle.media_type.starts_with("application/vnd.dev.sigstore.bundle"), "not a sigstore bundle");
    let env = bundle.dsse_envelope;
    ensure!(env.payload_type == PAYLOAD_TYPE, "unexpected payload type {}", env.payload_type);
    let payload = B64.decode(&env.payload).context("payload")?;

    // The signature first: only then is the payload worth reading.
    let message = pae(&env.payload_type, &payload);
    let signer = keys.iter().find(|k| {
        env.signatures.iter().any(|s| {
            B64.decode(&s.sig)
                .is_ok_and(|sig| UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, &k.point).verify(&message, &sig).is_ok())
        })
    });
    let Some(signer) = signer else { bail!("not signed by a trusted key") };

    let statement: Statement = serde_json::from_slice(&payload).context("not an in-toto statement")?;
    ensure!(statement.kind == STATEMENT_TYPE, "unexpected statement type {}", statement.kind);
    ensure!(statement.predicate_type == PREDICATE_TYPE, "unexpected predicate {}", statement.predicate_type);
    let hex = manifest_digest.strip_prefix("sha256:").unwrap_or(manifest_digest);
    ensure!(
        statement.subject.iter().any(|s| s.digest.get("sha256").is_some_and(|d| d == hex)),
        "the signature is for another artifact"
    );
    Ok(signer)
}

/// DSSE pre-authentication encoding.
fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut m = format!("DSSEv1 {} {} {} ", payload_type.len(), payload_type, payload.len()).into_bytes();
    m.extend_from_slice(payload);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    // Made with `cosign generate-key-pair` and `cosign sign --key` (cosign 3.1.3) of a package
    // whose manifest is sha256:7ddc9d77….
    const PUB: &str = "-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEPtGgoYxCXaYsZqFpzbwN5Hsphjbv
Rbf5sYBMwKt3geSOWj034m7lTzKo7SzVSusVWkQOIuf4In8xhhr4l98AHw==
-----END PUBLIC KEY-----";
    const DIGEST: &str = "sha256:7ddc9d77eb53b3cdd7262b8b2dd3e0983b3129db9c6b4f4a9abdead15e378335";
    const BUNDLE: &str = r#"{"mediaType":"application/vnd.dev.sigstore.bundle.v0.3+json","verificationMaterial":{"publicKey":{"hint":"/9LQYS4e8S+gvQAgaW8YnAmxtzq6P8QjwQy3m3A43e0="}},"dsseEnvelope":{"payload":"eyJfdHlwZSI6Imh0dHBzOi8vaW4tdG90by5pby9TdGF0ZW1lbnQvdjEiLCJzdWJqZWN0IjpbeyJkaWdlc3QiOnsic2hhMjU2IjoiN2RkYzlkNzdlYjUzYjNjZGQ3MjYyYjhiMmRkM2UwOTgzYjMxMjlkYjljNmI0ZjRhOWFiZGVhZDE1ZTM3ODMzNSJ9LCJhbm5vdGF0aW9ucyI6e319XSwicHJlZGljYXRlVHlwZSI6Imh0dHBzOi8vc2lnc3RvcmUuZGV2L2Nvc2lnbi9zaWduL3YxIiwicHJlZGljYXRlIjp7fX0=","payloadType":"application/vnd.in-toto+json","signatures":[{"sig":"MEUCIQDz+VpDpBNh3IlZGJExe/cXL5zFXrSWDnXKSd/VpqLRRwIgeJ9OgliQCzsiZhLzVD6vLx5WNXhGhwynNY/l81xidO0="}]}}"#;

    fn key() -> TrustedKey {
        TrustedKey { file: "cosign.pub".into(), point: parse_pem(PUB).unwrap() }
    }

    #[test]
    fn cosign_bundle() {
        let keys = [key()];
        assert!(verify_bundle(BUNDLE.as_bytes(), DIGEST, &keys).is_ok());
        // Another artifact.
        let other = DIGEST.replace("7ddc", "7ddd");
        assert!(verify_bundle(BUNDLE.as_bytes(), &other, &keys).is_err());
        // A changed signature.
        let forged = BUNDLE.replace("MEUCIQDz", "MEUCIQDy");
        assert!(verify_bundle(forged.as_bytes(), DIGEST, &keys).is_err());
        // Another key: a point that is valid but not the signer's (the generator of P-256).
        let mut g = vec![4];
        g.extend(hex("6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296"));
        g.extend(hex("4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5"));
        let stranger = TrustedKey { file: "other.pub".into(), point: g };
        assert!(verify_bundle(BUNDLE.as_bytes(), DIGEST, &[stranger]).is_err());
        assert!(parse_pem("-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----").is_err());
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }
}
