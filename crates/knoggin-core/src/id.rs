// SPDX-License-Identifier: AGPL-3.0-only
//! Identifiers. UUIDv7 values for stores, memories and users; a `u32` for namespaces.
//!
//! The engine never makes an ID: the host makes one and passes it in.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(
            Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord,
            Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Wraps a UUID. The host passes UUIDv7 values; nothing here checks the version.
            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// The UUID inside.
            pub const fn uuid(self) -> Uuid {
                self.0
            }

            /// Rebuilds the ID from its 16 big-endian bytes.
            pub const fn from_bytes(bytes: [u8; 16]) -> Self {
                Self(Uuid::from_bytes(bytes))
            }

            /// The 16 big-endian bytes, which sort the same way as the ID.
            pub const fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }
        }

        impl fmt::Display for $name {
            /// Lower-case hyphenated form.
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0.hyphenated(), f)
            }
        }

        impl From<Uuid> for $name {
            fn from(uuid: Uuid) -> Self {
                Self(uuid)
            }
        }
    };
}

uuid_id!(
    /// Identity of one store.
    StoreId
);
uuid_id!(
    /// Identity of one memory. UUIDv7, so the natural order is creation order.
    MemoryId
);
uuid_id!(
    /// Identity of one user.
    UserId
);

/// Identity of a namespace inside one store: sequential from 1, never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NamespaceId(u32);

impl NamespaceId {
    /// Wraps a raw value.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw value.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for NamespaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ns:{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_print_and_serialize_lower_case_hyphenated() {
        let id = MemoryId::from_bytes([0xAB; 16]);
        assert_eq!(id.to_string(), "abababab-abab-abab-abab-abababababab");
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            "\"abababab-abab-abab-abab-abababababab\""
        );
        let back: MemoryId =
            serde_json::from_str("\"abababab-abab-abab-abab-abababababab\"").unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn ids_order_by_bytes() {
        let a = MemoryId::from_bytes(1u128.to_be_bytes());
        let b = MemoryId::from_bytes(2u128.to_be_bytes());
        assert!(a < b);
        assert_eq!(MemoryId::from_bytes(*a.as_bytes()), a);
        assert_eq!(NamespaceId::new(7).to_string(), "ns:7");
    }
}
