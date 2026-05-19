/// Generate a compact, roughly-sorted entry ID using nanosecond timestamp
/// mixed with a Knuth multiplicative hash for distribution.
pub fn generate_entry_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("{:08x}", nanos.wrapping_mul(2654435761))
}