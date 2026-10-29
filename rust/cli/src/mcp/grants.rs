// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Who may call the remote MCP server: registered clients, the pairing code,
//! authorization codes in flight, and the grants they turned into.
//!
//! One JSON file, `mcp-grants.json` in the state directory, `0600`. Nothing
//! secret is kept in the clear — every code and token is stored as its SHA-256,
//! so the file tells a reader who is connected but lets nobody connect.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

/// The file under the state directory.
pub const FILE: &str = "mcp-grants.json";

/// Everything the authorization server remembers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Grants {
    #[serde(default)]
    pub clients: Vec<Client>,
    /// The one pairing code that can approve a connection, if one was issued.
    /// It is bound to one open approval request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing: Option<Pairing>,
    /// Approval pages that are open: one per `GET /authorize`, waiting for a
    /// pairing code. There is no global lockout any more (#103) — wrong codes
    /// count against the request they were typed for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requests: Vec<Pending>,
    #[serde(default)]
    pub codes: Vec<AuthCode>,
    #[serde(default)]
    pub grants: Vec<Grant>,
}

/// A client that registered itself (RFC 7591).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Client {
    pub client_id: String,
    #[serde(default)]
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    /// Empty for a public client (`token_endpoint_auth_method: none`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub secret_hash: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// A pairing code issued by `herdr-tower mcp pair`, good for one request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pairing {
    /// The [`Pending::id`] it approves. Empty in a file written before codes
    /// were bound to requests, and then it approves nothing.
    #[serde(default)]
    pub request: String,
    pub code_hash: String,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// An approval page someone opened, waiting for its pairing code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    /// Shown on the page and given to `mcp pair` to say which page a code is for.
    pub id: String,
    pub client_id: String,
    #[serde(default)]
    pub client_name: String,
    pub redirect_uri: String,
    pub challenge: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    pub scopes: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    /// Wrong codes typed on this page.
    #[serde(default)]
    pub failures: u32,
}

/// An authorization code between `/authorize` and `/token`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthCode {
    pub code_hash: String,
    pub client_id: String,
    pub redirect_uri: String,
    /// The PKCE S256 challenge.
    pub challenge: String,
    pub scopes: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// One approved connection: a live access token and the refresh token that
/// renews it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    /// Short, for `mcp grants` and `mcp revoke`.
    pub id: String,
    pub client_id: String,
    #[serde(default)]
    pub client_name: String,
    pub scopes: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_used: Option<OffsetDateTime>,
    pub access_hash: String,
    #[serde(with = "time::serde::rfc3339")]
    pub access_expires: OffsetDateTime,
    pub refresh_hash: String,
    #[serde(with = "time::serde::rfc3339")]
    pub refresh_expires: OffsetDateTime,
    /// Refresh tokens already exchanged. Seeing one again means a copy of it
    /// is in someone else's hands, and the grant is revoked (OAuth 2.1 §4.3.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spent_refresh: Vec<String>,
}

impl Grants {
    /// Forgets whatever can no longer be used: expired codes and pairing,
    /// grants past their refresh lifetime, and clients that registered but
    /// never finished connecting within a day.
    pub fn prune(&mut self, now: OffsetDateTime) {
        self.codes.retain(|c| c.expires_at > now);
        self.requests.retain(|r| r.expires_at > now);
        let requests = &self.requests;
        if self
            .pairing
            .as_ref()
            .is_some_and(|p| p.expires_at <= now || !requests.iter().any(|r| r.id == p.request))
        {
            self.pairing = None;
        }
        self.grants.retain(|g| g.refresh_expires > now);
        let grants = &self.grants;
        let codes = &self.codes;
        self.clients.retain(|c| {
            now - c.created_at < time::Duration::days(1)
                || grants.iter().any(|g| g.client_id == c.client_id)
                || codes.iter().any(|k| k.client_id == c.client_id)
                || requests.iter().any(|r| r.client_id == c.client_id)
        });
    }

    pub fn client(&self, client_id: &str) -> Option<&Client> {
        self.clients.iter().find(|c| c.client_id == client_id)
    }
}

/// The SHA-256 of a secret, in hex. How every code and token is stored.
pub fn hash(secret: &str) -> String {
    hex(&Sha256::digest(secret.as_bytes()))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `n` random bytes from the operating system.
pub fn random_bytes(n: usize) -> io::Result<Vec<u8>> {
    tower_os::random_bytes(n)
}

/// A token: 256 random bits, base64url without padding.
pub fn random_token() -> io::Result<String> {
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes(32)?))
}

