//! Identifiers (domain.md "Identifiers").
//!
//! - Uuid-backed ids (v7, time-sortable) display as `prefix_<base32>`: the 128 bits rendered as
//!   26 Crockford base32 characters, lowercase (TypeID layout; the first character carries the top
//!   3 bits). `FromStr` accepts exactly that form. Serde uses the same string.
//! - Git object ids (`CommitSha`, `BlobOid`, `TreeOid`) are 20 bytes, lowercase hex.
//! - SHA-256 digests (`RecipeHash`, `ContentHash`) are 32 bytes, lowercase hex.
//!
//! Ids are minted by the `IdGen` port; this module only defines representation.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

/// Why an id string was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdParseError {
    #[error("expected prefix `{expected}_`")]
    WrongPrefix { expected: &'static str },
    #[error("expected {expected} characters, got {got}")]
    BadLength { expected: usize, got: usize },
    #[error("invalid character {0:?}")]
    BadChar(char),
    #[error("value out of range")]
    Overflow,
}

const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
const BASE32_LEN: usize = 26;

/// 128-bit value → 26 lowercase Crockford base32 characters (2 leading zero bits).
pub(crate) fn base32_encode(value: u128) -> [u8; BASE32_LEN] {
    let mut out = [0u8; BASE32_LEN];
    for (i, slot) in out.iter_mut().enumerate() {
        let shift = 5 * (BASE32_LEN - 1 - i);
        let idx = ((value >> shift) & 0x1f) as usize;
        *slot = CROCKFORD[idx];
    }
    out
}

pub(crate) fn base32_decode(s: &str) -> Result<u128, IdParseError> {
    if s.len() != BASE32_LEN {
        return Err(IdParseError::BadLength { expected: BASE32_LEN, got: s.len() });
    }
    let mut value: u128 = 0;
    for (i, c) in s.chars().enumerate() {
        let digit = CROCKFORD.iter().position(|&b| b as char == c).ok_or(IdParseError::BadChar(c))? as u128;
        // The first character holds only the top 3 bits of 128.
        if i == 0 && digit > 7 {
            return Err(IdParseError::Overflow);
        }
        value = (value << 5) | digit;
    }
    Ok(value)
}

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, ::ts_rs::TS)]
        pub struct $name(Uuid);

        impl $name {
            /// Display prefix (`{PREFIX}_<base32>`).
            pub const PREFIX: &'static str = $prefix;

            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            pub const fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl From<Uuid> for $name {
            fn from(uuid: Uuid) -> Self {
                Self(uuid)
            }
        }

        impl From<$name> for Uuid {
            fn from(id: $name) -> Uuid {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let encoded = base32_encode(self.0.as_u128());
                // CROCKFORD is ASCII, so the bytes are valid UTF-8.
                let text = std::str::from_utf8(&encoded).map_err(|_| fmt::Error)?;
                write!(f, "{}_{}", Self::PREFIX, text)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, f)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let rest = s
                    .strip_prefix(Self::PREFIX)
                    .and_then(|r| r.strip_prefix('_'))
                    .ok_or(IdParseError::WrongPrefix { expected: Self::PREFIX })?;
                Ok(Self(Uuid::from_u128(base32_decode(rest)?)))
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

uuid_id!(
    /// Tenant.
    OrgId, "org"
);
uuid_id!(UserId, "usr");
uuid_id!(RepoId, "repo");
uuid_id!(TicketId, "tkt");
uuid_id!(CauseId, "cause");
uuid_id!(RunId, "run");
uuid_id!(GateId, "gat");
uuid_id!(ArtifactId, "art");
uuid_id!(JobId, "job");
uuid_id!(RunnerId, "rnr");
uuid_id!(CustomerId, "cust");
uuid_id!(
    /// Versioned fix brief row (fix.md Additions).
    FixBriefId, "brief"
);
uuid_id!(
    /// `fix_pull_requests` row (fix.md Additions).
    FixPullRequestId, "fixpr"
);
uuid_id!(
    /// Triage eval (triage.md Additions).
    TriageEvalId, "teval"
);

/// Why a hex digest string was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HexParseError {
    #[error("expected {expected} hex characters, got {got}")]
    BadLength { expected: usize, got: usize },
    #[error("invalid hex")]
    BadHex,
}

pub(crate) fn decode_hex<const N: usize>(s: &str) -> Result<[u8; N], HexParseError> {
    if s.len() != 2 * N {
        return Err(HexParseError::BadLength { expected: 2 * N, got: s.len() });
    }
    let mut out = [0u8; N];
    hex::decode_to_slice(s, &mut out).map_err(|_| HexParseError::BadHex)?;
    Ok(out)
}

/// Fixed-size byte newtype with lowercase-hex `Display`/`FromStr`/serde.
macro_rules! hex_bytes_id {
    ($(#[$meta:meta])* $name:ident, $n:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, ::ts_rs::TS)]
        #[ts(type = "string")]
        pub struct $name(pub [u8; $n]);

        impl $name {
            pub const fn from_bytes(bytes: [u8; $n]) -> Self {
                Self(bytes)
            }

            pub const fn as_bytes(&self) -> &[u8; $n] {
                &self.0
            }

            pub fn to_hex(&self) -> String {
                ::hex::encode(self.0)
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                for b in &self.0 {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            }
        }

        impl ::std::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, "{}({})", stringify!($name), self)
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::domain::ids::HexParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                $crate::domain::ids::decode_hex::<$n>(s).map(Self)
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = <::std::borrow::Cow<'de, str> as ::serde::Deserialize<'de>>::deserialize(deserializer)?;
                s.parse().map_err(<D::Error as ::serde::de::Error>::custom)
            }
        }
    };
}
pub(crate) use hex_bytes_id;

