//! Self-signed CA + leaf generation and the rustls acceptor for native
//! HTTPS termination. Design goals (public project, domain-less LAN):
//! - first run generates a private CA + a leaf covering the configured
//!   hosts (DNS names and IP SANs);
//! - clients load the CA via `CODEX_CA_CERTIFICATE` — no system trust
//!   store changes, no domain purchase;
//! - certs live under the state dir and persist across restarts.

use crate::config::TlsCfg;
use rcgen::{CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct TlsMaterial {
    pub server_config: Arc<rustls::ServerConfig>,
    pub ca_path: std::path::PathBuf,
}

/// Generates (or reuses) the CA and leaf for the configured hosts. Regenerates
/// the leaf when the host list changed; the CA is only recreated when absent.
pub fn ensure_material(state_root: &str, cfg: &TlsCfg) -> Result<PathBuf, String> {
    let (ca_path, cert_path, key_path) = {
        let (c, s, k) = (
            Path::new(state_root).join("herdex-ca.pem"),
            Path::new(state_root).join("herdex-server.pem"),
            Path::new(state_root).join("herdex-server.key"),
        );
        (c, s, k)
    };
    let _ = ca_path;
    let hosts = cfg.enabled_hosts();
    let fingerprint = hosts.join("\n");
    let meta_path = Path::new(state_root).join("herdex-tls-hosts.txt");
    let needs_issue = match (
        std::fs::read(&cert_path),
        std::fs::read_to_string(&meta_path),
    ) {
        (Ok(existing), Ok(previous)) => existing.is_empty() || previous != fingerprint,
        (Ok(_), Err(_)) => true,
        _ => true,
    };
    if !needs_issue {
        return Ok(ca_path);
    }

    // CA (persisted so the same CA signs renewed leaves)
    let ca_key_path = Path::new(state_root).join("herdex-ca.key");
    let ca_key = load_or_create_ca_key(&ca_key_path)?;
    let mut ca_params = CertificateParams::new(vec![]).map_err(|e| e.to_string())?;
    ca_params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "herdex local CA");
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca_issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
    let ca_cert = ca_params
        .self_signed(&ca_key)
        .map_err(|e| format!("CA cert: {e}"))?;

    // leaf: SANs for the configured hosts
    let mut params = CertificateParams::default();
    for host in &cfg.enabled_hosts() {
        if let Ok(ip) = host.parse::<IpAddr>() {
            params.subject_alt_names.push(rcgen::SanType::IpAddress(ip));
        } else {
            params.subject_alt_names.push(rcgen::SanType::DnsName(
                rcgen::string::Ia5String::try_from(host.clone()).map_err(|e| e.to_string())?,
            ));
        }
    }
    params
        .distinguished_name
        .push(DnType::CommonName, hosts_cn(cfg));
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let leaf_key = KeyPair::generate().map_err(|e| e.to_string())?;
    let leaf = params
        .signed_by(&leaf_key, &ca_issuer)
        .map_err(|e| e.to_string())?;

    std::fs::write(&cert_path, leaf.pem()).map_err(|e| e.to_string())?;
    std::fs::write(&key_path, leaf_key.serialize_pem()).map_err(|e| e.to_string())?;
    std::fs::write(&ca_path, ca_cert.pem()).map_err(|e| e.to_string())?;
    std::fs::write(&meta_path, fingerprint).map_err(|e| e.to_string())?;
    restrict_key_permissions(&key_path);
    Ok(ca_path)
}

fn hosts_cn(cfg: &TlsCfg) -> String {
    cfg.enabled_hosts().first().cloned().unwrap_or_default()
}

/// The CA key must be persisted separately from the CA certificate: the
/// .pem holds the cert (public material, served at /ca.pem) while the key
/// must outlive restarts so every leaf stays signed by the same CA.
fn load_or_create_ca_key(key_path: &Path) -> Result<KeyPair, String> {
    if let Ok(existing) = std::fs::read_to_string(key_path) {
        if !existing.trim().is_empty() {
            return KeyPair::from_pem(&existing).map_err(|e| e.to_string());
        }
    }
    let key = KeyPair::generate().map_err(|e| e.to_string())?;
    std::fs::write(key_path, key.serialize_pem()).map_err(|e| e.to_string())?;
    restrict_key_permissions(key_path);
    Ok(key)
}

fn restrict_key_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perm = meta.permissions();
            perm.set_mode(0o600);
            let _ = std::fs::set_permissions(path, perm);
        }
    }
}

/// Loads the generated material into a rustls server config.
pub fn server_config(state_root: &str, cfg: &TlsCfg) -> Result<Arc<rustls::ServerConfig>, String> {
    ensure_material(state_root, cfg)?;
    let cert_path = Path::new(state_root).join("herdex-server.pem");
    let key_path = Path::new(state_root).join("herdex-server.key");
    let certs: Vec<_> = rustls_pemfile::certs(&mut std::io::BufReader::new(
        std::fs::File::open(&cert_path).map_err(|e| e.to_string())?,
    ))
    .collect::<Result<_, _>>()
    .map_err(|e| e.to_string())?;
    let key_pem = std::fs::read_to_string(&key_path).map_err(|e| e.to_string())?;
    let mut pem_reader = std::io::BufReader::new(key_pem.as_bytes());
    let key_der = rustls_pemfile::private_key(&mut pem_reader)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "empty server key".to_string())?;
    let cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key_der)
        .map_err(|e| e.to_string())?;
    Ok(Arc::new(cfg))
}
