//! SFTP client (russh + russh-sftp) — M2 spike outcome.
//!
//! Scope notes:
//! - Password and public-key authentication.
//! - Host-key policy: standard OpenSSH known_hosts checking (hashed entries
//!   supported via russh::keys); unknown hosts are recorded (TOFU) in the
//!   config-dir known_hosts; a changed key aborts the transfer.
//! - Single-stream download (SFTP v3 has no native multi-range semantics;
//!   parallelism would need multiple sessions — deferred).
//!
//! Mirrors the original's psftp.dll (PuTTY SFTP) role.

use std::path::{Path, PathBuf};

use russh::client;
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::error::{Error, Result};
use crate::url::Url;

/// known_hosts-backed host key policy (TOFU on first contact).
pub struct HostKeyStore {
    path: PathBuf,
}

impl HostKeyStore {
    pub fn default_path() -> PathBuf {
        crate::settings::Settings::config_dir().join("known_hosts")
    }

    pub fn load(path: PathBuf) -> Self {
        HostKeyStore { path }
    }

    /// Standard OpenSSH check + TOFU record. Returns the decision.
    fn verify(&self, host: &str, port: u16, key: &russh::keys::PublicKey) -> bool {
        match russh::keys::check_known_hosts_path(host, port, key, &self.path) {
            Ok(true) => true,
            Ok(false) => {
                // unknown host → record and accept (TOFU)
                tracing::warn!(
                    host = %host,
                    fingerprint = %key.fingerprint(russh::keys::HashAlg::Sha256),
                    "sftp: recording new host key (TOFU)"
                );
                self.record(host, port, key);
                true
            }
            Err(e) => {
                tracing::error!("sftp: known_hosts check failed: {e}");
                false // KeyChanged / parse errors → reject
            }
        }
    }

    /// Append an OpenSSH-style known_hosts line.
    fn record(&self, host: &str, port: u16, key: &russh::keys::PublicKey) {
        let host_field = if port == 22 { host.to_string() } else { format!("[{host}]:{port}") };
        let Ok(blob) = key.to_openssh() else {
            return;
        };
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(f, "{host_field} {blob}");
        }
    }
}

/// SSH client handler with known_hosts + TOFU host-key checking.
pub struct KnownHostsHandler {
    store: HostKeyStore,
    host: String,
    port: u16,
}

impl client::Handler for KnownHostsHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let key = server_public_key.public_key();
        Ok(self.store.verify(&self.host, self.port, &key))
    }
}

/// Open an authenticated SSH session.
/// Auth order: public key (if `key_path` given) → password.
pub async fn connect_session(
    url: &Url,
    user: &str,
    pass: Option<&str>,
    key_path: Option<&Path>,
    key_passphrase: Option<&str>,
) -> Result<client::Handle<KnownHostsHandler>> {
    let config = std::sync::Arc::new(client::Config {
        nodelay: true,
        ..Default::default()
    });
    let handler = KnownHostsHandler {
        store: HostKeyStore::load(HostKeyStore::default_path()),
        host: url.host.clone(),
        port: url.port,
    };
    let mut session = client::connect(config, (url.host.as_str(), url.port), handler).await?;

    if let Some(kp) = key_path {
        let key = russh::keys::load_secret_key(kp, key_passphrase)
            .map_err(|e| Error::Auth(format!("load key '{}' failed: {e}", kp.display())))?;
        let auth = russh::keys::PrivateKeyWithHashAlg::new(std::sync::Arc::new(key), None);
        let r = session.authenticate_publickey(user, auth).await?;
        if r.success() {
            return Ok(session);
        }
        return Err(Error::Auth(format!("sftp public-key auth rejected for '{user}'")));
    }

    let pass = pass.ok_or_else(|| {
        Error::Auth("sftp needs credentials: -a user:pass or --key <privatekey>".into())
    })?;
    let r = session.authenticate_password(user, pass).await?;
    if r.success() {
        Ok(session)
    } else {
        Err(Error::Auth(format!("sftp password auth rejected for '{user}'")))
    }
}

/// Open the SFTP subsystem on a session channel and stat `path`.
/// Returns (sftp session, file size Option).
pub async fn open_remote(
    session: &mut client::Handle<KnownHostsHandler>,
    path: &str,
) -> Result<(SftpSession, Option<u64>)> {
    let channel = session.channel_open_session().await?;
    channel.request_subsystem(true, "sftp").await?;
    let sftp = SftpSession::new(channel.into_stream()).await?;
    let meta = sftp.metadata(path).await?;
    Ok((sftp, meta.size))
}

/// Stream a range [start, end) of the remote file into `sink` (called with
/// absolute file offsets). end == u64::MAX means read to EOF.
/// Returns bytes written.
pub async fn download_range<S>(
    sftp: &SftpSession,
    path: &str,
    start: u64,
    end: u64,
    mut sink: S,
) -> Result<u64>
where
    S: FnMut(u64, &[u8]),
{
    let mut file = sftp.open(path).await?;
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut offset = start;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            return Ok(offset - start);
        }
        sink(offset, &buf[..n]);
        offset += n as u64;
        if end != u64::MAX && offset >= end {
            return Ok(end - start);
        }
    }
}
