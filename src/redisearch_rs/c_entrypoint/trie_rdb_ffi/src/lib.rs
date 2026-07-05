/*
 * Copyright (c) 2006-Present, Redis Ltd.
 * All rights reserved.
 *
 * Licensed under your choice of the Redis Source Available License 2.0
 * (RSALv2); or (b) the Server Side Public License v1 (SSPLv1); or (c) the
 * GNU Affero General Public License v3 (AGPLv3).
*/

//! FFI transport for the lex-mode trie RDB serialization.
//!
//! Bridges the pure-Rust framing in [`trie_rdb`] to Redis' RDB IO primitives
//! (`RedisModule_SaveUnsigned`, `RedisModule_LoadStringBuffer`, and friends).
//! [`trie_rdb`]'s serializers are generic over the shared `rdb_io::RdbIO`
//! trait, and the `rdb_io` crate implements that trait directly for
//! [`redis_module::RedisModuleIO`]. So each `extern "C"` entry point below just
//! wraps the caller's `*mut RedisModuleIO` in a [`redis_module::RedisModuleIO`]
//! and hands it to the generic serializer — no per-crate bridge type is needed.

#![allow(non_camel_case_types, non_snake_case)]

use ffi::RedisModuleIO;
use redis_module::{RedisModuleIO as RmIo, raw};
use trie_rdb::RdbOpts;
use trie_rdb::TrieEntry;
use trie_rdb::str as str_rdb;
use trie_rs::str_trie_map::StrTrieMap;

/// Opaque FFI handle for a [`StrTrieMap<TrieEntry>`].
///
/// Distinct from the void-payload `TrieMap` exposed by the `triemap_ffi`
/// crate, so the two C symbol sets do not collide. Keys flow through
/// the UTF-8-validating [`str_rdb`] loader, so non-UTF-8 RDB payloads
/// surface as `NULL` from [`LexTrieRs_RdbLoad`]. Construct via
/// [`LexTrieRs_New`] or [`LexTrieRs_RdbLoad`]; free via [`LexTrieRs_Free`].
pub struct LexTrieRs(pub StrTrieMap<TrieEntry>);

/// [`RdbWrite`] backed by a [`RmIo`] wrapping the caller's `RedisModuleIO*`.
///
/// The wrapper holds the raw pointer captured at construction and never
/// stores it anywhere else. Validity is promised at construction (see
/// [`RmIoWriter::new`]). NUL framing and the scratch buffer that amortizes
/// its allocation are owned by [`trie_rdb::byte::save`]; this impl just
/// forwards each slice straight to `RedisModule_SaveStringBuffer` via
/// [`RmIo::write_slice`].
///
/// Public so other `*_ffi` crates whose types persist through [`trie_rdb`]
/// (e.g. the spell-check dictionary) can drive their save entry points
/// through the same transport.
pub struct RmIoWriter {
    io: RmIo,
}

impl RmIoWriter {
    /// Wrap the caller's `RedisModuleIO*` as an [`RdbWrite`] sink.
    ///
    /// # Safety
    ///
    /// - `io` must be a valid, non-null `*mut RedisModuleIO` supplied by a
    ///   Redis module save callback, and must remain valid for the entire
    ///   lifetime of the returned writer — every [`RdbWrite`] method
    ///   dereferences it.
    pub unsafe fn new(io: *mut RedisModuleIO) -> Self {
        Self {
            io: RmIo::new(io.cast::<raw::RedisModuleIO>()),
        }
    }
}

impl RdbWrite for RmIoWriter {
    fn save_u64(&mut self, v: u64) {
        self.io.write_unsigned(v);
    }

    fn save_f64(&mut self, v: f64) {
        self.io.write_double(v);
    }

    fn save_bytes(&mut self, b: &[u8]) {
        self.io.write_slice(b);
    }
}

/// [`RdbRead`] backed by a [`RmIo`] wrapping the caller's `RedisModuleIO*`.
///
/// The wrapped `read_*` methods already poll `RedisModule_IsIOError` after
/// each underlying `RedisModule_Load*` and surface failures as `Err`, so we
/// do not need a separate `IsIOError` check.
///
/// Public for the same cross-crate reuse as [`RmIoWriter`].
pub struct RmIoReader {
    io: RmIo,
}

impl RmIoReader {
    /// Wrap the caller's `RedisModuleIO*` as an [`RdbRead`] source.
    ///
    /// # Safety
    ///
    /// - `io` must be a valid, non-null `*mut RedisModuleIO` supplied by a
    ///   Redis module load callback, and must remain valid for the entire
    ///   lifetime of the returned reader — every [`RdbRead`] method
    ///   dereferences it.
    pub unsafe fn new(io: *mut RedisModuleIO) -> Self {
        Self {
            io: RmIo::new(io.cast::<raw::RedisModuleIO>()),
        }
    }
}

