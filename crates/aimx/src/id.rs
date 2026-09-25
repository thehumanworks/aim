//! Random identifiers (process ids, workspace ids, resume tokens).

/// A random 128-bit identifier as lower-case hex.
pub(crate) fn random_hex() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // The OS RNG failing is not recoverable in a meaningful way; time plus a counter is still
        // unique within this process (never used for secrets when the RNG works).
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        return format!("{nanos:024x}{count:08x}");
    }
    hex(&bytes)
}

/// Lower-case hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// A random 256-bit secret as lower-case hex (resume tokens); `None` when the OS RNG fails.
pub(crate) fn secret_hex() -> Option<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).ok()?;
    Some(hex(&bytes))
}
