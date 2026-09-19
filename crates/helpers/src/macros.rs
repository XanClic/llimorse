//! Macros for this workspace.

/// Allow merging two objects of the same type into one.
pub trait Mergeable {
    /// Merge `other` into `self`, with `other` taking precedence.
    fn merge(&mut self, other: Self);

    /// Merge `other` into `self`, with `self` taking precedence.
    fn merge_weak(&mut self, other: Self);
}

impl<T> Mergeable for Option<T> {
    fn merge(&mut self, other: Self) {
        if other.is_some() {
            *self = other;
        }
    }

    fn merge_weak(&mut self, other: Self) {
        if self.is_none() {
            *self = other;
        }
    }
}

impl Mergeable for bool {
    fn merge(&mut self, other: Self) {
        *self |= other;
    }

    fn merge_weak(&mut self, other: Self) {
        *self |= other;
    }
}

/// Derive the `Mergeable` trait for structs composed entirely of `Mergeable` fields.
#[macro_export]
macro_rules! derive_merge {
    (
        $(#[$attr:meta])*
        $vis:vis struct $struct_name:ident {
            $(
                $(#[$field_attr:meta])*
                $field_id:ident: $field_type:ty,
            )+
        }
    ) => {
        $(#[$attr])*
        $vis struct $struct_name {
            $(
                $(#[$field_attr])*
                $field_id: $field_type,
            )+
        }

        impl $crate::macros::Mergeable for $struct_name {
            fn merge(&mut self, other: Self) {
                $(self.$field_id.merge(other.$field_id);)+
            }

            fn merge_weak(&mut self, other: Self) {
                $(self.$field_id.merge_weak(other.$field_id);)+
            }
        }
    };
}

pub use derive_merge;
