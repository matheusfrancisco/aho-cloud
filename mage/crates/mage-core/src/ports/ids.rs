//! `IdGen` port (README "Port conventions"; domain.md "Identifiers"): every id is minted here so
//! tests are deterministic (`mage_core::testing::SeqIdGen`).

use uuid::Uuid;

pub trait IdGen: Send + Sync {
    /// Time-ordered UUID v7, for every entity id (storage.md "Ids").
    fn uuid_v7(&self) -> Uuid;
    /// Random UUID v4 (Linear comment `client_id`, intake.md "Notify").
    fn uuid_v4(&self) -> Uuid;
    /// 128 random bits (triage prompt nonce, triage.md "Prompt").
    fn nonce(&self) -> [u8; 16];
}

impl dyn IdGen + '_ {
    /// A typed id from a fresh v7 uuid: `ids.new_id::<RunId>()`.
    pub fn new_id<T: From<Uuid>>(&self) -> T {
        T::from(self.uuid_v7())
    }
}

/// Production generator: `Uuid::now_v7` (monotonic within the process) and OS randomness.
#[derive(Debug, Clone, Copy, Default)]
pub struct UuidV7Gen;

/// Version nibble and variant bits of a v4 uuid: the 6 bits that are not random.
const V4_FIXED_BITS: u128 = 0x0000_0000_0000_f000_c000_0000_0000_0000;

impl IdGen for UuidV7Gen {
    fn uuid_v7(&self) -> Uuid {
        Uuid::now_v7()
    }

    fn uuid_v4(&self) -> Uuid {
        Uuid::new_v4()
    }

    fn nonce(&self) -> [u8; 16] {
        // A v4 uuid carries 122 random bits; the 6 fixed bits are replaced by 6 random bits (the
        // low node bits) of a second v4 uuid.
        let a = Uuid::new_v4().as_u128();
        let spare = Uuid::new_v4().as_u128() & 0x3f;
        let fill = ((spare & 0xf) << 76) | ((spare >> 4) << 62);
        ((a & !V4_FIXED_BITS) | fill).to_be_bytes()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::domain::RunId;

    #[test]
    fn v7_ids_are_version_7_and_increasing() {
        let ids: &dyn IdGen = &UuidV7Gen;
        let a = ids.uuid_v7();
        let b = ids.uuid_v7();
        assert_eq!(a.get_version_num(), 7);
        assert!(a < b);
        let run: RunId = ids.new_id();
        assert_eq!(run.as_uuid().get_version_num(), 7);
        assert_eq!(ids.uuid_v4().get_version_num(), 4);
    }

    #[test]
    fn nonce_randomizes_the_bits_a_v4_uuid_fixes() {
        // Over 256 draws the 6 formerly fixed bits take more than one value unless they are
        // constant (probability of a false failure: 2^-1530).
        let fixed: BTreeSet<u128> = (0..256).map(|_| u128::from_be_bytes(UuidV7Gen.nonce()) & V4_FIXED_BITS).collect();
        assert!(fixed.len() > 1, "version/variant bits never vary: {fixed:x?}");
    }
}
