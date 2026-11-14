// to allow `mod foo {}` in foo.rs; clippy thinks this is a mistake/misunderstanding of
// how `mod` works, but we want this sometimes for pymodule declarations
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
#![allow(clippy::module_inception)]

#[macro_use]
extern crate rustpython_derive;

extern crate alloc;

mod _asyncio;
mod _remote_debugging;
pub mod array;
mod binascii;
mod bisect;
mod bz2;
mod cjkcodecs;
mod cmath;
mod compression; // internal module
mod contextvars;
mod csv;
mod elementtree;

mod lzma;

mod zlib;

mod blake2;
mod hashlib;
mod md5;
mod sha1;
mod sha256;
mod sha3;
mod sha512;

mod json;

mod _opcode;

#[path = "_tokenize.rs"]
mod _tokenize;

mod math;

mod _datetime;
mod _heapq;
mod _queue;
mod _zoneinfo;
mod pickle;
mod pyexpat;
mod pystruct;
mod mersenne_twister;
mod random;
mod statistics;
mod suggestions;

// TODO: maybe make this an extension module, if we ever get those
// mod re;

#[cfg(any(all(target_arch = "wasm32", target_os = "unknown"), target_os = "wasi"))]
#[path = "socket_wasm.rs"]
pub mod socket;

mod unicodedata;

use rustpython_common as common;
use rustpython_vm as vm;

use crate::vm::{Context, builtins};

/// Returns module definitions for multi-phase init modules.
/// These modules are added to sys.modules BEFORE their exec function runs,
/// allowing safe circular imports.
pub fn stdlib_module_defs(ctx: &Context) -> Vec<&'static builtins::PyModuleDef> {
    vec![
        _asyncio::module_def(ctx),
        cjkcodecs::_codecs_cn::module_def(ctx),
        cjkcodecs::_codecs_hk::module_def(ctx),
        cjkcodecs::_codecs_iso2022::module_def(ctx),
        cjkcodecs::_codecs_jp::module_def(ctx),
        cjkcodecs::_codecs_kr::module_def(ctx),
        cjkcodecs::_codecs_tw::module_def(ctx),
        cjkcodecs::multibytecodec::module_def(ctx),
        _opcode::module_def(ctx),
        _remote_debugging::module_def(ctx),
        array::module_def(ctx),
        binascii::module_def(ctx),
        bisect::module_def(ctx),
        blake2::module_def(ctx),
        bz2::module_def(ctx),
        cmath::module_def(ctx),
        contextvars::module_def(ctx),
        csv::module_def(ctx),
        elementtree::module_def(ctx),
        hashlib::module_def(ctx),
        json::module_def(ctx),
        lzma::module_def(ctx),
        math::module_def(ctx),
        md5::module_def(ctx),
        pickle::module_def(ctx),
        pyexpat::module_def(ctx),
        pystruct::module_def(ctx),
        _datetime::module_def(ctx),
        _zoneinfo::module_def(ctx),
        _heapq::module_def(ctx),
        _queue::module_def(ctx),
        random::module_def(ctx),
        sha1::module_def(ctx),
        sha256::module_def(ctx),
        sha3::module_def(ctx),
        sha512::module_def(ctx),
        #[cfg(any(all(target_arch = "wasm32", target_os = "unknown"), target_os = "wasi"))]
        socket::module_def(ctx),
        statistics::module_def(ctx),
        suggestions::module_def(ctx),
        _tokenize::module_def(ctx),
        unicodedata::module_def(ctx),
        zlib::module_def(ctx),
    ]
}