impl RdbRead for RmIoReader {
    fn load_u64(&mut self) -> io::Result<u64> {
        self.io.read_unsigned().map_err(io::Error::other)
    }

    fn load_f64(&mut self) -> io::Result<f64> {
        self.io.read_double().map_err(io::Error::other)
    }

    fn load_bytes(&mut self) -> io::Result<Vec<u8>> {
        self.io
            .read_string_buffer()
            .map(|buf| buf.as_ref().to_vec())
            .map_err(io::Error::other)
    }
}

/// Allocate an empty [`LexTrieRs`] on the Rust heap.
///
/// The returned pointer owns its allocation and must be released through
/// [`LexTrieRs_Free`].
#[unsafe(no_mangle)]
pub extern "C" fn LexTrieRs_New() -> *mut LexTrieRs {
    Box::into_raw(Box::new(LexTrieRs(StrTrieMap::new())))
}

/// Free a [`LexTrieRs`] previously produced by [`LexTrieRs_New`] or
/// [`LexTrieRs_RdbLoad`]. A NULL pointer is a no-op.
///
/// # Safety
///
/// - `t` must be either NULL or a pointer previously returned by
///   [`LexTrieRs_New`] / [`LexTrieRs_RdbLoad`] and not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn LexTrieRs_Free(t: *mut LexTrieRs) {
    if t.is_null() {
        return;
    }
    // SAFETY: caller guarantees `t` came from `Box::into_raw` in this
    // module and has not been freed. Reconstructing the `Box` here drops
    // it and releases the allocation.
    drop(unsafe { Box::from_raw(t) });
}

/// Serialize a [`LexTrieRs`] to `io` in the lex-mode RDB wire format.
///
/// Mirrors the C function `TrieType_GenericSave` for a Rust-side trie.
/// Save is infallible at this layer; any underlying RDB IO error surfaces
/// later via `RedisModule_IsIOError` on the load side.
///
/// # Safety
///
/// - `io` must be a valid `*mut RedisModuleIO` supplied by the calling
///   Redis module command and remain valid for the duration of the call.
/// - `map` must be a valid pointer to a [`LexTrieRs`] (typically obtained
///   from [`LexTrieRs_New`] / [`LexTrieRs_RdbLoad`]). It is borrowed
///   immutably for the duration of the call; no aliasing mutable
///   references must exist.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn LexTrieRs_RdbSave(
    io: *mut RedisModuleIO,
    map: *const LexTrieRs,
    save_payloads: bool,
    save_num_docs: bool,
) {
    debug_assert!(!io.is_null(), "io cannot be NULL");
    debug_assert!(!map.is_null(), "map cannot be NULL");

    // SAFETY: caller guarantees `map` is a valid `*const LexTrieRs`
    // and that no aliasing mutable references exist for the call.
    let map = unsafe { &*map };
    // SAFETY: caller guarantees `io` is valid for the duration of the call,
    // which bounds the writer's lifetime.
    let mut w = unsafe { RmIoWriter::new(io) };
    let opts = RdbOpts {
        payloads: save_payloads,
        num_docs: save_num_docs,
    };
    str_rdb::save(&map.0, &mut rm_io, opts);
}

/// Deserialize a [`LexTrieRs`] from `io` in the lex-mode RDB wire format.
///
/// Mirrors the C function `TrieType_GenericLoad` for a Rust-side trie.
/// Returns NULL on any RDB IO or framing error, matching the C contract
/// for `TrieType_GenericLoad` at `src/trie/trie.c:431-438`.
///
/// On success, the caller owns the returned pointer and must release it
/// via [`LexTrieRs_Free`].
///
/// # Safety
///
/// - `io` must be a valid `*mut RedisModuleIO` supplied by the calling
///   Redis module type loader and remain valid for the duration of the
///   call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn LexTrieRs_RdbLoad(
    io: *mut RedisModuleIO,
    load_payloads: bool,
    load_num_docs: bool,
) -> *mut LexTrieRs {
    debug_assert!(!io.is_null(), "io cannot be NULL");

    // SAFETY: caller guarantees `io` is valid for the duration of the call,
    // which bounds the reader's lifetime.
    let mut r = unsafe { RmIoReader::new(io) };
    let opts = RdbOpts {
        payloads: load_payloads,
        num_docs: load_num_docs,
    };
    match str_rdb::load(&mut rm_io, opts) {
        Ok(map) => Box::into_raw(Box::new(LexTrieRs(map))),
        Err(_) => std::ptr::null_mut(),
    }
}
