//! End-to-end tests for Bento.
//!
//! These run the shipped `bentod` binary against a whole deployment built
//! in a temporary directory. See `TESTING.md` for what is real, what is
//! substituted, and why.

// The harness is a toolkit. Not every helper is used by every test, and a
// helper that only one future test needs is still worth keeping next to
// the ones beside it.
#![allow(dead_code)]

mod cases;
mod harness;
mod imageserver;
mod libvirtd;
