//! Download, cache, and run grammars from the Tree-sitter WASM registry.
//!
//! Registry operations block. Run them on a background thread, not on the
//! parser workers. Use [`Workers::run`] or [`Workers::run_blocking`] for parser
//! creation, parsing, and queries, which need larger stacks on some platforms.

mod registry;
mod runtime;
mod workers;

pub use registry::{Network, Registry, RegistryBuilder, Resolution, default_cache_dir};
pub use runtime::{Grammar, Runtime};
pub use tree_sitter;
pub use workers::Workers;
