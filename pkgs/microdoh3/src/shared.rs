//! Shared-memory IPC: upstream distribution and worker score feedback.
//!
//! Two memfds, each a single 4 KiB page, are created by the supervisor before
//! forking:
//!
//! * **resolve** (supervisor → children): the candidate upstream address set
//!   plus the relative selection weight of each address. The supervisor is the
//!   sole writer; children map the page `PROT_READ`, so the mapping flags
//!   enforce the direction.
//! * **scores** (children → supervisor): one seqlock-guarded slot per worker
//!   carrying the measured score of the address that worker is currently
//!   using. Each slot has exactly one writer (its own worker), so the seqlock
//!   is enough; the supervisor maps the page `PROT_READ`.
//!
//! Both pages use the same lock-free seqlock discipline: a reader retries on
//! the next pass if it observes an odd (in-progress) sequence or a torn read.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::atomic::{fence, AtomicU32, Ordering};
use std::time::Instant;

use nix::sys::memfd::{memfd_create, MFdFlags};
use nix::sys::mman::{mmap, MapFlags, ProtFlags};
use nix::unistd::ftruncate;

/// Max upstream addresses stored (DNS answers plus a seeded extra list).
pub const MAX_ADDRS: usize = 16;
/// Max worker processes that can own a score slot.
pub const MAX_WORKERS: usize = 32;
const SHM_SIZE: usize = 4096;

/// Worker flags reported in a score slot.
pub const FLAG_CONNECTED: u32 = 1 << 0;
pub const FLAG_HANDSHAKING: u32 = 1 << 1;
/// The connection offered TLS 1.3 early data (a resumption ticket allowed it).
pub const FLAG_ZERO_RTT: u32 = 1 << 2;

/// Convert a measured RTT into the score the supervisor selects on.
///
/// This is the only arithmetic in the mechanism: higher is better, 0 means "no
/// usable measurement". Loss needs no separate term — a lossy path shows up as
/// slow *successful* requests (retransmits), which is exactly what this tracks.
pub fn score_from_rtt(rtt_us: u32) -> u32 {
    if rtt_us == 0 {
        // Nothing measured yet: unknown, not "instant".
        return 0;
    }
    1_000_000 / rtt_us
}

/// One stored socket address (IP only; the port is fixed by configuration).
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct RawAddr {
    bytes: [u8; 16],
    is_v6: u8,
    _pad: [u8; 15],
}

impl RawAddr {
    fn from_ip(ip: IpAddr) -> Self {
        let mut raw = RawAddr::default();
        match ip {
            IpAddr::V4(v4) => {
                raw.bytes[..4].copy_from_slice(&v4.octets());
                raw.is_v6 = 0;
            }
            IpAddr::V6(v6) => {
                raw.bytes = v6.octets();
                raw.is_v6 = 1;
            }
        }
        raw
    }

    fn to_ip(&self) -> IpAddr {
        if self.is_v6 == 1 {
            IpAddr::V6(Ipv6Addr::from(self.bytes))
        } else {
            IpAddr::V4(Ipv4Addr::new(
                self.bytes[0],
                self.bytes[1],
                self.bytes[2],
                self.bytes[3],
            ))
        }
    }
}

/// CLOCK_MONOTONIC now in seconds (system-wide epoch, comparable across processes).
fn mono_now_secs() -> u64 {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)
        .map(|ts| ts.tv_sec() as u64)
        .unwrap_or(0)
}

/// Publishable `mmap` helper: create a memfd of `SHM_SIZE` bytes.
fn create_shm(name: &str) -> io::Result<OwnedFd> {
    let fd = memfd_create(name, MFdFlags::MFD_CLOEXEC)?;
    ftruncate(&fd, SHM_SIZE as _)?;
    Ok(fd)
}

fn map_fd<T>(fd: BorrowedFd<'_>, prot: ProtFlags) -> io::Result<*mut T> {
    let ptr = unsafe {
        mmap(
            None,
            SHM_SIZE.try_into().unwrap(),
            prot,
            MapFlags::MAP_SHARED,
            fd,
            0,
        )?
    };
    Ok(ptr.as_ptr().cast())
}

