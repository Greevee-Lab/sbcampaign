// SPDX-License-Identifier: AGPL-3.0-only
//! Provider accesses: who may call the Core API and what they may do.
//!
//! A caller is identified by its TLS client certificate. The certificate must
//! be issued by the steelcore CA (checked during the handshake) **and** its
//! SHA-256 fingerprint must be listed in the configuration. Renewing a
//! certificate means adding the new fingerprint; revoking means removing it.

use crate::core_api::{Provider, profile::AccessRecord};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, fmt, path::PathBuf, sync::Arc};
use toml::Table;

/// What an access may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// ResolveIdentity (creates players for the access's provider).
    IdentityWrite,
    /// SetDisplayName for players of the access's provider.
    ProfileWrite,
    /// GetPlayer, CheckDisplayName.
    ReadPublic,
    /// AdminService.
    Admin,
}

impl Scope {
    pub const ALL: [Scope; 4] = [Scope::IdentityWrite, Scope::ProfileWrite, Scope::ReadPublic, Scope::Admin];

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::IdentityWrite => "identity:write",
            Scope::ProfileWrite => "profile:write",
            Scope::ReadPublic => "read:public",
            Scope::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Scope> {
        Scope::ALL.into_iter().find(|x| x.as_str() == s)
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One configured access.
#[derive(Debug, Clone, PartialEq)]
pub struct Access {
    /// Unique name, used in logs and audit rows (for example "steelnet-eu").
    pub name: String,
    /// The provider this access speaks for. None for services that act for
    /// no provider (read-only statistics, admin tools).
    pub provider: Option<Provider>,
    /// Lowercase hex SHA-256 of the client certificate (DER).
    pub cert_sha256: String,
    pub scopes: Vec<Scope>,
    /// Sustained calls per second and burst size.
    pub rate_per_second: f64,
    pub burst: f64,
}

impl Access {
    pub fn has(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }

    pub fn record(&self) -> AccessRecord {
        AccessRecord {
            name: self.name.clone(),
            provider: self.provider,
            cert_sha256: self.cert_sha256.clone(),
            scopes: self.scopes.iter().map(|s| s.as_str().to_owned()).collect(),
        }
    }
}

/// Lowercase hex SHA-256 of a DER certificate.
pub fn cert_fingerprint(der: &[u8]) -> String {
    Sha256::digest(der).iter().map(|b| format!("{b:02x}")).collect()
}

/// Accesses by certificate fingerprint.
#[derive(Debug, Clone, Default)]
pub struct AccessList {
    by_fingerprint: HashMap<String, Arc<Access>>,
}

impl AccessList {
    pub fn new(accesses: Vec<Access>) -> Result<Self, String> {
        let mut by_fingerprint = HashMap::new();
        let mut names = std::collections::HashSet::new();
        for a in accesses {
            if !names.insert(a.name.clone()) {
                return Err(format!("core_api.access: duplicate name {:?}", a.name));
            }
            if by_fingerprint.insert(a.cert_sha256.clone(), Arc::new(a)).is_some() {
                return Err("core_api.access: duplicate cert_sha256".into());
            }
        }
        Ok(Self { by_fingerprint })
    }

    pub fn lookup(&self, cert_der: &[u8]) -> Option<Arc<Access>> {
        self.by_fingerprint.get(&cert_fingerprint(cert_der)).cloned()
    }

    pub fn records(&self) -> Vec<AccessRecord> {
        let mut v: Vec<_> = self.by_fingerprint.values().map(|a| a.record()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn len(&self) -> usize {
        self.by_fingerprint.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_fingerprint.is_empty()
    }
}

/// The `[core_api]` section of the server configuration.
#[derive(Debug, Clone)]
pub struct CoreApiConfig {
    pub bind: std::net::SocketAddr,
    /// Server certificate chain (PEM), issued by the steelcore CA.
    pub cert: PathBuf,
    /// Server private key (PEM).
    pub key: PathBuf,
    /// CA certificate(s) that issue client certificates (PEM).
    pub client_ca: PathBuf,
    pub accesses: AccessList,
    pub name_policy: crate::core_api::names::NamePolicy,
    pub max_connections: usize,
}

fn valid_access_name(s: &str) -> bool {
    let b = s.as_bytes();
    (2..=64).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.-".contains(c))
}

fn parse_access(t: &Table) -> Result<Access, String> {
    let s = |k: &str| t.get(k).and_then(|v| v.as_str()).ok_or_else(|| format!("core_api.access: missing {k}"));
    let name = s("name")?.to_owned();
    if !valid_access_name(&name) {
        return Err(format!("core_api.access: invalid name {name:?}"));
    }
    let provider = match t.get("provider").and_then(|v| v.as_str()) {
        None | Some("") => None,
        Some(p) => Some(Provider::from_id(p).ok_or_else(|| format!("core_api.access {name}: unknown provider {p:?}"))?),
    };
    let fp = s("cert_sha256")?.to_ascii_lowercase().replace(':', "");
    if fp.len() != 64 || !fp.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("core_api.access {name}: cert_sha256 must be 64 hex digits"));
    }
    let mut scopes = Vec::new();
    for v in t.get("scopes").and_then(|v| v.as_array()).ok_or_else(|| format!("core_api.access {name}: missing scopes"))? {
        let raw = v.as_str().unwrap_or("");
        let scope = Scope::parse(raw).ok_or_else(|| format!("core_api.access {name}: unknown scope {raw:?}"))?;
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    if (scopes.contains(&Scope::IdentityWrite) || scopes.contains(&Scope::ProfileWrite)) && provider.is_none() {
        return Err(format!("core_api.access {name}: identity:write and profile:write need a provider"));
    }
    let num = |k: &str, d: f64| -> Result<f64, String> {
        match t.get(k) {
            None => Ok(d),
            Some(v) => v
                .as_float()
                .or_else(|| v.as_integer().map(|i| i as f64))
                .filter(|x| *x > 0.0 && x.is_finite())
                .ok_or_else(|| format!("core_api.access {name}: {k} must be a positive number")),
        }
    };
    Ok(Access {
        rate_per_second: num("rate_per_second", 20.0)?,
        burst: num("burst", 40.0)?,
        name,
        provider,
        cert_sha256: fp,
        scopes,
    })
}

impl CoreApiConfig {
    /// Parse `[core_api]`. Returns Ok(None) if the section is absent (the
    /// listener is then not started).
    pub fn from_root(root: &Table) -> Result<Option<Self>, String> {
        let Some(t) = root.get("core_api") else {
            return Ok(None);
        };
        let t = t.as_table().ok_or("core_api must be a table")?;
        if t.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
            return Ok(None);
        }
        let s = |k: &str| t.get(k).and_then(|v| v.as_str()).ok_or_else(|| format!("core_api: missing {k}"));
        let bind: std::net::SocketAddr = s("bind")?.parse().map_err(|e| format!("core_api.bind: {e}"))?;
        let days = |k: &str, d: i64| -> Result<i64, String> {
            match t.get(k) {
                None => Ok(d),
                Some(v) => v
                    .as_integer()
                    .filter(|x| (0..=3650).contains(x))
                    .ok_or_else(|| format!("core_api.{k}: 0..3650 days")),
            }
        };
        let mut accesses = Vec::new();
        if let Some(list) = t.get("access") {
            for a in list.as_array().ok_or("core_api.access must be an array of tables")? {
                accesses.push(parse_access(a.as_table().ok_or("core_api.access entries must be tables")?)?);
            }
        }
        let max_connections = t
            .get("max_connections")
            .and_then(|v| v.as_integer())
            .map(|v| usize::try_from(v).map_err(|_| "core_api.max_connections"))
            .transpose()?
            .unwrap_or(64)
            .clamp(1, 4096);
        Ok(Some(Self {
            bind,
            cert: s("cert")?.into(),
            key: s("key")?.into(),
            client_ca: s("client_ca")?.into(),
            accesses: AccessList::new(accesses)?,
            name_policy: crate::core_api::names::NamePolicy {
                change_interval: days("display_name_change_days", 7)? * 86_400,
                reservation: days("display_name_reservation_days", 30)? * 86_400,
            },
            max_connections,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "aa00000000000000000000000000000000000000000000000000000000000000";

    fn root(extra: &str) -> Table {
        format!(
            "[core_api]\nbind = \"127.0.0.1:47101\"\ncert = \"s.crt\"\nkey = \"s.key\"\nclient_ca = \"ca.crt\"\n{extra}"
        )
        .parse()
        .unwrap()
    }

    #[test]
    fn absent_section_disables_the_listener() {
        assert!(CoreApiConfig::from_root(&"[network]\nbind_port = 1".parse().unwrap()).unwrap().is_none());
        assert!(CoreApiConfig::from_root(&root("enabled = false")).unwrap().is_none());
    }

    #[test]
    fn parses_accesses_and_policy() {
        let c = CoreApiConfig::from_root(&root(&format!(
            "display_name_change_days = 1\n[[core_api.access]]\nname = \"steelnet-eu\"\nprovider = \"steelbound\"\n\
             cert_sha256 = \"{}\"\nscopes = [\"identity:write\", \"profile:write\", \"read:public\"]\nrate_per_second = 5\n",
            FP.to_uppercase()
        )))
        .unwrap()
        .unwrap();
        assert_eq!(c.name_policy.change_interval, 86_400);
        assert_eq!(c.accesses.len(), 1);
        let r = &c.accesses.records()[0];
        assert_eq!((r.name.as_str(), r.provider, r.cert_sha256.as_str()), ("steelnet-eu", Some(Provider::Steelbound), FP));
    }

    #[test]
    fn rejects_bad_accesses() {
        for bad in [
            format!("name = \"x\"\nprovider = \"steelbound\"\ncert_sha256 = \"{FP}\"\nscopes = []"),
            format!("name = \"stats\"\ncert_sha256 = \"{FP}\"\nscopes = [\"profile:write\"]"),
            format!("name = \"stats\"\ncert_sha256 = \"{FP}\"\nscopes = [\"everything\"]"),
            format!("name = \"stats\"\nprovider = \"nope\"\ncert_sha256 = \"{FP}\"\nscopes = []"),
            "name = \"stats\"\ncert_sha256 = \"abc\"\nscopes = []".to_owned(),
        ] {
            assert!(CoreApiConfig::from_root(&root(&format!("[[core_api.access]]\n{bad}"))).is_err(), "{bad}");
        }
        let dup = format!(
            "[[core_api.access]]\nname = \"a1\"\ncert_sha256 = \"{FP}\"\nscopes = []\n\
             [[core_api.access]]\nname = \"a2\"\ncert_sha256 = \"{FP}\"\nscopes = []"
        );
        assert!(CoreApiConfig::from_root(&root(&dup)).is_err());
    }
}
