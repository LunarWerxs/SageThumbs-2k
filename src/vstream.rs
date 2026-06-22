//! A block-caching read-only `IStream` for video thumbnails of containers we don't have a
//! bespoke index parser for (AVI, WMV/ASF, …). Media Foundation's own demuxer drives the
//! seeking (it reads the file's real index — AVI `idx1`, the ASF Index Object — and jumps to
//! the keyframe near our target time); we just make the underlying reads cheap.
//!
//! Why this is needed: the original "video never thumbnails / 30 s hang" bug was MF doing
//! *thousands of tiny reads* through the shell's marshaled COM thumbnail stream — each a slow
//! cross-apartment RPC. This wrapper coalesces those into a handful of **1 MiB block** reads
//! cached in RAM, so MF can seek freely (to the true ~30 % representative frame) at a few big
//! reads total instead of thousands of tiny ones. A **block budget** caps the distinct bytes we
//! ever pull from the source, so even if MF decides to scan a multi-GB file it stays bounded
//! (past the budget, reads short → MF fails → the caller falls back to a head prefix / default
//! icon). It runs on the same timeout-guarded worker as the other video tiers.
//!
//! Read-only: every mutating `IStream`/`ISequentialStream` method is a no-op/`E_NOTIMPL`. All
//! state is behind a `Mutex` so a panic can never unwind across the COM ABI (panic = abort).

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use windows::core::{Error, Result, HRESULT};
use windows::Win32::Foundation::{
    E_FAIL, E_INVALIDARG, E_NOTIMPL, E_POINTER, S_FALSE, S_OK, STG_E_ACCESSDENIED,
};
use windows::Win32::System::Com::{
    ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG, STATSTG, STGC, STGTY_STREAM,
    STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
};
use windows_implement::implement;

/// Read granularity: one cross-apartment RPC fetches this much from the source at a time.
const BLOCK: u64 = 1024 * 1024;
/// Hard cap on distinct blocks ever pulled from the source (192 MiB). A well-indexed file
/// touches a tiny fraction of this (header + index + one GOP); hitting it means MF is scanning
/// a huge unindexed file, so we stop feeding it and let the caller fall back.
const BUDGET_BLOCKS: usize = 192;

struct State {
    pos: u64,
    cache: HashMap<u64, Box<[u8]>>,
    blocks_read: usize,
}

/// A read-only `IStream` over `inner`, caching 1 MiB blocks. Construct, then `.into()` an
/// [`IStream`] to hand to `MFCreateMFByteStreamOnStream`.
#[implement(IStream)]
pub struct BlockCacheStream {
    inner: IStream,
    size: u64,
    /// Wall-clock cutoff: past it, further source reads are refused (short read → MF gives up).
    /// Bounds I/O even when this runs inline on the shell's thumbnail thread (no worker timeout).
    deadline: Instant,
    state: Mutex<State>,
}

impl BlockCacheStream {
    pub fn new(inner: IStream, size: u64, deadline: Instant) -> Self {
        Self {
            inner,
            size,
            deadline,
            state: Mutex::new(State {
                pos: 0,
                cache: HashMap::new(),
                blocks_read: 0,
            }),
        }
    }

    /// Ensure block `blk` is cached; returns false if unavailable (budget hit / deadline passed /
    /// past EOF / read error), which surfaces to MF as a short read.
    fn ensure_block(&self, st: &mut State, blk: u64) -> bool {
        if st.cache.contains_key(&blk) {
            return true;
        }
        if st.blocks_read >= BUDGET_BLOCKS || Instant::now() >= self.deadline {
            return false;
        }
        let start = blk * BLOCK;
        if start >= self.size {
            return false;
        }
        let len = BLOCK.min(self.size - start) as usize;
        let mut buf = vec![0u8; len];
        if unsafe { self.read_inner_at(start, &mut buf) }.is_none() {
            return false;
        }
        st.cache.insert(blk, buf.into_boxed_slice());
        st.blocks_read += 1;
        true
    }

    /// One big sequential read from the source at absolute `off`, looping over short reads.
    unsafe fn read_inner_at(&self, off: u64, buf: &mut [u8]) -> Option<()> {
        self.inner.Seek(off as i64, STREAM_SEEK_SET, None).ok()?;
        let mut filled = 0;
        while filled < buf.len() {
            let mut got: u32 = 0;
            let want = (buf.len() - filled).min(u32::MAX as usize) as u32;
            if self
                .inner
                .Read(buf[filled..].as_mut_ptr() as *mut c_void, want, Some(&mut got))
                .is_err()
            {
                return None;
            }
            if got == 0 {
                break;
            }
            filled += (got as usize).min(buf.len() - filled);
        }
        (filled == buf.len()).then_some(())
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
            let blk = abs / BLOCK;
            if !self.ensure_block(&mut st, blk) {
                break; // budget / EOF / read error → short read
            }
            let block = &st.cache[&blk];
            let off = (abs % BLOCK) as usize;
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