fn unmap<T>(ptr: *mut T) {
    unsafe {
        let _ = nix::sys::mman::munmap(
            std::ptr::NonNull::new_unchecked(ptr.cast::<std::ffi::c_void>()),
            SHM_SIZE,
        );
    }
}

// ---------------------------------------------------------------------------
// resolve page (supervisor → children)
// ---------------------------------------------------------------------------

/// The shared resolve page layout (repr(C), seqlock-guarded).
#[repr(C)]
struct SharedResolve {
    /// Sequence counter: odd while the writer is publishing, even otherwise.
    seq: AtomicU32,
    /// Number of valid entries in `addrs`/`weights`.
    count: u32,
    /// Revision, bumped on every successful publish.
    revision: u32,
    /// Number of worker processes the supervisor has preforked.
    workers: u32,
    /// CLOCK_MONOTONIC seconds when this revision was published (stagger base).
    published_mono_secs: u64,
    /// CLOCK_MONOTONIC seconds when this resolution expires (informational).
    expires_mono_secs: u64,
    addrs: [RawAddr; MAX_ADDRS],
    /// Relative selection weight per address; higher is better, 0 = unusable.
    weights: [u32; MAX_ADDRS],
}

/// What a worker reads out of the resolve page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpstreamSet {
    pub addrs: Vec<IpAddr>,
    pub weights: Vec<u32>,
    pub revision: u32,
    pub workers: u32,
    pub published_mono_secs: u64,
}

impl UpstreamSet {
    /// Deterministically pick this worker's address slot.
    ///
    /// Weights are relative preferences; the worker `idx` of `workers` takes
    /// the address whose cumulative-weight interval contains
    /// `(idx + 0.5) / workers`. Doing it by interval (instead of independently
    /// sampling) makes the fleet's split across addresses exact rather than a
    /// noisy multinomial — N workers over weights 3:2:1 really do land 3/2/1.
    /// Because candidates are published best-first, the highest worker index
    /// naturally lands on the lowest-weight (exploration) candidate.
    pub fn slot_for(&self, idx: usize) -> Option<usize> {
        let n = self.addrs.len().min(self.weights.len());
        if n == 0 {
            return None;
        }
        let workers = self.workers.max(1) as u64;
        let total: u64 = self.weights[..n].iter().map(|&w| w as u64).sum();
        if total == 0 {
            // No information yet: spread evenly.
            return Some((idx as u64 % n as u64) as usize);
        }
        // Midpoint of this worker's slice, in units of total weight.
        let target = (2 * (idx as u64) + 1) * total / (2 * workers);
        let mut acc: u64 = 0;
        for (i, &w) in self.weights[..n].iter().enumerate() {
            acc += w as u64;
            if target < acc {
                return Some(i);
            }
        }
        Some(n - 1)
    }

    pub fn weight_of(&self, addr: IpAddr) -> u32 {
        self.addrs
            .iter()
            .position(|a| *a == addr)
            .map(|i| self.weights.get(i).copied().unwrap_or(0))
            .unwrap_or(0)
    }
}

/// The supervisor's writable handle to the resolve page.
pub struct ResolveWriter {
    ptr: *mut SharedResolve,
}

// The raw pointer is only dereferenced in the supervisor process.
unsafe impl Send for ResolveWriter {}

impl ResolveWriter {
    /// Create the memfd and map it shared+writable. The returned fd is
    /// inherited by children (who map it read-only).
    pub fn create() -> io::Result<(OwnedFd, Self)> {
        let fd = create_shm("microdoh3-resolve")?;
        let ptr: *mut SharedResolve =
            map_fd(fd.as_fd(), ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)?;
        let writer = Self { ptr };
        // Initialize to a clean even sequence.
        writer.raw().seq.store(0, Ordering::Relaxed);
        Ok((fd, writer))
    }

