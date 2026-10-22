use std::io::Read;

/// A random (version 4) UUID, as Claude Code session ids are.
pub fn v4() -> String {
    let mut b = [0u8; 16];
    // /dev/urandom always exists on the platforms Choros runs on; a failed
    // read would leave zeros, which only costs uniqueness, so fall back to
    // mixing in the clock rather than failing a land over it.
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_err()
    {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes();
        b.copy_from_slice(&t);
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}