/// A six-digit code, uniform over 000000–999999.
pub fn random_code() -> io::Result<String> {
    loop {
        let b = random_bytes(4)?;
        let n = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        // Rejection sampling: the top sliver of the u32 range would make the
        // low codes slightly likelier than the rest.
        if n < 4_294_000_000 {
            return Ok(format!("{:06}", n % 1_000_000));
        }
    }
}

/// The file, under the state directory.
pub fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// Reads the grants. A missing file is an empty set; an unreadable one is an
/// error, never an empty set — treating it as empty would let the next write
/// throw away every live grant along with the lockout.
pub fn load(dir: &Path) -> io::Result<Grants> {
    match fs::read(path(dir)) {
        Ok(body) => {
            serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Grants::default()),
        Err(e) => Err(e),
    }
}

/// Reads, changes and writes the grants under one lock, pruned first.
///
/// The server and the `mcp pair` / `mcp revoke` commands are separate
/// processes writing the same file, so the read and the write have to be one
/// step.
///
/// The file is written only when something in it changed. Every `/token`,
/// `/register` and `POST /authorize` comes through here, unauthenticated, and
/// a write is a temporary file, an `fsync` and a rename under the lock: a
/// flood of requests with a made-up request id or client used to serialise
/// on that `fsync` and rewrite the file each time (#27). A refusal changes
/// nothing, so it writes nothing; what expired is dropped by the first write
/// after it expired and ignored by every read until then.
pub fn update<T>(
    dir: &Path,
    now: OffsetDateTime,
    f: impl FnOnce(&mut Grants) -> T,
) -> io::Result<T> {
    fs::create_dir_all(dir)?;
    let _lock = Lock::acquire(&dir.join("mcp-grants.lock"))?;
    let before = load(dir)?;
    let mut g = before.clone();
    g.prune(now);
    let out = f(&mut g);
    if g != before {
        write(dir, &g)?;
    }
    Ok(out)
}

