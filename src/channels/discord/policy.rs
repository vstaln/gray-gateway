//! Owner/allowlist admission + one-shot pairing.
use std::hint::black_box;

/// Admit one inbound message. Returns the cleaned prompt, or `None` to drop:
/// unknown/empty owner, bots, non-allowlisted senders, guild messages without
/// an @mention of the bot, and empty text after mention-stripping. A message
/// carrying `files` is admitted on its attachments alone — a voice note with
/// no caption is still a turn.
#[allow(clippy::too_many_arguments)]
pub fn incoming(
    author: &str,
    owner: &str,
    bot: bool,
    dm: bool,
    text: &str,
    bot_id: &str,
    allowed: &[String],
    files: bool,
) -> Option<String> {
    if owner.is_empty() || bot {
        return None;
    }
    if !is_allowed_user(author, owner, allowed) {
        return None;
    }
    let mut cleaned = text.to_string();
    if !dm {
        let plain = format!("<@{bot_id}>");
        let nick = format!("<@!{bot_id}>");
        if !text.contains(&plain) && !text.contains(&nick) {
            return None;
        }
        cleaned = cleaned.replace(&plain, "").replace(&nick, "");
    }
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() && !files {
        return None;
    }
    Some(cleaned)
}

/// Check if an author is authorized (owner or in allowed_users list).
pub fn is_allowed_user(author: &str, owner: &str, allowed: &[String]) -> bool {
    if owner.is_empty() || author.is_empty() {
        return false;
    }
    author == owner || allowed.iter().any(|id| id == author)
}

/// Admit one slash command invocation.
///
/// Sender must be owner or in `allowed_users`.
/// For `/ask`, requires a non-empty string prompt and returns `Some(prompt)`.
/// For `/new`, `/reset`, `/status`, and `/stop`, returns `Some(cmd_name)`.
pub fn slash_admission(
    user_id: &str,
    owner_id: &str,
    allowed: &[String],
    command_name: &str,
    prompt: Option<&str>,
) -> Option<String> {
    if !is_allowed_user(user_id, owner_id, allowed) {
        return None;
    }
    match command_name {
        "ask" => {
            let p = prompt?.trim();
            if p.is_empty() {
                None
            } else {
                Some(p.to_string())
            }
        }
        "new" | "reset" | "status" | "stop" => Some(command_name.to_string()),
        _ => None,
    }
}

/// One-shot local pairing code. Single-use, 300 s expiry.
pub struct Pairing {
    pub code: String,
    pub expires: f64,
    pub used: bool,
}

impl Pairing {
    pub fn new(now: f64) -> Self {
        Self {
            code: random_code(),
            expires: now + 300.0,
            used: false,
        }
    }

    pub fn accept(&mut self, text: &str, now: f64) -> bool {
        if self.used || now >= self.expires || !constant_eq(text, &self.code) {
            return false;
        }
        self.used = true;
        true
    }
}

/// Does this DM text carry the pairing code? Trim-tolerant, constant-time
/// comparison on the exact code (a contains-match would let any text
/// smuggle the code past the wizard).
pub fn code_matches(text: &str, code: &str) -> bool {
    constant_eq(text.trim(), code)
}

/// 18 random bytes, base64url without padding — like secrets.token_urlsafe(18).
fn random_code() -> String {
    let mut buf = [0u8; 18];
    #[cfg(unix)]
    let ok = {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut buf))
            .is_ok()
    };
    #[cfg(windows)]
    let ok = {
        use windows_sys::Win32::Security::Cryptography::BCryptGenRandom;
        const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
        // SAFETY: buf is writable for its length; a null provider handle asks
        // for the system-preferred RNG. Returns an NTSTATUS (0 = success).
        unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            ) == 0
        }
    };
    #[cfg(not(any(unix, windows)))]
    let ok = false;
    if !ok {
        // Degraded entropy path (no OS RNG readable): hash pid + time so the
        // code is still unpredictable to a remote peer. Documented fallback.
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(std::process::id().to_be_bytes());
        h.update(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
                .to_be_bytes(),
        );
        let d = h.finalize();
        buf.copy_from_slice(&d[..18]);
    }
    base64_nopad(&buf)
}

fn base64_nopad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(24);
    for chunk in bytes.chunks(3) {
        let mut n: u32 = 0;
        for (i, &b) in chunk.iter().enumerate() {
            n |= (b as u32) << (16 - 8 * i);
        }
        let chars = match chunk.len() {
            3 => 4,
            2 => 3,
            _ => 2,
        };
        for i in 0..chars {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// Constant-time string equality: no early exit on first mismatch.
fn constant_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for i in 0..a.len() {
        diff |= black_box(a[i] ^ b[i]);
    }
    black_box(diff) == 0
}
