//! Minimal OpenSSH `known_hosts` support for verifying SSH host keys presented
//! to libgit2/libssh2.
//!
//! git2's `certificate_check` callback discards libgit2's own known_hosts verdict
//! (the `valid` argument is dropped in the Rust binding), so to implement
//! trust-on-first-use we read `~/.ssh/known_hosts` ourselves to classify a
//! presented key as [`HostKeyStatus::Match`], [`HostKeyStatus::Changed`] or
//! [`HostKeyStatus::Unknown`], and append accepted keys back to the file.
//!
//! Both plain (`host keytype base64key`) and hashed (`|1|salt|hash`) host
//! patterns are supported, matching what OpenSSH writes by default.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

type HmacSha1 = Hmac<Sha1>;

/// Result of checking a presented host key against the user's known_hosts files.
#[derive(Debug, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// The host is known and the presented key matches a stored one.
    Match,
    /// The host is known for this key type but the presented key differs
    /// (potential man-in-the-middle — refuse loudly).
    Changed,
    /// The host is not in any known_hosts file (offer trust-on-first-use).
    Unknown,
}

enum LineMatch {
    Match,
    Changed,
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Path to the user's primary known_hosts file (where new entries are written).
pub fn user_known_hosts() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".ssh").join("known_hosts"))
}

fn candidate_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(home) = home_dir() {
        let ssh = home.join(".ssh");
        files.push(ssh.join("known_hosts"));
        files.push(ssh.join("known_hosts2"));
    }
    files.push(PathBuf::from("/etc/ssh/ssh_known_hosts"));
    files
}

/// Format a raw SHA-256 host key hash the way OpenSSH prints it.
pub fn fingerprint_sha256(hash: &[u8]) -> String {
    format!("SHA256:{}", STANDARD_NO_PAD.encode(hash))
}

/// Check a presented host key against every readable known_hosts file.
pub fn check(host: &str, key_type: &str, raw_key: &[u8]) -> HostKeyStatus {
    let mut changed = false;
    for path in candidate_files() {
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines() {
            match classify_line(line, host, key_type, raw_key) {
                Some(LineMatch::Match) => return HostKeyStatus::Match,
                Some(LineMatch::Changed) => changed = true,
                None => {}
            }
        }
    }
    if changed {
        HostKeyStatus::Changed
    } else {
        HostKeyStatus::Unknown
    }
}

/// Append a plain (unhashed) entry for `host` to the user's known_hosts file,
/// creating `~/.ssh` and the file with restrictive permissions if needed.
pub fn add(host: &str, key_type: &str, raw_key: &[u8]) -> std::io::Result<PathBuf> {
    let path = user_known_hosts().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "home directory not found")
    })?;

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }
    }

    let line = format!("{} {} {}\n", host, key_type, STANDARD.encode(raw_key));
    let mut opts = fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&path)?;
    file.write_all(line.as_bytes())?;
    Ok(path)
}

fn classify_line(line: &str, host: &str, key_type: &str, raw_key: &[u8]) -> Option<LineMatch> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let mut fields = line.split_whitespace();
    let mut patterns = fields.next()?;
    // Skip an optional @cert-authority / @revoked marker.
    if patterns.starts_with('@') {
        patterns = fields.next()?;
    }
    let entry_type = fields.next()?;
    let entry_key = fields.next()?;

    if !host_matches(patterns, host) {
        return None;
    }
    if entry_type != key_type {
        return None;
    }

    match STANDARD.decode(entry_key) {
        Ok(bytes) if bytes == raw_key => Some(LineMatch::Match),
        Ok(_) => Some(LineMatch::Changed),
        Err(_) => None,
    }
}

fn host_matches(patterns: &str, host: &str) -> bool {
    for pat in patterns.split(',') {
        let pat = pat.trim();
        if pat.is_empty() {
            continue;
        }
        if let Some(rest) = pat.strip_prefix("|1|") {
            if hashed_matches(rest, host) {
                return true;
            }
        } else if glob_match(&normalize_pattern(pat), host) {
            return true;
        }
    }
    false
}

/// Strip an optional `[host]:port` wrapper down to the bare host.
fn normalize_pattern(pat: &str) -> String {
    if let Some(inner) = pat.strip_prefix('[')
        && let Some(idx) = inner.find("]:")
    {
        return inner[..idx].to_string();
    }
    pat.to_string()
}

