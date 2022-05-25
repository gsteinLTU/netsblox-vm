#![forbid(unsafe_code)]
#![no_std]

//! [NetsBlox](https://netsblox.org/) is a block-based programming environment developed at Vanderbilt
//! which is based on [Snap!](https://snap.berkeley.edu/) from Berkeley.
//! NetsBlox adds several networking-related features, in particular the use of Remote Procedure Calls (RPCs)
//! that can access web-based resources and cloud utilities, as well as message passing between
//! NetsBlox projects running anywhere in the world.
//!
//! `netsblox_vm` is a pure Rust implementation of the NetsBlox block-based code execution engine
//! which is written in safe, no_std Rust for use on arbitrary devices, including embedded applications.

extern crate no_std_compat as std;

macro_rules! trivial_from_impl {
    ($t:ident : $($f:ident),*$(,)?) => {$(
        impl From<$f> for $t { fn from(e: $f) -> $t { $t::$f(e) } }
    )*}
}

pub mod bytecode;
pub mod runtime;
pub mod process;

#[cfg(test)] mod test;