hex_bytes_id!(
    /// Git commit id.
    CommitSha, 20
);
hex_bytes_id!(
    /// Git blob id; content-addressed cache key.
    BlobOid, 20
);
hex_bytes_id!(
    /// Git tree id.
    TreeOid, 20
);
hex_bytes_id!(
    /// SHA-256 of the canonical recipe TOML (executor.md).
    RecipeHash, 32
);
hex_bytes_id!(
    /// SHA-256 of a ticket's normalized content (intake.md Additions).
    ContentHash, 32
);

/// Identity of an indexed commit.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, ts_rs::TS)]
pub struct RepoVersionKey {
    pub repo_id: RepoId,
    pub sha: CommitSha,
}

impl RepoVersionKey {
    pub const fn new(repo_id: RepoId, sha: CommitSha) -> Self {
        Self { repo_id, sha }
    }
}

impl fmt::Display for RepoVersionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.repo_id, self.sha)
    }
}

/// GitHub App installation id (intake.md Additions).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, ts_rs::TS)]
#[serde(transparent)]
pub struct InstallationId(pub u64);

impl fmt::Display for InstallationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Serde helpers for raw `[u8; N]` digests stored as lowercase hex strings.
pub mod hex_array {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer, const N: usize>(bytes: &[u8; N], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(d: D) -> Result<[u8; N], D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        super::decode_hex::<N>(&s).map_err(serde::de::Error::custom)
    }

    /// `Option<[u8; N]>` as an optional hex string.
    pub mod option {
        use serde::{Deserialize, Deserializer, Serializer};