fn hashed_matches(rest: &str, host: &str) -> bool {
    let (salt_b64, hash_b64) = match rest.split_once('|') {
        Some(parts) => parts,
        None => return false,
    };
    let (Ok(salt), Ok(expected)) = (STANDARD.decode(salt_b64), STANDARD.decode(hash_b64)) else {
        return false;
    };
    let Ok(mut mac) = HmacSha1::new_from_slice(&salt) else {
        return false;
    };
    mac.update(host.as_bytes());
    mac.finalize().into_bytes().as_slice() == expected.as_slice()
}

/// Case-insensitive shell-style match supporting `*` and `?`, as used for
/// known_hosts host patterns.
fn glob_match(pattern: &str, text: &str) -> bool {
    glob_bytes(
        pattern.to_ascii_lowercase().as_bytes(),
        text.to_ascii_lowercase().as_bytes(),
    )
}

fn glob_bytes(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.first() {
        None => text.is_empty(),
        Some(b'*') => {
            // Match zero characters, or one character then retry the '*'.
            glob_bytes(&pattern[1..], text) || (!text.is_empty() && glob_bytes(pattern, &text[1..]))
        }
        Some(b'?') => !text.is_empty() && glob_bytes(&pattern[1..], &text[1..]),
        Some(&c) => !text.is_empty() && text[0] == c && glob_bytes(&pattern[1..], &text[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashed_pattern(host: &str, salt: &[u8]) -> String {
        let mut mac = HmacSha1::new_from_slice(salt).unwrap();
        mac.update(host.as_bytes());
        let digest = mac.finalize().into_bytes();
        format!("|1|{}|{}", STANDARD.encode(salt), STANDARD.encode(digest))
    }

    #[test]
    fn plain_host_matches_exactly() {
        assert!(host_matches("example.com", "example.com"));
        assert!(!host_matches("example.com", "other.com"));
    }

    #[test]
    fn host_match_is_case_insensitive() {
        assert!(host_matches("Example.COM", "example.com"));
    }

    #[test]
    fn comma_separated_and_wildcards_match() {
        assert!(host_matches("a.com,b.com", "b.com"));
        assert!(host_matches("*.example.com", "git.example.com"));
        assert!(!host_matches("*.example.com", "example.com"));
    }

    #[test]
    fn bracket_port_pattern_is_normalized() {
        assert!(host_matches("[git.example.com]:2222", "git.example.com"));
    }

    #[test]
    fn hashed_host_matches_via_hmac() {
        let pat = hashed_pattern("git.interligent.com", b"some-salt-bytes");
        assert!(host_matches(&pat, "git.interligent.com"));
        assert!(!host_matches(&pat, "evil.example.com"));
    }

    #[test]
    fn classify_detects_match_change_and_irrelevant() {
        let key = b"\x00\x01\x02\x03raw-key-bytes";
        let b64 = STANDARD.encode(key);
        let line = format!("example.com ssh-ed25519 {b64}");

        assert!(matches!(
            classify_line(&line, "example.com", "ssh-ed25519", key),
            Some(LineMatch::Match)
        ));
        assert!(matches!(
            classify_line(&line, "example.com", "ssh-ed25519", b"different"),
            Some(LineMatch::Changed)
        ));
        // Different host or key type is not our concern.
        assert!(classify_line(&line, "other.com", "ssh-ed25519", key).is_none());
        assert!(classify_line(&line, "example.com", "ssh-rsa", key).is_none());
    }

    #[test]
    fn classify_skips_comments_and_markers() {
        let key = b"abc";
        let b64 = STANDARD.encode(key);
        assert!(classify_line("# a comment", "example.com", "ssh-ed25519", key).is_none());
        let marked = format!("@cert-authority example.com ssh-ed25519 {b64}");
        assert!(matches!(
            classify_line(&marked, "example.com", "ssh-ed25519", key),
            Some(LineMatch::Match)
        ));
    }

    #[test]
    fn fingerprint_matches_openssh_format() {
        // 32 zero bytes -> known base64 (no padding).
        let fp = fingerprint_sha256(&[0u8; 32]);
        assert_eq!(fp, "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    }
}
