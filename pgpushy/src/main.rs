//! The `pgpushy` binary: the IO shell around [`pgpushy_core`].
//!
//! Everything that touches the filesystem, the network, a subprocess, or a
//! database lives here; everything deterministic lives in the core crate.

#![forbid(unsafe_code)]

fn main() {
    println!("pgpushy {}", env!("CARGO_PKG_VERSION"));
}
