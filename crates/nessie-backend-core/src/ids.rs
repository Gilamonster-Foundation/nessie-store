//! Type-safe identifier newtypes.
//!
//! ONTAP keys volumes and snapshots by UUID. Wrapping each in a distinct newtype
//! means a `SnapshotUuid` can never be passed where a `VolumeUuid` is expected —
//! a class of bug the compiler now rejects. Each newtype is `#[serde(transparent)]`
//! so it serializes as a bare UUID string (the registry and wire shapes stay
//! plain text).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Generate a fresh random (v4) identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Borrow the inner [`Uuid`].
            #[must_use]
            pub fn as_uuid(&self) -> &Uuid {
                &self.0
            }

            /// Consume the newtype and return the inner [`Uuid`].
            #[must_use]
            pub fn into_uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<Uuid> for $name {
            fn from(u: Uuid) -> Self {
                Self(u)
            }
        }

        impl core::fmt::Display for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                core::fmt::Display::fmt(&self.0, f)
            }
        }

        impl core::str::FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(s)?))
            }
        }
    };
}

id_newtype! {
    /// Stable identifier for an ONTAP volume (a substrate dataset/bucket/stream).
    VolumeUuid
}

id_newtype! {
    /// Stable identifier for a snapshot of a volume.
    SnapshotUuid
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn distinct_ids_do_not_unify() {
        // This is a compile-time guarantee; the runtime assert just exercises it.
        let v = VolumeUuid::new();
        let s = SnapshotUuid::new();
        assert_ne!(v.to_string(), s.to_string());
    }

    #[test]
    fn display_roundtrips_through_fromstr() {
        let v = VolumeUuid::new();
        let parsed = VolumeUuid::from_str(&v.to_string()).expect("uuid parses");
        assert_eq!(v, parsed);
    }

    #[test]
    fn serializes_as_bare_uuid_string() {
        let v = VolumeUuid::from(Uuid::nil());
        let json = serde_json::to_string(&v).expect("serialize");
        assert_eq!(json, "\"00000000-0000-0000-0000-000000000000\"");
        let back: VolumeUuid = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(v, back);
    }
}
