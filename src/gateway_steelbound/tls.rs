// SPDX-License-Identifier: AGPL-3.0-only
//! TLS of the Steelbound gateway: server authentication only (players have
//! no client certificates; they prove who they are with the ticket).
//!
//! Clients pin the server's public key: the pin is the SHA-256 of the DER
//! `SubjectPublicKeyInfo` of the certificate, 64 lowercase hex digits. It
//! stays the same when the certificate is renewed with the same key, so the
//! certificate may be self-signed and its dates do not matter to a pinning
//! client. The server logs the pin at startup.

use rustls::{
    ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Server TLS configuration from PEM bytes. `tls13_only` refuses TLS 1.2
/// (rustls offers only ECDHE + AEAD suites for 1.2 anyway). Returns the
/// configuration and the SPKI pin of the leaf certificate.
pub fn server_config(cert_pem: &[u8], key_pem: &[u8], tls13_only: bool) -> Result<(Arc<ServerConfig>, String), String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("certificate: {e}"))?;
    let leaf = certs.first().ok_or("certificate: no certificate in file")?;
    let pin = spki_pin(leaf.as_ref()).ok_or("certificate: cannot read its public key")?;
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| format!("key: {e}"))?;
    let versions: &[&rustls::SupportedProtocolVersion] =
        if tls13_only { &[&rustls::version::TLS13] } else { &[&rustls::version::TLS13, &rustls::version::TLS12] };
    let mut cfg = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("certificate/key: {e}"))?;
    // Nothing to rotate, nothing to resume: each transaction is a fresh,
    // short connection.
    cfg.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    cfg.send_tls13_tickets = 0;
    Ok((Arc::new(cfg), pin))
}

/// One DER element: (tag, header length, content length).
fn tlv(b: &[u8]) -> Option<(u8, usize, usize)> {
    let tag = *b.first()?;
    let first = *b.get(1)? as usize;
    if first < 0x80 {
        return Some((tag, 2, first));
    }
    let n = first & 0x7f;
    if n == 0 || n > 4 || b.len() < 2 + n {
        return None;
    }
    let len = b[2..2 + n].iter().fold(0usize, |acc, &x| (acc << 8) | x as usize);
    Some((tag, 2 + n, len))
}

/// The DER `SubjectPublicKeyInfo` of an X.509 certificate.
pub fn spki_der(cert: &[u8]) -> Option<&[u8]> {
    // Certificate ::= SEQUENCE { tbsCertificate SEQUENCE { ... } ... }
    let (tag, hl, _) = tlv(cert)?;
    (tag == 0x30).then_some(())?;
    let tbs_all = cert.get(hl..)?;
    let (tag, hl, len) = tlv(tbs_all)?;
    (tag == 0x30).then_some(())?;
    let mut rest = tbs_all.get(hl..hl + len)?;
    // [0] version (optional), serial, signature, issuer, validity, subject, spki
    let mut index = 0;
    loop {
        let (tag, hl, len) = tlv(rest)?;
        let element = rest.get(..hl + len)?;
        if index == 0 && tag == 0xa0 {
            rest = &rest[hl + len..];
            continue;
        }
        if index == 5 {
            return (tag == 0x30).then_some(element);
        }
        index += 1;
        rest = &rest[hl + len..];
    }
}

/// The pin of a certificate: lowercase hex SHA-256 of its SPKI.
pub fn spki_pin(cert: &[u8]) -> Option<String> {
    let spki = spki_der(cert)?;
    Some(Sha256::digest(spki).iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_is_hash_of_the_public_key() {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let a = rcgen::CertificateParams::new(vec!["a.invalid".into()]).unwrap().self_signed(&key).unwrap();
        let b = rcgen::CertificateParams::new(vec!["b.invalid".into()]).unwrap().self_signed(&key).unwrap();
        let pin_a = spki_pin(a.der()).unwrap();
        // Same key, other certificate: same pin.
        assert_eq!(pin_a, spki_pin(b.der()).unwrap());
        // Equals SHA-256 over the DER public key rcgen reports.
        let expected: String = Sha256::digest(rcgen::PublicKeyData::subject_public_key_info(&key)).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(pin_a, expected);
        let (_, pin) = server_config(a.pem().as_bytes(), key.serialize_pem().as_bytes(), false).unwrap();
        assert_eq!(pin, pin_a);
    }

    #[test]
    fn garbage_has_no_pin() {
        assert_eq!(spki_pin(&[]), None);
        assert_eq!(spki_pin(&[0x30, 0x82, 0xff]), None);
        assert_eq!(spki_pin(&[0x30, 0x03, 0x30, 0x01, 0x00]), None);
    }
}
