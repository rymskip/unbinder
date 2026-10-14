#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod bundle;
mod dereference;
mod documents;
mod error;
mod hooks;
mod pointer;
mod references;
mod shared;
mod stats;
mod walk;
#[cfg(feature = "wasm")]
pub mod wasm;

pub use crate::bundle::{bundle_schema, bundle_schema_with_hooks};
pub use crate::dereference::{
    CircularPolicy, Dereferenced, ExternalPolicy, InternalPolicy, LocalPolicy, Options,
    SharedSchema, SiblingMerge, TrueTarget, dereference_schema, dereference_schema_shared,
    dereference_schema_with, dereference_schema_with_hooks,
};
pub use crate::documents::Documents;
pub use crate::error::{Error, PointerError};
pub use crate::hooks::{Hooks, Place, Source, Step};
pub use crate::stats::Stats;