    fn raw(&self) -> &SharedResolve {
        unsafe { &*self.ptr }
    }

    fn raw_mut(&mut self) -> &mut SharedResolve {
        unsafe { &mut *self.ptr }
    }

    /// Publish a new upstream set with per-address selection weights.
    pub fn publish(
        &mut self,
        addrs: &[IpAddr],
        weights: &[u32],
        workers: usize,
        expires_at: Instant,
    ) {
        let old = self.raw().seq.load(Ordering::Relaxed);
        self.raw().seq.store(old.wrapping_add(1), Ordering::Release); // odd: write in progress
        {
            let p = self.raw_mut();
            let n = addrs.len().min(MAX_ADDRS);
            p.count = n as u32;
            p.revision = p.revision.wrapping_add(1);
            p.workers = workers as u32;
            for (i, addr) in addrs.iter().take(n).enumerate() {
                p.addrs[i] = RawAddr::from_ip(*addr);
                p.weights[i] = weights.get(i).copied().unwrap_or(1);
            }
            for w in p.weights.iter_mut().skip(n) {
                *w = 0;
            }
            p.published_mono_secs = mono_now_secs();
            p.expires_mono_secs = mono_now_secs()
                + expires_at
                    .saturating_duration_since(Instant::now())
                    .as_secs();
        }
        fence(Ordering::SeqCst);
        self.raw().seq.store(old.wrapping_add(2), Ordering::Release); // even: committed
    }
}

impl Drop for ResolveWriter {
    fn drop(&mut self) {
        unmap(self.ptr);
    }
}

/// A child's read-only view of the resolve page.
pub struct ResolveReader {
    ptr: *const SharedResolve,
    last_seq: u32,
}

unsafe impl Send for ResolveReader {}

impl ResolveReader {
    /// Map the inherited fd read-only.
    pub fn map_readonly(fd: RawFd) -> io::Result<Self> {
        let borrowed: BorrowedFd<'_> = unsafe { BorrowedFd::borrow_raw(fd) };
        let ptr: *const SharedResolve = map_fd(borrowed, ProtFlags::PROT_READ)?;
        Ok(Self { ptr, last_seq: 0 })
    }

    fn raw(&self) -> &SharedResolve {
        unsafe { &*self.ptr }
    }

    /// Expiry (CLOCK_MONOTONIC seconds), informational only.
    #[allow(dead_code)]
    pub fn expires_mono_secs(&self) -> u64 {
        self.raw().expires_mono_secs
    }

    /// Whether the published resolution is past its TTL.
    #[allow(dead_code)]
    pub fn is_stale(&self) -> bool {
        mono_now_secs() > self.raw().expires_mono_secs
    }

    /// Read the current upstream set if it changed since the last call.
    /// Returns None when unchanged, when the writer is mid-publish (retry
    /// next time), or when a torn read was detected (retry next time).
    pub fn read_if_changed(&mut self) -> Option<UpstreamSet> {
        let first = self.raw().seq.load(Ordering::Acquire);
        if first == self.last_seq || first % 2 == 1 {
            return None;
        }
        fence(Ordering::SeqCst);
        let p = self.raw();
        let count = p.count.min(MAX_ADDRS as u32);
        let mut addrs = Vec::with_capacity(count as usize);
        let mut weights = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            addrs.push(p.addrs[i].to_ip());
            weights.push(p.weights[i]);
        }
        let out = UpstreamSet {
            addrs,
            weights,
            revision: p.revision,
            workers: p.workers,
            published_mono_secs: p.published_mono_secs,
        };
        fence(Ordering::SeqCst);
        let second = self.raw().seq.load(Ordering::Acquire);
        if first != second {
            return None; // torn read — retry on the next pass
        }
        self.last_seq = first;
        Some(out)
    }

    /// Blocking read of the current set (used once at child startup; the
    /// supervisor always publishes before forking, so this never spins long).
    pub fn read_initial(&mut self) -> UpstreamSet {
        loop {
            if let Some(v) = self.read_if_changed() {
                if !v.addrs.is_empty() {
                    return v;
                }
            }
            std::thread::yield_now();
        }
    }
}

