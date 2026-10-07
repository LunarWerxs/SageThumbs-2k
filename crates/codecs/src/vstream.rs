//! A block-caching read-only `IStream` for video thumbnails of containers we don't have a
//! bespoke index parser for (AVI, WMV/ASF, …). Media Foundation's own demuxer drives the
//! seeking (it reads the file's real index — AVI `idx1`, the ASF Index Object — and jumps to
//! the keyframe near our target time); we just make the underlying reads cheap.
//!
//! Why this is needed: the original "video never thumbnails / 30 s hang" bug was MF doing
//! *thousands of tiny reads* through the shell's marshaled COM thumbnail stream — each a slow
//! cross-apartment RPC. This wrapper coalesces those into a handful of **1 MiB block** reads
//! cached in RAM, so MF can seek freely (to the true ~30 % representative frame) at a few big
//! reads total instead of thousands of tiny ones. A **byte budget** caps the distinct bytes we
//! ever pull from the source, so even if MF decides to scan a multi-GB file it stays bounded
//! (past the budget, reads short → MF fails → the caller falls back to a head prefix / default
//! icon). It runs on the same timeout-guarded worker as the other video tiers. A reader that
//! visits many small, far-apart spots instead (Windows' PDF engine, see `pdf::STREAM_BLOCK`)
//! takes smaller blocks under the same byte budget ([`BlockCacheStream::with_block`]).
//!
//! Read-only: every mutating `IStream`/`ISequentialStream` method is a no-op/`E_NOTIMPL`. All
//! state is behind a `Mutex` so a panic can never unwind across the COM ABI (panic = abort).

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows::core::{Error, Result, HRESULT};
use windows::Win32::Foundation::{
    E_FAIL, E_INVALIDARG, E_NOTIMPL, E_POINTER, STG_E_ACCESSDENIED, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{
    ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG, STATSTG, STGC, STGTY_STREAM,
    STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
};
use windows_implement::implement;

/// Default read granularity: one cross-apartment RPC fetches this much from the source at a time.
const BLOCK: u64 = 1024 * 1024;
/// The smallest block [`BlockCacheStream::with_block`] accepts.
const MIN_BLOCK: u64 = 4 * 1024;
/// Hard cap on the distinct bytes ever pulled from the source (192 MiB), whatever the block
/// size. A well-indexed file touches a tiny fraction of this (header + index + one GOP);
/// hitting it means the reader is scanning a huge file, so we stop feeding it and let the
/// caller fall back.
const BUDGET_BYTES: u64 = 192 * 1024 * 1024;

struct State {
    pos: u64,
    cache: HashMap<u64, Box<[u8]>>,
    blocks_read: usize,
}

/// What a [`BlockCacheStream`] pulled and why it refused a read, if it did. Shared through an
/// `Arc` because the stream itself goes to the decoder as an `IStream`, and a decoder that
/// fails says only that it failed: these say whether the cache starved it (issue #59's big
/// PDFs failed on an exhausted budget and the log never said so).
#[derive(Default)]
pub struct CacheStats {
    block: u64,
    budget_blocks: usize,
    blocks: AtomicUsize,
    over_budget: AtomicBool,
    past_deadline: AtomicBool,
    read_failed: AtomicBool,
}

impl CacheStats {
    /// One debug-log clause: what was pulled, and which refusals (if any) the reader met.
    pub fn describe(&self) -> String {
        let refusals = [
            (&self.over_budget, "the read budget ran out"),
            (&self.past_deadline, "the read deadline passed"),
            (&self.read_failed, "a read of the source failed"),
        ];
        let why: Vec<&str> = refusals
            .iter()
            .filter(|(hit, _)| hit.load(Ordering::Relaxed))
            .map(|(_, why)| *why)
            .collect();
        let pulled = format!(
            "the block cache pulled {} of at most {} blocks of {} KiB",
            self.blocks.load(Ordering::Relaxed),
            self.budget_blocks,
            self.block >> 10
        );
        if why.is_empty() {
            pulled
        } else {
            format!("{pulled}, then refused reads: {}", why.join(", "))
        }
    }
}

/// A read-only `IStream` over `inner`, caching blocks (1 MiB unless [`Self::with_block`] says
/// otherwise). Construct, then `.into()` an [`IStream`] to hand to `MFCreateMFByteStreamOnStream`.
#[implement(IStream)]
pub struct BlockCacheStream {
    inner: IStream,
    size: u64,
    /// Wall-clock cutoff: past it, further source reads are refused (short read → MF gives up).
    /// Bounds I/O even when this runs inline on the shell's thumbnail thread (no worker timeout).
    deadline: Instant,
    /// Bytes per source read, and how many distinct blocks [`BUDGET_BYTES`] allows at that size.
    block: u64,
    budget_blocks: usize,
    stats: Arc<CacheStats>,
    state: Mutex<State>,
}

impl BlockCacheStream {
    pub fn new(inner: IStream, size: u64, deadline: Instant) -> Self {
        Self::with_block(inner, size, deadline, BLOCK)
    }

    /// [`Self::new`] reading `block` bytes per source read (clamped to 4 KiB..=1 MiB) under the
    /// same `BUDGET_BYTES`. Big blocks suit a reader that seeks a few times and then reads on;
    /// a reader that visits many small, far-apart spots wants small ones, or every visit costs a
    /// whole block: Windows' PDF engine reads ~8 KiB at each page object of a scanned book, a
    /// scan's length apart, and at 1 MiB a 200-page book spent the whole budget (issue #59).
    pub fn with_block(inner: IStream, size: u64, deadline: Instant, block: u64) -> Self {
        let block = block.clamp(MIN_BLOCK, BLOCK);
        let budget_blocks = (BUDGET_BYTES / block) as usize;
        Self {
            inner,
            size,
            deadline,
            block,
            budget_blocks,
            stats: Arc::new(CacheStats {
                block,
                budget_blocks,
                ..CacheStats::default()
            }),
            state: Mutex::new(State {
                pos: 0,
                cache: HashMap::new(),
                blocks_read: 0,
            }),
        }
    }

    /// What this stream has pulled and refused so far; still readable once the stream has gone
    /// to a decoder as an `IStream`.
    pub fn stats(&self) -> Arc<CacheStats> {
        Arc::clone(&self.stats)
    }

    /// Ensure block `blk` is cached; returns false if unavailable (budget hit / deadline passed /
    /// past EOF / read error), which surfaces to MF as a short read. Each refusal but EOF is
    /// recorded in [`Self::stats`].
    fn ensure_block(&self, st: &mut State, blk: u64) -> bool {
        if st.cache.contains_key(&blk) {
            return true;
        }
        if st.blocks_read >= self.budget_blocks {
            self.stats.over_budget.store(true, Ordering::Relaxed);
            return false;
        }
        if Instant::now() >= self.deadline {
            self.stats.past_deadline.store(true, Ordering::Relaxed);
            return false;
        }
        let start = blk * self.block;
        if start >= self.size {
            return false;
        }
        let len = self.block.min(self.size - start) as usize;
        let mut buf = vec![0u8; len];
        if unsafe { self.read_inner_at(start, &mut buf) }.is_none() {
            self.stats.read_failed.store(true, Ordering::Relaxed);
            return false;
        }
        st.cache.insert(blk, buf.into_boxed_slice());
        st.blocks_read += 1;
        self.stats.blocks.store(st.blocks_read, Ordering::Relaxed);
        true
    }

    /// One big sequential read from the source at absolute `off`, looping over short reads.
    unsafe fn read_inner_at(&self, off: u64, buf: &mut [u8]) -> Option<()> {
        self.inner.Seek(off as i64, STREAM_SEEK_SET, None).ok()?;
        crate::streamsrc::read_full(&self.inner, buf)
    }
}

impl ISequentialStream_Impl for BlockCacheStream_Impl {
    fn Read(&self, pv: *mut c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        if pv.is_null() {
            return E_POINTER;
        }
        let Ok(mut st) = self.state.lock() else {
            return E_FAIL;
        };
        let pos = st.pos;
        let want = (cb as u64).min(self.size.saturating_sub(pos)) as usize;
        let out = unsafe { std::slice::from_raw_parts_mut(pv as *mut u8, want) };
        let mut done = 0usize;
        while done < want {
            let abs = pos + done as u64;
            let blk = abs / self.block;
            if !self.ensure_block(&mut st, blk) {
                break; // budget / EOF / read error → short read
            }
            let block = &st.cache[&blk];
            let off = (abs % self.block) as usize;
            let n = (want - done).min(block.len() - off);
            out[done..done + n].copy_from_slice(&block[off..off + n]);
            done += n;
        }
        st.pos = pos + done as u64;
        if !pcbread.is_null() {
            unsafe { *pcbread = done as u32 };
        }
        // S_FALSE signals a short read (genuine EOF or budget cut), like a real stream.
        if done == cb as usize {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Write(&self, _pv: *const c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for BlockCacheStream_Impl {
    fn Seek(&self, dlibmove: i64, dworigin: STREAM_SEEK, plibnewposition: *mut u64) -> Result<()> {
        let mut st = self.state.lock().map_err(|_| Error::from(E_FAIL))?;
        let base: i128 = match dworigin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => st.pos as i128,
            STREAM_SEEK_END => self.size as i128,
            _ => return Err(Error::from(E_INVALIDARG)),
        };
        let np = base + dlibmove as i128;
        if np < 0 {
            return Err(Error::from(E_INVALIDARG));
        }
        // Seeking past EOF is legal for a stream; subsequent reads just return 0 bytes.
        st.pos = np as u64;
        if !plibnewposition.is_null() {
            unsafe { *plibnewposition = st.pos };
        }
        Ok(())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: &STATFLAG) -> Result<()> {
        if pstatstg.is_null() {
            return Err(Error::from(E_POINTER));
        }
        let s = STATSTG {
            r#type: STGTY_STREAM.0 as u32,
            cbSize: self.size,
            ..Default::default()
        };
        unsafe { *pstatstg = s };
        Ok(())
    }

    // Read-only stream: nothing else is supported.
    fn SetSize(&self, _libnewsize: u64) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn CopyTo(
        &self,
        _pstm: windows::core::Ref<'_, IStream>,
        _cb: u64,
        _pcbread: *mut u64,
        _pcbwritten: *mut u64,
    ) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn Commit(&self, _grfcommitflags: &STGC) -> Result<()> {
        Ok(())
    }
    fn Revert(&self) -> Result<()> {
        Ok(())
    }
    fn LockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: &LOCKTYPE) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn UnlockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: u32) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn Clone(&self) -> Result<IStream> {
        Err(Error::from(E_NOTIMPL))
    }
}

#[cfg(test)]
mod tests;