fn write(dir: &Path, g: &Grants) -> io::Result<()> {
    let body = serde_json::to_vec_pretty(g)?;
    let tmp = dir.join(format!("{FILE}.tmp"));
    {
        let mut f = tower_os::private(
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true),
        )
        .open(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    // A file created before the mode was set keeps its old one; `mode` only
    // applies on creation.
    tower_os::set_mode(&tmp, 0o600)?;
    fs::rename(&tmp, path(dir))
}

/// An exclusive `flock` held for as long as the value lives.
pub(crate) struct Lock(fs::File);

impl Lock {
    pub(crate) fn acquire(path: &Path) -> io::Result<Self> {
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        // `flock` on Unix, `LockFileEx` on Windows.
        f.lock()?;
        Ok(Lock(f))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Closing the file would release it too.
        let _ = self.0.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000) + time::Duration::seconds(secs)
    }

    #[test]
    fn hashes_are_sha256_hex() {
        // FIPS 180-2 test vector.
        assert_eq!(
            hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn tokens_and_codes_have_their_shapes() {
        let t = random_token().unwrap();
        assert_eq!(t.len(), 43, "32 bytes, base64url, no padding");
        assert!(t
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_ne!(t, random_token().unwrap());
        let c = random_code().unwrap();
        assert_eq!(c.len(), 6);
        assert!(c.bytes().all(|b| b.is_ascii_digit()));
    }

    #[test]
    fn the_file_is_private_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        update(dir.path(), at(0), |g| {
            g.pairing = Some(Pairing {
                request: "ABC123".into(),
                code_hash: hash("123456"),
                expires_at: at(300),
            })
        })
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path(dir.path())).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let g = load(dir.path()).unwrap();
        assert_eq!(g.pairing.unwrap().code_hash, hash("123456"));
        let body = fs::read_to_string(path(dir.path())).unwrap();
        assert!(!body.contains("123456"), "only the hash is written");
    }

    #[test]
    fn an_unreadable_file_is_an_error_not_an_empty_set() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(path(dir.path()), "{not json").unwrap();
        assert!(load(dir.path()).is_err());
        assert!(update(dir.path(), at(0), |_| ()).is_err());
        assert_eq!(fs::read_to_string(path(dir.path())).unwrap(), "{not json");
    }

    #[test]
    fn prune_forgets_only_what_can_no_longer_be_used() {
        let client = |id: &str, created: i64| Client {
            client_id: id.into(),
            client_name: String::new(),
            redirect_uris: vec![],
            secret_hash: String::new(),
            created_at: at(created),
        };
        let grant = |client: &str, refresh_expires: i64| Grant {
            id: format!("g-{client}"),
            client_id: client.into(),
            client_name: String::new(),
            scopes: vec![],
            created_at: at(0),
            last_used: None,
            access_hash: String::new(),
            access_expires: at(0),
            refresh_hash: String::new(),
            refresh_expires: at(refresh_expires),
            spent_refresh: vec![],
        };
        let day = 86_400;
        let mut g = Grants {
            clients: vec![
                client("fresh", 2 * day - 10),
                client("abandoned", 0),
                client("connected", 0),
            ],
            pairing: Some(Pairing {
                request: "OLD".into(),
                code_hash: String::new(),
                expires_at: at(2 * day - 1),
            }),
            requests: vec![pending("OLD", "abandoned", 2 * day - 1)],
            codes: vec![],
            grants: vec![grant("connected", 3 * day), grant("expired", 2 * day)],
        };
        g.prune(at(2 * day));
        let ids: Vec<&str> = g.clients.iter().map(|c| c.client_id.as_str()).collect();
        assert_eq!(ids, ["fresh", "connected"]);
        assert_eq!(g.grants.len(), 1);
        assert!(g.pairing.is_none());
        assert!(g.requests.is_empty(), "an expired approval page is closed");
    }

    fn pending(id: &str, client: &str, expires: i64) -> Pending {
        Pending {
            id: id.into(),
            client_id: client.into(),
            client_name: String::new(),
            redirect_uri: String::new(),
            challenge: String::new(),
            state: None,
            scopes: vec![],
            created_at: at(0),
            expires_at: at(expires),
            failures: 0,
        }
    }

    /// A code outlives nothing: its page closing takes it along, and a client
    /// with an open page is kept while the page is.
    #[test]
    fn a_pairing_goes_with_its_request() {
        let mut g = Grants {
            requests: vec![pending("KEEP", "c1", 100)],
            pairing: Some(Pairing {
                request: "GONE".into(),
                code_hash: String::new(),
                expires_at: at(100),
            }),
            clients: vec![Client {
                client_id: "c1".into(),
                client_name: String::new(),
                redirect_uris: vec![],
                secret_hash: String::new(),
                created_at: at(-3 * 86_400),
            }],
            ..Grants::default()
        };
        g.prune(at(0));
        assert!(g.pairing.is_none(), "bound to a request that is not open");
        assert_eq!(g.clients.len(), 1, "its page is still open");
    }

    /// #27: an update that changes nothing leaves the file alone — no new
    /// file renamed over it — and one that changes something writes it.
    #[cfg(unix)]
    #[test]
    fn an_update_that_changes_nothing_writes_nothing() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        update(dir.path(), at(0), |g| {
            g.requests.push(pending("KEEP", "c1", 600))
        })
        .unwrap();
        let inode = || fs::metadata(path(dir.path())).unwrap().ino();
        let first = inode();
        let seen = update(dir.path(), at(1), |g| g.requests.len()).unwrap();
        assert_eq!(seen, 1);
        assert_eq!(inode(), first, "a read-only update rewrote the file");
        // Nothing at all to write for a fresh directory either.
        let empty = tempfile::tempdir().unwrap();
        update(empty.path(), at(0), |_| ()).unwrap();
        assert!(!path(empty.path()).exists());
        update(dir.path(), at(2), |g| g.requests[0].failures += 1).unwrap();
        assert_ne!(inode(), first, "a change is written");
        assert_eq!(load(dir.path()).unwrap().requests[0].failures, 1);
        // What expired is dropped by the next update, a write like any other.
        let late = inode();
        update(dir.path(), at(601), |_| ()).unwrap();
        assert_ne!(inode(), late);
        assert!(load(dir.path()).unwrap().requests.is_empty());
    }

    /// A file written before codes were bound to requests still loads, and its
    /// unbound code approves nothing.
    #[test]
    fn an_older_file_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            path(dir.path()),
            r#"{"clients":[],"pairing":{"code_hash":"x","expires_at":"2099-01-01T00:00:00Z","failures":2},"locked_until":"2099-01-01T00:00:00Z","codes":[],"grants":[]}"#,
        )
        .unwrap();
        let g = load(dir.path()).unwrap();
        assert_eq!(g.pairing.as_ref().unwrap().request, "");
        let mut g = g;
        g.prune(at(0));
        assert!(g.pairing.is_none());
    }
}