impl Drop for ResolveReader {
    fn drop(&mut self) {
        unmap(self.ptr as *mut SharedResolve);
    }
}

// ---------------------------------------------------------------------------
// scores page (children → supervisor)
// ---------------------------------------------------------------------------

/// One worker's measurement of the address it is currently using.
#[repr(C)]
struct ScoreSlot {
    /// Seqlock: odd while the owning worker is writing.
    seq: AtomicU32,
    /// Address the worker is currently using (all-zero bytes = none).
    addr: RawAddr,
    /// Selection score of `addr`; 0 = no usable measurement.
    score: u32,
    /// Requests completed / failed in the last window (diagnostics only).
    ok: u32,
    fail: u32,
    /// `FLAG_*` bits.
    flags: u32,
    /// CLOCK_MONOTONIC seconds of the last update.
    updated_mono_secs: u64,
    _pad: [u8; 4],
}

/// Shared page of per-worker score slots.
#[repr(C)]
struct SharedScores {
    slots: [ScoreSlot; MAX_WORKERS],
}

/// A decoded score reading.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScoreSample {
    pub addr: Option<IpAddr>,
    /// Selection score (see [`score_from_rtt`]); 0 = unusable/unmeasured.
    pub score: u32,
    /// Requests completed / failed in the last window (diagnostics only).
    pub ok: u32,
    pub fail: u32,
    pub flags: u32,
    pub updated_mono_secs: u64,
}

/// A child's writable handle to its own score slot.
pub struct ScoreWriter {
    ptr: *mut SharedScores,
    idx: usize,
}

unsafe impl Send for ScoreWriter {}

impl ScoreWriter {
    /// Map the inherited scores fd read-write and claim slot `idx`.
    pub fn map(fd: RawFd, idx: usize) -> io::Result<Self> {
        let borrowed: BorrowedFd<'_> = unsafe { BorrowedFd::borrow_raw(fd) };
        let ptr: *mut SharedScores =
            map_fd(borrowed, ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)?;
        let idx = idx.min(MAX_WORKERS - 1);
        Ok(Self { ptr, idx })
    }

    fn slot(&self) -> &ScoreSlot {
        unsafe { &(*self.ptr).slots[self.idx] }
    }

    /// Publish this worker's current measurement (seqlock write).
    pub fn write(&self, sample: &ScoreSample) {
        let s = self.slot();
        let old = s.seq.load(Ordering::Relaxed);
        s.seq.store(old.wrapping_add(1), Ordering::Release);
        fence(Ordering::SeqCst);
        unsafe {
            let m = &mut (*self.ptr).slots[self.idx];
            m.addr = sample.addr.map(RawAddr::from_ip).unwrap_or_default();
            m.score = sample.score;
            m.ok = sample.ok;
            m.fail = sample.fail;
            m.flags = sample.flags;
            m.updated_mono_secs = mono_now_secs();
        }
        fence(Ordering::SeqCst);
        s.seq.store(old.wrapping_add(2), Ordering::Release);
    }
}

impl Drop for ScoreWriter {
    fn drop(&mut self) {
        unmap(self.ptr);
    }
}

/// The supervisor's read-only view of all score slots.
pub struct ScoreReader {
    ptr: *const SharedScores,
}

unsafe impl Send for ScoreReader {}

impl ScoreReader {
    /// Create the scores memfd and map it read-only in the supervisor.
    pub fn create() -> io::Result<(OwnedFd, Self)> {
        let fd = create_shm("microdoh3-scores")?;
        let ptr: *const SharedScores = map_fd(fd.as_fd(), ProtFlags::PROT_READ)?;
        Ok((fd, Self { ptr }))
    }

