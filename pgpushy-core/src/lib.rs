//! Order-free desired-state synthesis for [pgpushy].
//!
//! This crate is the pure half of pgpushy: it takes the *contents* of a source
//! tree — path and text, already read from disk by the caller — and produces
//! the single desired-state document that pgschema consumes, plus the order in
//! which schemas must be reconciled. It performs no IO, opens no connections,
//! and is deterministic: the same input always yields byte-identical output.
//!
//! The normative description of what this crate must do lives in
//! `docs/spec.md`; section references throughout this crate point at it.
//!
//! [pgpushy]: https://github.com/arcanyx-pub/pgpushy

#![forbid(unsafe_code)]
