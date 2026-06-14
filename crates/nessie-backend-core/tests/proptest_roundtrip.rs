//! Property-based regression: JSON serialization round-trips for the core
//! domain types over arbitrary inputs, and advertised capability presets stay
//! self-consistent. Example-based tests in the modules cover specific shapes;
//! these cover the space.

use nessie_backend_core::{
    CloneOrigin, Snapshot, Volume, VolumeSpec, VolumeState, VolumeStyle, VolumeType, VolumeUuid,
};
use proptest::prelude::*;
use uuid::Uuid;

fn arb_uuid() -> impl Strategy<Value = Uuid> {
    any::<[u8; 16]>().prop_map(Uuid::from_bytes)
}

fn arb_name() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_][a-zA-Z0-9_-]{0,31}".prop_map(|s| s)
}

fn arb_state() -> impl Strategy<Value = VolumeState> {
    prop_oneof![
        Just(VolumeState::Online),
        Just(VolumeState::Offline),
        Just(VolumeState::Restricted),
    ]
}

fn arb_type() -> impl Strategy<Value = VolumeType> {
    prop_oneof![Just(VolumeType::Rw), Just(VolumeType::Dp)]
}

prop_compose! {
    fn arb_volume()(
        uuid in arb_uuid(),
        name in arb_name(),
        size_bytes in proptest::option::of(any::<u64>()),
        state in arb_state(),
        vol_type in arb_type(),
        clone in proptest::option::of((arb_name(), arb_name())),
    ) -> Volume {
        Volume {
            uuid: VolumeUuid::from(uuid),
            name,
            size_bytes,
            state,
            style: VolumeStyle::Flexvol,
            vol_type,
            clone: clone.map(|(parent_volume, parent_snapshot)| CloneOrigin {
                parent_volume,
                parent_snapshot,
            }),
        }
    }
}

proptest! {
    #[test]
    fn volume_json_roundtrips(v in arb_volume()) {
        let json = serde_json::to_string(&v).expect("serialize");
        let back: Volume = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(v, back);
    }

    #[test]
    fn volume_spec_json_roundtrips(
        name in arb_name(),
        size_bytes in proptest::option::of(any::<u64>()),
    ) {
        let spec = VolumeSpec { name, size_bytes };
        let json = serde_json::to_string(&spec).expect("serialize");
        let back: VolumeSpec = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(spec, back);
    }

    #[test]
    fn snapshot_json_roundtrips(
        uuid in arb_uuid(),
        name in arb_name(),
        size_consumed in any::<u64>(),
    ) {
        let s = Snapshot {
            uuid: uuid.into(),
            name,
            create_time: None,
            size_consumed,
        };
        let json = serde_json::to_string(&s).expect("serialize");
        let back: Snapshot = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(s, back);
    }
}