        pub fn serialize<S: Serializer, const N: usize>(bytes: &Option<[u8; N]>, s: S) -> Result<S::Ok, S::Error> {
            match bytes {
                Some(b) => s.serialize_some(&hex::encode(b)),
                None => s.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(d: D) -> Result<Option<[u8; N]>, D::Error> {
            let s = <Option<std::borrow::Cow<'de, str>>>::deserialize(d)?;
            s.map(|s| super::super::decode_hex::<N>(&s).map_err(serde::de::Error::custom)).transpose()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_id_round_trips_through_prefixed_base32() {
        let uuid = Uuid::from_u128(0x0190_1234_5678_7abc_8def_0123_4567_89ab);
        let id = RunId::from_uuid(uuid);
        let text = id.to_string();
        assert!(text.starts_with("run_"));
        assert_eq!(text.len(), "run_".len() + 26);
        assert_eq!(text.parse::<RunId>(), Ok(id));
    }

    #[test]
    fn uuid_id_boundaries() {
        for v in [0u128, u128::MAX, 1, 1 << 127] {
            let id = OrgId::from_uuid(Uuid::from_u128(v));
            assert_eq!(id.to_string().parse::<OrgId>(), Ok(id));
        }
        assert_eq!(OrgId::from_uuid(Uuid::nil()).to_string(), "org_00000000000000000000000000");
        assert_eq!(OrgId::from_uuid(Uuid::max()).to_string(), "org_7zzzzzzzzzzzzzzzzzzzzzzzzz");
    }

    #[test]
    fn uuid_id_rejects_malformed() {
        assert_eq!("tkt_00000000000000000000000000".parse::<RunId>(), Err(IdParseError::WrongPrefix { expected: "run" }));
        assert_eq!("run00000000000000000000000000".parse::<RunId>(), Err(IdParseError::WrongPrefix { expected: "run" }));
        assert_eq!("run_0000".parse::<RunId>(), Err(IdParseError::BadLength { expected: 26, got: 4 }));
        // 'u' is not in the Crockford alphabet; uppercase is not accepted.
        assert_eq!("run_0000000000000000000000000u".parse::<RunId>(), Err(IdParseError::BadChar('u')));
        assert_eq!("run_0000000000000000000000000A".parse::<RunId>(), Err(IdParseError::BadChar('A')));
        // First character > 7 would need 131 bits.
        assert_eq!("run_80000000000000000000000000".parse::<RunId>(), Err(IdParseError::Overflow));
    }

    #[test]
    fn uuid_id_order_follows_uuid_order() {
        let a = RunId::from_uuid(Uuid::from_u128(5));
        let b = RunId::from_uuid(Uuid::from_u128(6));
        assert!(a < b);
        assert!(a.to_string() < b.to_string());
    }

    #[test]
    fn uuid_id_serde_uses_prefixed_string() {
        let id = CauseId::from_uuid(Uuid::from_u128(42));
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<CauseId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<CauseId>("\"run_0000000000000000000000001a\"").is_err());
    }

    #[test]
    fn commit_sha_hex_round_trip_and_validation() {
        let text = "3f2a1c9e0b1d2c3e4f5a6b7c8d9e0f1a2b3c4d5e";
        let sha: CommitSha = text.parse().unwrap();
        assert_eq!(sha.to_string(), text);
        assert_eq!("3f2a".parse::<CommitSha>(), Err(HexParseError::BadLength { expected: 40, got: 4 }));
        assert_eq!("zz2a1c9e0b1d2c3e4f5a6b7c8d9e0f1a2b3c4d5e".parse::<CommitSha>(), Err(HexParseError::BadHex));
        // 64 hex chars is a RecipeHash, not a CommitSha.
        assert!(format!("{text}{}", "0".repeat(24)).parse::<CommitSha>().is_err());
        assert_eq!(serde_json::to_string(&sha).unwrap(), format!("\"{text}\""));
    }

    #[test]
    fn recipe_hash_is_32_bytes() {
        let h = RecipeHash::from_bytes([0xab; 32]);
        assert_eq!(h.to_string().len(), 64);
        assert_eq!(h.to_string().parse::<RecipeHash>(), Ok(h));
    }

    #[test]
    fn hex_array_serde_helper() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct S {
            #[serde(with = "hex_array")]
            a: [u8; 4],
            #[serde(with = "hex_array::option")]
            b: Option<[u8; 2]>,
        }
        let s = S { a: [1, 2, 3, 255], b: Some([0, 16]) };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#"{"a":"010203ff","b":"0010"}"#);
        assert_eq!(serde_json::from_str::<S>(&json).unwrap(), s);
        let none = S { a: [0; 4], b: None };
        assert_eq!(serde_json::from_str::<S>(&serde_json::to_string(&none).unwrap()).unwrap(), none);
        assert!(serde_json::from_str::<S>(r#"{"a":"0102","b":null}"#).is_err());
    }
}
