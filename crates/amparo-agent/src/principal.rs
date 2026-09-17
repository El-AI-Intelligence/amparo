//! The kernel-minted agent principal (`agent:<fnv1a-hex>`) sent as the wire
//! spec's additive `agent_id` field (WIRE-SPEC §11).
//!
//! The kernel is the only minting authority (assembly D3): the principal is
//! an opaque id derived from the kernel's own hash function, and callers
//! never synthesize one. Amparo carries no `ellm-core` dependency, so this
//! module replicates the kernel's 56-bit FNV-1a hash byte-for-byte
//! (`ellm-core/src/lib.rs`, `fn fnv1a_56`) — the value is identical to what
//! the kernel mints for the same entity name.
//!
//! A caller that holds a session mints from the stable `session:<id>` entity
//! name; a caller with no identity sends [`ANONYMOUS_AGENT_ID`], and a caller
//! with no session at all leaves the field off the wire entirely.

/// The identity presented by a runtime that holds no kernel-minted agent id
/// — the wire spec's `agent_id` for unregistered callers.
pub const ANONYMOUS_AGENT_ID: &str = "anonymous";

/// FNV-1a, truncated to 56 bits — a byte-for-byte replica of the kernel's
/// `fnv1a_56`. This is exactly `EntityId::new(name, ENTITY_TYPE_AGENT).hash()`
/// with the 8-bit type tag masked off, so the helper never needs the kernel
/// crate to agree with it.
fn fnv1a_56(s: &str) -> u64 {
    const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
    const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
    let mut hash = FNV_OFFSET;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash & 0x00FF_FFFF_FFFF_FFFF
}

/// Mint the agent principal for a session id: `agent:<fnv1a-hex>` over the
/// kernel's `session:<id>` entity name. Stable across runs — the same
/// session id always mints the same principal.
pub fn mint_agent_id(session_id: &str) -> String {
    format!("agent:{:x}", fnv1a_56(&format!("session:{session_id}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_56_matches_the_kernel_vectors() {
        // Published FNV-1a 64-bit vectors, truncated to 56 bits — pins the
        // replica to the algorithm the kernel hashes entity names with.
        assert_eq!(fnv1a_56(""), 0x00F2_9CE4_8422_2325);
        assert_eq!(fnv1a_56("a"), 0x0063_DC4C_8601_EC8C);
        assert_eq!(fnv1a_56("foobar"), 0x0094_4171_F739_67E8);
    }

    #[test]
    fn minted_ids_are_stable_and_well_formed() {
        let id = mint_agent_id("web-1");
        assert_eq!(id, "agent:6cabf493b1ddbd");
        assert_eq!(id, mint_agent_id("web-1"), "minting is deterministic");
        assert_ne!(id, mint_agent_id("web-2"), "sessions mint distinct ids");
        // The kernel's `EntityId::from_agent_id` accepts at most 14 hex
        // digits — a 56-bit hash never exceeds that.
        let hex = id.strip_prefix("agent:").unwrap();
        assert!(hex.len() <= 14 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
