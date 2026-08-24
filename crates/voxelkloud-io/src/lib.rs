//! `voxelkloud-io` — reading and writing point clouds, in Rust.
//!
//! The native twin of `@voxelkloud/core` plus its drivers: the same neutral
//! vocabulary (bounds, attributes, CRS declarations, tolerated warnings), the
//! same four formats, and the writers the TypeScript side has no reason to
//! have. What the browser streams, this produces.
//!
//! Three rules carried over from the TypeScript, because they are what made it
//! work and not because of symmetry:
//!
//! 1. **Nothing here opens a socket.** Bytes enter through [`ByteSource`], and
//!    a reader says which ranges it wants. That is what lets one code path
//!    serve a local file, an HTTP deployment and, later, a `File` in a browser
//!    tab — and it is why `voxelkloud-cli` owns every transport decision.
//! 2. **Anomalies are tolerated, not thrown.** Real files break their own
//!    specs. A reader that refuses them reads nothing; a reader that ignores
//!    them lies. So they land in [`Warning`]s on the value, in discovery order.
//! 3. **The vocabulary names no format.** A [`CloudInfo`] from a Potree
//!    directory and one from a COPC file are the same type, and code that wants
//!    the difference has to ask for it.
//!
//! [`ByteSource`]: source::ByteSource
//! [`Warning`]: warning::Warning
//! [`CloudInfo`]: cloud::CloudInfo

pub mod attribute;
pub mod bounds;
pub mod cloud;
pub mod crs;
pub mod error;
pub mod las;
pub mod octree;
pub mod source;
pub mod warning;

#[cfg(feature = "formats")]
pub mod format;

// E57 is neither a manifest format nor a LAS one: it has its own container and
// its own decoder, and a build that wants only LAS framing should not pay for
// an XML parser.
#[cfg(feature = "e57")]
pub mod e57;

// The write side. Reading a cloud needs none of this, and the wasm codec
// package builds without it.
#[cfg(feature = "laz")]
pub mod build;
#[cfg(feature = "laz")]
pub mod read;
#[cfg(feature = "laz")]
pub mod record;
#[cfg(all(feature = "laz", feature = "formats"))]
pub mod convert;
#[cfg(all(feature = "laz", feature = "formats"))]
pub mod optimize;
// The writers. COPC needs only the codec, which is what lets a browser build
// produce one; Potree and EPT describe themselves in JSON and are gated with
// the rest of the manifest formats, inside the module.
#[cfg(feature = "laz")]
pub mod write;

pub use attribute::{Attribute, AttributeRole, AttributeType};
pub use bounds::Bounds;
pub use cloud::{CloudInfo, FormatId};
pub use crs::{Crs, CrsFormat};
pub use error::{Error, Result};
pub use source::ByteSource;
pub use warning::Warning;

/// The version this library reports, which is the version of the CLI built on
/// it. `inspect --json` prints it, so a bug report carries it for free.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
