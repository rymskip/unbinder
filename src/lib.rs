#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod dereference;
mod documents;
mod error;
mod pointer;
mod references;
mod shared;
mod stats;
mod walk;
#[cfg(feature = "wasm")]
pub mod wasm;

pub use crate::dereference::{
    CircularPolicy, Dereferenced, Options, SharedSchema, SiblingMerge, TrueTarget,
    dereference_schema, dereference_schema_shared, dereference_schema_with,
};
pub use crate::documents::Documents;
pub use crate::error::{Error, PointerError};
pub use crate::stats::Stats;
