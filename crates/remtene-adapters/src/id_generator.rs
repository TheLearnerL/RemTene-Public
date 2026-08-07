//! ID generator implementation using UUID v7.

use remtene_application::ports::IdGenerator;
use remtene_domain::{DeliveryId, RequestId, SessionId};

/// UUID v7 based ID generator.
///
/// UUID v7 provides time-ordered identifiers with good sortability.
pub struct UuidV7Generator;

impl UuidV7Generator {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for UuidV7Generator {
    fn default() -> Self {
        Self::new()
    }
}

impl IdGenerator for UuidV7Generator {
    fn session_id(&self) -> SessionId {
        SessionId::new()
    }

    fn request_id(&self) -> RequestId {
        RequestId::new()
    }

    fn delivery_id(&self) -> DeliveryId {
        DeliveryId::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_unique_session_ids() {
        let generator = UuidV7Generator::new();
        let id1 = generator.session_id();
        let id2 = generator.session_id();
        assert_ne!(id1, id2);
    }

    #[test]
    fn generates_unique_request_ids() {
        let generator = UuidV7Generator::new();
        let id1 = generator.request_id();
        let id2 = generator.request_id();
        assert_ne!(id1, id2);
    }

    #[test]
    fn generates_unique_delivery_ids() {
        let generator = UuidV7Generator::new();
        let id1 = generator.delivery_id();
        let id2 = generator.delivery_id();
        assert_ne!(id1, id2);
    }
}
