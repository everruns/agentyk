//! A typed extension bag — axum's `Extensions` pattern, so
//! [`crate::tool::ToolContext`] can carry arbitrary host-injected services
//! (a credential store, a workspace handle, anything) without core knowing
//! what they are. A tool that needs one downcasts by type; core stays free
//! of any concrete host concern (no `SessionFileSystem`, no credential
//! store type, nothing DB- or server-shaped in this crate).

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

/// Type-keyed bag of `Arc<T>` values, one per type. Cheap to clone (each
/// entry is an `Arc`).
#[derive(Clone, Default)]
pub struct Extensions(HashMap<TypeId, Arc<dyn Any + Send + Sync>>);

impl Extensions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a value, replacing any previous value of the same type.
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) {
        self.0.insert(TypeId::of::<T>(), Arc::new(value));
    }

    /// Fetch a value by type, if one was inserted.
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.0
            .get(&TypeId::of::<T>())
            .and_then(|value| value.clone().downcast::<T>().ok())
    }

    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.0.contains_key(&TypeId::of::<T>())
    }
}

impl std::fmt::Debug for Extensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extensions")
            .field("len", &self.0.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Widget(u32);
    struct Gadget;

    #[test]
    fn insert_and_get_round_trip_by_type() {
        let mut extensions = Extensions::new();
        extensions.insert(Widget(7));
        assert_eq!(*extensions.get::<Widget>().unwrap(), Widget(7));
        assert!(extensions.get::<Gadget>().is_none());
    }

    #[test]
    fn insert_replaces_the_previous_value_of_the_same_type() {
        let mut extensions = Extensions::new();
        extensions.insert(Widget(1));
        extensions.insert(Widget(2));
        assert_eq!(*extensions.get::<Widget>().unwrap(), Widget(2));
    }

    #[test]
    fn clone_shares_the_underlying_values() {
        let mut extensions = Extensions::new();
        extensions.insert(Widget(9));
        let cloned = extensions.clone();
        assert_eq!(*cloned.get::<Widget>().unwrap(), Widget(9));
    }
}