    /// Read one slot; None on a torn or in-progress write.
    pub fn read(&self, idx: usize) -> Option<ScoreSample> {
        if idx >= MAX_WORKERS {
            return None;
        }
        let s = unsafe { &(*self.ptr).slots[idx] };
        let first = s.seq.load(Ordering::Acquire);
        if first == 0 || first % 2 == 1 {
            return None;
        }
        fence(Ordering::SeqCst);
        let sample = ScoreSample {
            addr: if s.addr.bytes.iter().all(|&b| b == 0) && s.addr.is_v6 == 0 {
                None
            } else {
                Some(s.addr.to_ip())
            },
            score: s.score,
            ok: s.ok,
            fail: s.fail,
            flags: s.flags,
            updated_mono_secs: s.updated_mono_secs,
        };
        fence(Ordering::SeqCst);
        let second = s.seq.load(Ordering::Acquire);
        if first != second {
            return None;
        }
        Some(sample)
    }
}

impl Drop for ScoreReader {
    fn drop(&mut self) {
        unmap(self.ptr as *mut SharedScores);
    }
}

/// CLOCK_MONOTONIC seconds, exposed for callers that need the same clock the
/// shared pages use.
pub fn mono_secs() -> u64 {
    mono_now_secs()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn publish(w: &mut ResolveWriter, ips: &[IpAddr], weights: &[u32]) {
        w.publish(
            ips,
            weights,
            weights.len().max(1),
            Instant::now() + Duration::from_secs(300),
        );
    }

    #[test]
    fn write_then_read_roundtrip() {
        let (fd, mut writer) = ResolveWriter::create().unwrap();
        let ips = vec![
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V6("2001:4860:4860::8888".parse().unwrap()),
        ];
        publish(&mut writer, &ips, &[70, 30]);

        let mut reader = ResolveReader::map_readonly(std::os::fd::AsRawFd::as_raw_fd(&fd)).unwrap();
        let got = reader.read_if_changed().expect("first read must see data");
        assert_eq!(got.addrs, ips);
        assert_eq!(got.weights, vec![70, 30]);
        assert_eq!(got.revision, 1);
        // Unchanged since last read → None.
        assert!(reader.read_if_changed().is_none());
        assert!(!reader.is_stale());
    }

    #[test]
    fn republish_is_seen() {
        let (fd, mut writer) = ResolveWriter::create().unwrap();
        let mut reader = ResolveReader::map_readonly(std::os::fd::AsRawFd::as_raw_fd(&fd)).unwrap();
        publish(&mut writer, &[IpAddr::V4(Ipv4Addr::LOCALHOST)], &[1]);
        assert_eq!(reader.read_if_changed().unwrap().addrs.len(), 1);
        publish(
            &mut writer,
            &[
                IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
                IpAddr::V4(Ipv4Addr::new(1, 0, 0, 1)),
            ],
            &[90, 10],
        );
        let got = reader.read_if_changed().unwrap();
        assert_eq!(got.addrs.len(), 2);
        assert_eq!(got.addrs[0], IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)));
        assert_eq!(got.revision, 2);
    }

    #[test]
    fn stale_detection() {
        let (_fd, mut writer) = ResolveWriter::create().unwrap();
        writer.publish(&[IpAddr::V4(Ipv4Addr::LOCALHOST)], &[1], 1, Instant::now());
        // Read back via a fresh reader on the same fd.
        let mut reader =
            ResolveReader::map_readonly(std::os::fd::AsRawFd::as_raw_fd(&_fd)).unwrap();
        let _ = reader.read_if_changed();
        // expires_mono_secs == mono_now (0-duration) → stale almost immediately.
        // Give it a second boundary.
        std::thread::sleep(Duration::from_millis(1100));
        assert!(reader.is_stale());
    }

    #[test]
    fn raw_addr_roundtrip() {
        let v4 = RawAddr::from_ip(IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)));
        assert_eq!(v4.to_ip(), IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)));
        let v6 = RawAddr::from_ip("2001:db8::1".parse::<IpAddr>().unwrap());
        assert_eq!(v6.to_ip(), "2001:db8::1".parse::<IpAddr>().unwrap());
    }

    fn set(addrs: &[&str], weights: &[u32], workers: usize) -> UpstreamSet {
        UpstreamSet {
            addrs: addrs.iter().map(|s| s.parse().unwrap()).collect(),
            weights: weights.to_vec(),
            revision: 1,
            workers: workers as u32,
            published_mono_secs: 0,
        }
    }

    #[test]
    fn slot_assignment_is_proportional() {
        // 6 workers, weights 3:2:1 → exactly 3/2/1 workers.
        let s = set(&["10.0.0.1", "10.0.0.2", "10.0.0.3"], &[300, 200, 100], 6);
        let mut counts = [0usize; 3];
        for i in 0..6 {
            counts[s.slot_for(i).unwrap()] += 1;
        }
        assert_eq!(counts, [3, 2, 1]);
    }

    #[test]
    fn slot_assignment_explores_the_tail() {
        // Best-first order with a small probe weight at the end: the last
        // worker must land on the probe candidate.
        let s = set(&["10.0.0.1", "10.0.0.2"], &[500, 100], 6);
        let mut counts = [0usize; 2];
        for i in 0..6 {
            counts[s.slot_for(i).unwrap()] += 1;
        }
        assert_eq!(counts, [5, 1]);
        assert_eq!(s.slot_for(5), Some(1));
    }

    #[test]
    fn slot_assignment_all_zero_is_even() {
        let s = set(&["10.0.0.1", "10.0.0.2", "10.0.0.3"], &[0, 0, 0], 3);
        let got: Vec<_> = (0..3).map(|i| s.slot_for(i).unwrap()).collect();
        assert_eq!(got, vec![0, 1, 2]);
    }

    #[test]
    fn slot_assignment_handles_more_workers_than_addrs() {
        let s = set(&["10.0.0.1", "10.0.0.2"], &[1, 1], 6);
        for i in 0..6 {
            assert!(s.slot_for(i).unwrap() < 2);
        }
        // Indices beyond the fleet still map somewhere sane (the selector is
        // only ever called with idx < workers).
        assert!(s.slot_for(6).unwrap() < 2);
    }

    #[test]
    fn empty_set_has_no_slot() {
        let s = set(&[], &[], 2);
        assert_eq!(s.slot_for(0), None);
    }

    #[test]
    fn weight_lookup() {
        let s = set(&["10.0.0.1", "10.0.0.2"], &[7, 3], 2);
        assert_eq!(s.weight_of("10.0.0.1".parse().unwrap()), 7);
        assert_eq!(s.weight_of("10.0.0.9".parse().unwrap()), 0);
    }

    #[test]
    fn score_write_then_read() {
        let (fd, reader) = ScoreReader::create().unwrap();
        let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
        let writer = ScoreWriter::map(raw, 2).unwrap();
        assert!(reader.read(2).is_none(), "unwritten slot must read as None");
        let sample = ScoreSample {
            addr: Some("104.16.132.229".parse().unwrap()),
            score: score_from_rtt(12_000),
            ok: 99,
            fail: 1,
            flags: FLAG_CONNECTED,
            updated_mono_secs: 0,
        };
        writer.write(&sample);
        let got = reader.read(2).expect("written slot must read back");
        assert_eq!(got.addr, sample.addr);
        assert_eq!(got.score, sample.score);
        assert_eq!(got.ok, 99);
        assert_eq!(got.flags & FLAG_CONNECTED, FLAG_CONNECTED);
        assert!(got.score > 0);
        // Other slots stay empty.
        assert!(reader.read(0).is_none());
    }

    #[test]
    fn score_falls_with_rtt_and_is_zero_when_unmeasured() {
        // Faster is better…
        assert!(score_from_rtt(20_000) > score_from_rtt(400_000));
        // …and "no measurement yet" is not a very fast RTT.
        assert_eq!(score_from_rtt(0), 0);
        assert_eq!(ScoreSample::default().score, 0);
    }
}
