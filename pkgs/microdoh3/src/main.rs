//! microdoh3 — minimal DNS-over-HTTP/3 proxy.
//!
//! Prefork model: the supervisor parses the CLI, then forks one child per
//! physical CPU core. Each child pins itself to its core, opens its own
//! SO_REUSEPORT DNS socket and its own QUIC (HTTP/3) connection upstream,
//! and runs a single-threaded epoll event loop — no async runtime anywhere.

mod base64url;
mod bootstrap;
mod dns;
mod event;
mod h3;
mod huffman_table;
mod qpack;
mod qpack_static;
mod quic;
mod shared;
mod url;
mod worker;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use nix::sys::signal::{kill, sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{fork, ForkResult, Pid};

use worker::{run as worker_run, WorkerConfig};

/// DNS-over-HTTP/3 proxy (QUIC 0-RTT, prefork, per-core pinning).
#[derive(Parser, Debug)]
#[command(name = "microdoh3", version)]
pub struct Cli {
    /// Address to listen on for DNS queries (UDP).
    #[arg(long, short = 'l', default_value = "0.0.0.0:5300")]
    pub listen: String,

    /// DoH upstream URL (HTTP/3 only, e.g. `https://dns.google/dns-query`).
    #[arg(long, short = 'u', default_value = "https://dns.google/dns-query")]
    pub upstream: String,

    /// Bearer token for `Authorization` header. Read from `$MICRODOH_TOKEN` if not given.
    #[arg(long, env = "MICRODOH_TOKEN")]
    pub token: Option<String>,

    /// Read the bearer token from this file (overrides --token / env).
    #[arg(long)]
    pub token_file: Option<String>,

    /// Bootstrap DNS server for resolving the DoH upstream hostname.
    #[arg(long, default_value = "8.8.8.8")]
    pub bootstrap_dns: String,

    /// Publish IPv4 candidates instead of IPv6 ones.
    ///
    /// The candidate set is pinned to a single family, because a worker binds
    /// its QUIC socket once, from the first published address, and never
    /// rebinds. The default is IPv6-first (good on NAT64/DNS64 networks); this
    /// flag selects the IPv4 half instead, which is what you want when the
    /// IPv6 path to the upstream is the lossy one.
    #[arg(long)]
    pub prefer_ipv4: bool,

    /// Derive the worker CPUs (and worker count) from NIC queue XPS maps.
    ///
    /// With XPS, each TX queue is served by a specific CPU; pinning one worker
    /// per queue CPU keeps packet processing and the process on the same core.
    /// Falls back to physical cores when no XPS map is found.
    #[arg(long)]
    pub xps_cpus: bool,

    /// Interfaces to read XPS maps from (comma separated). Default: the
    /// interface carrying the default route.
    #[arg(long, value_delimiter = ',')]
    pub xps_interface: Vec<String>,

    /// Request timeout in seconds.
    #[arg(long, default_value = "30")]
    pub timeout_secs: u64,

    /// Pad DNS queries with EDNS0 padding to 128-byte blocks (RFC 8467).
    #[arg(long)]
    pub pad: bool,

    /// Number of worker processes (0 = one per physical CPU core).
    #[arg(long, default_value = "0")]
    pub workers: u32,

    /// Comma-separated CPU core IDs to pin workers to (overrides --workers).
    #[arg(long)]
    pub cpus: Option<String>,

    /// Enable SO_BUSY_POLL on the DNS socket (lower latency, more CPU).
    #[arg(long)]
    pub busy_poll: bool,

    /// Do a non-blocking event sweep before sleeping in epoll.
    #[arg(long)]
    pub spin: bool,

    /// Lock all current and future memory (avoid page faults in hot path).
    #[arg(long)]
    pub mlockall: bool,

    /// Enable verbose logging (debug level).
    #[arg(long, short = 'v')]
    pub verbose: bool,
}

/// Supervisor shutdown flag set by the signal handler.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_supervisor_signal(_sig: i32) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Discover physical CPU cores via sysfs topology; returns representative
/// CPU IDs sorted by (package, core). Falls back to all online CPUs.
fn physical_cores() -> Vec<u32> {
    let mut cores: std::collections::BTreeMap<(i64, i64), u32> = Default::default();
    let mut fallback: Vec<u32> = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/devices/system/cpu") {
        for entry in rd.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(cpu) = name.strip_prefix("cpu").and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            fallback.push(cpu);
            let topo = entry.path().join("topology");
            let pkg = std::fs::read_to_string(topo.join("physical_package_id"))
                .ok()
                .and_then(|s| s.trim().parse::<i64>().ok());
            let core = std::fs::read_to_string(topo.join("core_id"))
                .ok()
                .and_then(|s| s.trim().parse::<i64>().ok());
            if let (Some(pkg), Some(core)) = (pkg, core) {
                cores
                    .entry((pkg, core))
                    .and_modify(|e| *e = (*e).min(cpu))
                    .or_insert(cpu);
            }
        }
    }
    if cores.is_empty() {
        if fallback.is_empty() {
            // No sysfs (sandbox/container): fall back to the std probe.
            let n = std::thread::available_parallelism()
                .map(|v| v.get())
                .unwrap_or(1);
            return (0..n as u32).collect();
        }
        fallback.sort_unstable();
        fallback
    } else {
        cores.into_values().collect()
    }
}

fn pin_to_cpu(cpu: u32) {
    use nix::sched::{sched_setaffinity, CpuSet};
    let mut set = CpuSet::new();
    if set.set(cpu as usize).is_ok() {
        let _ = sched_setaffinity(Pid::from_raw(0), &set);
    }
}

/// Parse one `xps_cpus` value: a comma-separated list of 32-bit hex groups,
/// least significant group first (the kernel's cpumask format).
fn parse_cpumask(s: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for (group, part) in s.trim().split(',').rev().enumerate() {
        let Ok(bits) = u32::from_str_radix(part.trim(), 16) else {
            continue;
        };
        for bit in 0..32 {
            if bits & (1 << bit) != 0 {
                out.push(group as u32 * 32 + bit);
            }
        }
    }
    out.sort_unstable();
    out
}

/// CPUs that TX queues of `iface` are steered to (union over all TX queues),
/// or empty when the interface has no XPS map.
fn xps_cpus_of(iface: &str) -> Vec<u32> {
    let dir = format!("/sys/class/net/{iface}/queues");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut cpus = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("tx-") {
            continue;
        }
        let Ok(mask) = std::fs::read_to_string(e.path().join("xps_cpus")) else {
            continue;
        };
        for cpu in parse_cpumask(&mask) {
            if !cpus.contains(&cpu) {
                cpus.push(cpu);
            }
        }
    }
    cpus.sort_unstable();
    cpus
}

/// Interface carrying the default route (best-effort; used when the caller did
/// not name one explicitly).
fn default_route_iface() -> Option<String> {
    let Ok(routes) = std::fs::read_to_string("/proc/net/route") else {
        return None;
    };
    for line in routes.lines().skip(1) {
        let mut f = line.split_whitespace();
        let iface = f.next()?;
        let _dest = f.next()?;
        // A zero destination is the default route.
        if f.next().map(|f| f == "00000000").unwrap_or(false) {
            return Some(iface.to_string());
        }
    }
    None
}

/// Worker CPUs derived from NIC XPS maps, or empty when unavailable.
fn xps_worker_cpus(ifaces: &[String]) -> Vec<u32> {
    let chosen: Vec<String> = if ifaces.is_empty() {
        default_route_iface().into_iter().collect()
    } else {
        ifaces.to_vec()
    };
    let mut cpus = Vec::new();
    for iface in &chosen {
        for cpu in xps_cpus_of(iface) {
            if !cpus.contains(&cpu) {
                cpus.push(cpu);
            }
        }
    }
    cpus.sort_unstable();
    cpus
}

fn main() {
    let cli = Cli::parse();
    let default_level = if cli.verbose { "debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_level))
        .init();

    // ── Parse and validate configuration once, before forking ──
    let listen: SocketAddr = match cli.listen.parse() {
        Ok(a) => a,
        Err(e) => {
            log::error!("invalid --listen {}: {e}", cli.listen);
            exit(2);
        }
    };
    let upstream = match url::HttpsUrl::parse(&cli.upstream) {
        Ok(u) => u,
        Err(e) => {
            log::error!("invalid --upstream: {e}");
            exit(2);
        }
    };
    let bootstrap_dns: IpAddr = match cli.bootstrap_dns.parse() {
        Ok(a) => a,
        Err(e) => {
            log::error!("invalid --bootstrap-dns: {e}");
            exit(2);
        }
    };
    let token: Option<Arc<str>> = if let Some(ref path) = cli.token_file {
        match std::fs::read_to_string(path) {
            Ok(s) => Some(Arc::from(s.trim())),
            Err(e) => {
                log::error!("cannot read --token-file {path}: {e}");
                exit(2);
            }
        }
    } else {
        cli.token.as_deref().map(Arc::from)
    };

    // ── Determine worker count and CPU assignments ──
    let cores = physical_cores();
    let cpus: Vec<u32> = if let Some(ref list) = cli.cpus {
        match list
            .split(',')
            .map(|s| s.trim().parse::<u32>())
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(v) if !v.is_empty() => v,
            _ => {
                log::error!("invalid --cpus list: {list}");
                exit(2);
            }
        }
    } else {
        // XPS alignment: when the NIC steers each TX queue to a specific CPU,
        // one worker per queue CPU keeps packet processing local. This is a
        // hint, not a promise — on hosts whose traffic crosses a TUN or a
        // single-queue PPP device (so: no queue to align with) it just falls
        // back to the physical-core plan.
        let from_xps = if cli.xps_cpus {
            let v = xps_worker_cpus(&cli.xps_interface);
            if v.is_empty() {
                log::warn!("--xps-cpus: no XPS map found; using physical cores");
            } else {
                log::info!("--xps-cpus: queue CPUs {v:?}");
            }
            v
        } else {
            Vec::new()
        };
        let pool: &[u32] = if from_xps.is_empty() {
            &cores
        } else {
            &from_xps
        };
        let n = if cli.workers == 0 {
            pool.len()
        } else {
            (cli.workers as usize).min(pool.len())
        };
        pool[..n.max(1)].to_vec()
    };

    // ── Create the shared-memory IPC pages ──
    // resolve: supervisor writes, children map it PROT_READ.
    // scores:  children write one slot each, supervisor maps it PROT_READ.
    let (shm_fd, mut shm_writer) = match shared::ResolveWriter::create() {
        Ok(v) => v,
        Err(e) => {
            log::error!("shared memory init: {e}");
            exit(2);
        }
    };
    let shm_fd = std::os::fd::AsRawFd::as_raw_fd(&shm_fd);
    let (score_fd, scores) = match shared::ScoreReader::create() {
        Ok(v) => v,
        Err(e) => {
            log::error!("score shared memory init: {e}");
            exit(2);
        }
    };
    let score_fd = std::os::fd::AsRawFd::as_raw_fd(&score_fd);

    // ── Resolve the upstream ONCE here, before forking. Otherwise every
    // child would issue identical bootstrap queries at the same instant.
    // The result is published to shared memory; the supervisor keeps it
    // fresh on TTL expiry (cold path: no child ever does blocking DNS).
    let bootstrap = bootstrap::Bootstrap::new(bootstrap_dns);
    let (resolve_state, remotes) = loop {
        match bootstrap::resolve_upstream(&bootstrap, &upstream.host, upstream.port) {
            Ok(r) => break r,
            Err(e) => {
                if SHUTDOWN.load(Ordering::SeqCst) {
                    exit(0);
                }
                log::error!(
                    "bootstrap resolve of {} failed ({e}), retrying in 2s",
                    upstream.host
                );
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    };
    // ── Assemble the candidate address set ──
    // Whatever the DoH hostname resolves to, in bootstrap order. The weighted
    // selection below spreads the fleet across these by measured quality;
    // widening the pool is a DNS concern (serve more addresses for the name),
    // not a client one.
    let resolved: Vec<IpAddr> = remotes.iter().map(|r| r.ip()).collect();
    let mut candidates: Vec<IpAddr> = Vec::new();
    update_candidates(&mut candidates, &resolved, cli.prefer_ipv4);
    // No measurements exist yet: equal weights, so the fleet spreads across
    // every candidate and measures it before the weights start steering.
    let mut published: Vec<(IpAddr, u32)> = candidates.iter().map(|a| (*a, 1u32)).collect();
    shm_writer.publish(
        &candidates,
        &vec![1u32; candidates.len()],
        cpus.len(),
        resolve_state.expires_at,
    );
    log::info!(
        "microdoh3: {} worker(s) on cpu(s) {:?}, {} candidate upstream address(es) {:?}, upstream https://{}:{}{}",
        cpus.len(),
        cpus,
        candidates.len(),
        candidates,
        upstream.host,
        upstream.port,
        upstream.path
    );

    // ── Install supervisor signal handlers ──
    let action = SigAction::new(
        SigHandler::Handler(on_supervisor_signal),
        SaFlags::empty(), // no SA_RESTART: we want waitpid interrupted
        SigSet::empty(),
    );
    // NOTE: no SIGCHLD handler — waitpid already wakes on child death, and a
    // handler would clobber the SHUTDOWN flag semantics.
    unsafe {
        let _ = sigaction(Signal::SIGTERM, &action);
        let _ = sigaction(Signal::SIGINT, &action);
    }

    // ── Prefork workers ──
    // pid → (cpu, child_idx, started_at, fast_failures)
    let mut children: HashMap<Pid, (u32, usize, Instant, u32)> = HashMap::new();
    // cpu → (child_idx, restart_at, fast_failures): crash-backoff restarts,
    // scheduled instead of slept through so the loop never blocks.
    let mut pending_restarts: HashMap<u32, (usize, Instant, u32)> = HashMap::new();
    for (idx, &cpu) in cpus.iter().enumerate() {
        spawn_worker(
            cpu,
            idx,
            0,
            &cli,
            &listen,
            &upstream,
            &token,
            shm_fd,
            score_fd,
            &mut children,
        );
    }

    // ── Supervise: reap children, keep bootstrap resolution fresh in
    // shared memory (cold path), turn worker measurements into selection
    // weights, shut down cleanly. All blocking DNS work happens here in the
    // supervisor, never in a child.
    let mut next_refresh = resolve_state.expires_at;
    /// Delay before retrying a failed bootstrap refresh.
    const RETRY_INTERVAL: Duration = Duration::from_secs(60);
    /// How often the supervisor re-reads worker scores and republishes weights.
    const WEIGHT_INTERVAL: Duration = Duration::from_secs(2);
    /// A score older than this is ignored (the worker may have died).
    const SCORE_STALE_SECS: u64 = 30;
    let bootstrap_server = SocketAddr::new(bootstrap_dns, 53);
    let mut pending: Option<bootstrap::AsyncResolve> = None;
    let mut readable = false;
    // Rotates which candidate gets the exploration share each round.
    let mut probe_cursor: usize = 0;
    let mut next_weights = Instant::now() + WEIGHT_INTERVAL;
    loop {
        // 0. Spawn workers whose scheduled restart time has arrived.
        {
            let now = Instant::now();
            let due: Vec<u32> = pending_restarts
                .iter()
                .filter(|(_, (_, t, _))| *t <= now)
                .map(|(&c, _)| c)
                .collect();
            for cpu in due {
                if let Some((idx, _, fails)) = pending_restarts.remove(&cpu) {
                    spawn_worker(
                        cpu,
                        idx,
                        fails,
                        &cli,
                        &listen,
                        &upstream,
                        &token,
                        shm_fd,
                        score_fd,
                        &mut children,
                    );
                }
            }
        }

        // 1. Reap all exited children (non-blocking).
        loop {
            match waitpid(None, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, code)) => {
                    log::warn!("worker {pid} exited with code {code}");
                    schedule_child_exit(pid, &mut children, &mut pending_restarts);
                }
                Ok(WaitStatus::Signaled(pid, sig, _)) => {
                    log::warn!("worker {pid} killed by {sig}");
                    schedule_child_exit(pid, &mut children, &mut pending_restarts);
                }
                Ok(_) => break,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(nix::errno::Errno::ECHILD) => break,
                Err(e) => {
                    log::error!("waitpid: {e}");
                    break;
                }
            }
        }

        // 2. Shutdown?
        if SHUTDOWN.load(Ordering::SeqCst) {
            if children.is_empty() {
                break;
            }
            log::info!("supervisor: terminating {} worker(s)", children.len());
            for pid in children.keys() {
                let _ = kill(*pid, Signal::SIGTERM);
            }
            std::thread::sleep(Duration::from_millis(500));
            for pid in children.keys() {
                let _ = kill(*pid, Signal::SIGKILL);
            }
            children.clear();
            break;
        }

        // 3. Drive the non-blocking bootstrap refresh (never blocks the
        //    supervisor: reaping stays ≤1s even if the resolver is down).
        let now = Instant::now();
        if pending.is_none() && now >= next_refresh {
            match bootstrap::AsyncResolve::start(bootstrap_server, &upstream.host, now) {
                Ok(r) => pending = Some(r),
                Err(e) => {
                    log::warn!("bootstrap refresh start failed: {e}");
                    next_refresh = now + RETRY_INTERVAL;
                }
            }
        }
        let mut poll_timeout_ms = 1000u16;
        if let Some(r) = pending.as_mut() {
            if readable {
                match r.on_readable() {
                    Some(Ok((ips, ttl))) => {
                        let expiry = now + Duration::from_secs(ttl as u64);
                        // Refresh the DNS-derived candidates and keep any
                        // operator-seeded extras. Weights are published by the
                        // scoring step below, not here.
                        update_candidates(&mut candidates, &ips, cli.prefer_ipv4);
                        next_refresh = expiry;
                        pending = None;
                        log::info!(
                            "bootstrap refreshed {} → {:?} (ttl={ttl}s)",
                            upstream.host,
                            ips
                        );
                    }
                    Some(Err(e)) => {
                        log::warn!("bootstrap refresh failed (keeping stale): {e}");
                        next_refresh = now + RETRY_INTERVAL;
                        pending = None;
                    }
                    None => {}
                }
            }
            if let Some(r) = pending.as_mut() {
                if now >= r.deadline() {
                    if let Some(e) = r.on_timeout(now) {
                        log::warn!("bootstrap refresh timed out (keeping stale): {e}");
                        next_refresh = now + RETRY_INTERVAL;
                        pending = None;
                    }
                }
            }
            if let Some(r) = pending.as_ref() {
                let ms = r
                    .deadline()
                    .saturating_duration_since(now)
                    .as_millis()
                    .clamp(1, 1000);
                poll_timeout_ms = poll_timeout_ms.min(ms as u16);
            }
        }

        // 3b. Turn worker measurements into selection weights.
        //     Publishing only on change keeps the resolve page quiet: workers
        //     re-evaluate their assignment when — and only when — the weights
        //     actually moved.
        if now >= next_weights {
            next_weights = now + WEIGHT_INTERVAL;
            let now_secs = shared::mono_secs();
            let samples: Vec<_> = (0..cpus.len())
                .map(|i| {
                    scores.read(i).filter(|s| {
                        s.updated_mono_secs != 0
                            && now_secs.saturating_sub(s.updated_mono_secs) <= SCORE_STALE_SECS
                    })
                })
                .collect();
            let fresh = compute_weights(&candidates, &samples, cpus.len(), probe_cursor);
            probe_cursor = probe_cursor.wrapping_add(1);
            if fresh != published {
                let addrs: Vec<IpAddr> = fresh.iter().map(|(a, _)| *a).collect();
                let weights: Vec<u32> = fresh.iter().map(|(_, w)| *w).collect();
                log::debug!("weights: {fresh:?}");
                shm_writer.publish(&addrs, &weights, cpus.len(), now + Duration::from_secs(60));
                published = fresh;
            }
        }
        // Wake in time for the next scheduled worker restart.
        if let Some(t) = pending_restarts.values().map(|(_, t, _)| *t).min() {
            let ms = t.saturating_duration_since(now).as_millis().clamp(1, 1000);
            poll_timeout_ms = poll_timeout_ms.min(ms as u16);
        }

        // 4. Wait for the bootstrap socket (or timeout) — replaces sleep;
        //    signals interrupt the poll, children are reaped within a second.
        {
            use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
            readable = false;
            if let Some(r) = pending.as_ref() {
                let mut fds = [PollFd::new(
                    {
                        use std::os::fd::AsFd;
                        r.socket().as_fd()
                    },
                    PollFlags::POLLIN,
                )];
                match poll(
                    &mut fds,
                    PollTimeout::try_from(poll_timeout_ms).unwrap_or(PollTimeout::MAX),
                ) {
                    Ok(n) => readable = n > 0,
                    Err(nix::errno::Errno::EINTR) => {}
                    Err(_) => {}
                }
            } else {
                std::thread::sleep(Duration::from_millis(poll_timeout_ms as u64));
            }
        }
    }
    log::info!("supervisor: exit");
}

/// Backoff before restarting a repeatedly fast-failing worker.
/// fails=0 → immediate; fails=N → 2^N seconds, capped at 32s.
fn backoff_delay(fails: u32) -> Duration {
    if fails == 0 {
        Duration::ZERO
    } else {
        Duration::from_secs(1 << fails.min(5))
    }
}

/// Record a dead worker for scheduled restart (no sleeping here — the
/// supervisor loop must never block on backoff).
fn schedule_child_exit(
    pid: Pid,
    children: &mut HashMap<Pid, (u32, usize, Instant, u32)>,
    pending_restarts: &mut HashMap<u32, (usize, Instant, u32)>,
) {
    let Some((cpu, idx, started, fails)) = children.remove(&pid) else {
        return;
    };
    if SHUTDOWN.load(Ordering::SeqCst) {
        return;
    }
    let lived = started.elapsed();
    let fails = if lived < Duration::from_secs(10) {
        fails + 1
    } else {
        0
    };
    let delay = backoff_delay(fails);
    if fails > 0 {
        log::warn!("worker on cpu {cpu} died after {lived:.1?}; restart #{fails} in {delay:.1?}");
    }
    pending_restarts.insert(cpu, (idx, Instant::now() + delay, fails));
}

/// Rebuild the candidate list from a fresh DNS answer.
///
/// * A failed refresh must never wipe a working set, so an empty answer is
///   ignored (the previous resolution stays in force).
/// * The list is pinned to a single address family — the family of the first
///   address, which is what every worker binds its QUIC socket to. Publishing
///   the other family would create candidates no worker can ever dial, and
///   because they can never produce a score they would keep attracting the
///   exploration share forever.
fn update_candidates(candidates: &mut Vec<IpAddr>, resolved: &[IpAddr], prefer_ipv4: bool) {
    let mut next: Vec<IpAddr> = resolved.to_vec();
    if prefer_ipv4 {
        next.sort_by_key(|a| a.is_ipv6());
    }
    if let Some(first) = next.first() {
        let want_v6 = first.is_ipv6();
        next.retain(|a| a.is_ipv6() == want_v6);
    }
    if next.len() > shared::MAX_ADDRS {
        next.truncate(shared::MAX_ADDRS);
    }
    if !next.is_empty() {
        *candidates = next;
    }
}

/// Turn the workers' published scores into a relative weight per candidate.
///
/// A worker already publishes the score of the address it is sitting on, so
/// this only has to do three things:
///
/// * keep the best score reported for each address;
/// * give every address with no score a small floor, so an unmeasured or
///   currently-dead candidate still gets tried instead of dropping to zero;
/// * hand one *rotating* address with no score a full extra share, so a pool
///   with a dead member still cycles through it instead of pinning the same
///   one. Once every candidate has a score this changes nothing, and the
///   weights stop moving between rounds.
///
/// Candidates come back best-first: the deterministic slot assignment walks the
/// list, so ordering is what puts the highest worker indices on the probes.
fn compute_weights(
    candidates: &[IpAddr],
    samples: &[Option<shared::ScoreSample>],
    worker_count: usize,
    probe_cursor: usize,
) -> Vec<(IpAddr, u32)> {
    // Best score any worker reported for each candidate.
    let mut best = vec![0u32; candidates.len()];
    for s in samples.iter().flatten() {
        let Some(addr) = s.addr else { continue };
        if let Some(i) = candidates.iter().position(|a| *a == addr) {
            best[i] = best[i].max(s.score);
        }
    }

    let top = best.iter().copied().max().unwrap_or(0);
    let workers = worker_count.max(1) as u32;
    // Half a worker's share: enough that a pool larger than the fleet still
    // gets sampled, small enough that it cannot outvote what we measured.
    let floor = (top / (2 * workers)).max(1);
    // One worker's share, given to a different candidate each round.
    let one_slot = top / workers;
    let probe = if candidates.is_empty() {
        usize::MAX
    } else {
        probe_cursor % candidates.len()
    };

    let mut out: Vec<(IpAddr, u32)> = candidates
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let mut w = best[i].max(floor);
            // Only an address we have no score for is worth an extra probe:
            // probing a measured one would just shuffle the fleet every round.
            if i == probe && best[i] == 0 {
                w = w.saturating_add(one_slot);
            }
            (*a, w)
        })
        .collect();
    out.sort_by_key(|(_, w)| std::cmp::Reverse(*w));
    out
}

#[allow(clippy::too_many_arguments)]
fn spawn_worker(
    cpu: u32,
    child_idx: usize,
    fails: u32,
    cli: &Cli,
    listen: &SocketAddr,
    upstream: &url::HttpsUrl,
    token: &Option<Arc<str>>,
    shm_fd: std::os::fd::RawFd,
    score_fd: std::os::fd::RawFd,
    children: &mut HashMap<Pid, (u32, usize, Instant, u32)>,
) {
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => {
            children.insert(child, (cpu, child_idx, Instant::now(), fails));
            log::info!("worker pid {child} on cpu {cpu}");
        }
        Ok(ForkResult::Child) => {
            // Child: pin to core and run. All crypto/RNG init happens here,
            // after the fork. Never return to the supervisor.
            pin_to_cpu(cpu);
            let cfg = WorkerConfig {
                listen: *listen,
                upstream: upstream.clone(),
                token: token.clone(),
                timeout: Duration::from_secs(cli.timeout_secs),
                pad: cli.pad,
                busy_poll: cli.busy_poll,
                spin: cli.spin,
                mlockall: cli.mlockall,
                shm_fd,
                score_fd,
                child_idx,
            };
            match worker_run(cfg) {
                Ok(()) => exit(0),
                Err(e) => {
                    log::error!("worker on cpu {cpu} failed: {e}");
                    exit(1);
                }
            }
        }
        Err(e) => {
            log::error!("fork: {e}");
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_cores_nonempty() {
        let cores = physical_cores();
        assert!(!cores.is_empty());
    }

    #[test]
    fn backoff_delay_schedule() {
        assert_eq!(backoff_delay(0), Duration::ZERO);
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(2), Duration::from_secs(4));
        assert_eq!(backoff_delay(3), Duration::from_secs(8));
        assert_eq!(backoff_delay(4), Duration::from_secs(16));
        assert_eq!(backoff_delay(5), Duration::from_secs(32));
        // Capped at 32s beyond 2^5.
        assert_eq!(backoff_delay(6), Duration::from_secs(32));
        assert_eq!(backoff_delay(100), Duration::from_secs(32));
    }

    #[test]
    fn cli_defaults() {
        let cli = Cli::parse_from(["microdoh3"]);
        assert_eq!(cli.listen, "0.0.0.0:5300");
        assert_eq!(cli.upstream, "https://dns.google/dns-query");
        assert_eq!(cli.bootstrap_dns, "8.8.8.8");
        assert_eq!(cli.timeout_secs, 30);
        assert_eq!(cli.workers, 0);
        assert!(cli.cpus.is_none());
        assert!(!cli.pad);
        assert!(!cli.busy_poll);
        assert!(!cli.spin);
        assert!(!cli.mlockall);
    }

    #[test]
    fn cli_custom() {
        let cli = Cli::parse_from([
            "microdoh3",
            "-l",
            "[::1]:5443",
            "-u",
            "https://dns.nextdns.io/abc",
            "--bootstrap-dns",
            "1.1.1.1",
            "--timeout-secs",
            "10",
            "--token",
            "secret",
            "--workers",
            "2",
            "--cpus",
            "0,2",
            "--pad",
            "--busy-poll",
        ]);
        assert_eq!(cli.listen, "[::1]:5443");
        assert_eq!(cli.timeout_secs, 10);
        assert_eq!(cli.token.as_deref(), Some("secret"));
        assert_eq!(cli.workers, 2);
        assert_eq!(cli.cpus.as_deref(), Some("0,2"));
        assert!(cli.pad);
        assert!(cli.busy_poll);
    }

    #[test]
    fn cli_selection_flags() {
        let cli = Cli::parse_from([
            "microdoh3",
            "--prefer-ipv4",
            "--xps-cpus",
            "--xps-interface",
            "enp2s0f1,enp3s0",
        ]);
        assert!(cli.prefer_ipv4);
        assert!(cli.xps_cpus);
        assert_eq!(cli.xps_interface, vec!["enp2s0f1", "enp3s0"]);
        // Defaults stay opt-in.
        let d = Cli::parse_from(["microdoh3"]);
        assert!(!d.prefer_ipv4);
        assert!(!d.xps_cpus);
        assert!(d.xps_interface.is_empty());
    }

    #[test]
    fn cpumask_parsing() {
        // The kernel prints cpumasks with the most significant 32-bit group
        // first (`%*pb`), so the *last* printed group holds CPUs 0..31.
        assert_eq!(parse_cpumask("001"), vec![0]);
        assert_eq!(parse_cpumask("400"), vec![10]);
        assert_eq!(parse_cpumask("800"), vec![11]);
        assert_eq!(parse_cpumask("3f"), vec![0, 1, 2, 3, 4, 5]);
        // Only CPU 32 set: the high group is printed first.
        assert_eq!(parse_cpumask("00000001,00000000"), vec![32]);
        assert_eq!(parse_cpumask("00000002,00000000"), vec![33]);
        // Only CPU 0 set.
        assert_eq!(parse_cpumask("00000000,00000001"), vec![0]);
        assert_eq!(parse_cpumask("00000000,00000000"), Vec::<u32>::new());
        // CPU 30 (low group, bit 30) and CPU 32 (high group, bit 0).
        assert_eq!(parse_cpumask("00000001,40000000"), vec![30, 32]);
        assert_eq!(parse_cpumask("garbage"), Vec::<u32>::new());
    }

    #[test]
    fn candidate_refresh_replaces_and_never_wipes() {
        let mut c = vec!["1.1.1.1".parse().unwrap()];
        let resolved: Vec<IpAddr> = vec!["8.8.8.8".parse().unwrap(), "9.9.9.9".parse().unwrap()];
        update_candidates(&mut c, &resolved, false);
        let got: Vec<String> = c.iter().map(|a| a.to_string()).collect();
        assert_eq!(got, vec!["8.8.8.8", "9.9.9.9"]);
        // A failed refresh (empty answer) never wipes the working set.
        let before = c.clone();
        update_candidates(&mut c, &[], false);
        assert_eq!(c, before);
    }

    #[test]
    fn candidates_are_pinned_to_one_family() {
        // Default order (IPv6 first) keeps only the IPv6 half…
        let mut c = Vec::new();
        let resolved: Vec<IpAddr> = vec![
            "2606:4700::1".parse().unwrap(),
            "104.21.63.104".parse().unwrap(),
            "172.67.170.142".parse().unwrap(),
        ];
        update_candidates(&mut c, &resolved, false);
        assert_eq!(c, vec!["2606:4700::1".parse::<IpAddr>().unwrap()]);
        // …and --prefer-ipv4 flips which half that is.
        update_candidates(&mut c, &resolved, true);
        assert_eq!(
            c,
            vec![
                "104.21.63.104".parse::<IpAddr>().unwrap(),
                "172.67.170.142".parse::<IpAddr>().unwrap()
            ]
        );
    }

    /// A worker sitting on `addr` having reported `score`.
    fn sample(addr: &str, score: u32) -> shared::ScoreSample {
        shared::ScoreSample {
            addr: Some(addr.parse().unwrap()),
            score,
            ok: 10,
            fail: 0,
            flags: shared::FLAG_CONNECTED,
            updated_mono_secs: 1,
        }
    }

    fn slots(w: &[(IpAddr, u32)], workers: usize) -> Vec<usize> {
        let set = shared::UpstreamSet {
            addrs: w.iter().map(|(a, _)| *a).collect(),
            weights: w.iter().map(|(_, x)| *x).collect(),
            revision: 1,
            workers: workers as u32,
            published_mono_secs: 0,
        };
        (0..workers).map(|i| set.slot_for(i).unwrap()).collect()
    }

    #[test]
    fn weights_favour_the_faster_address() {
        let candidates: Vec<IpAddr> = ["10.0.0.1", "10.0.0.2"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        // 20 ms vs 400 ms.
        let samples = vec![
            Some(sample("10.0.0.1", shared::score_from_rtt(20_000))),
            Some(sample("10.0.0.2", shared::score_from_rtt(400_000))),
        ];
        let w = compute_weights(&candidates, &samples, 6, 0);
        assert_eq!(w[0].0, "10.0.0.1".parse::<IpAddr>().unwrap());
        assert!(w[0].1 > w[1].1, "faster must outrank slower: {w:?}");
        let s = slots(&w, 6);
        let fast = s.iter().filter(|i| **i == 0).count();
        assert!(fast > 3, "expected a skew towards the fast address: {s:?}");
    }

    #[test]
    fn unmeasured_addresses_keep_a_floor() {
        let candidates: Vec<IpAddr> = ["10.0.0.1", "10.0.0.2", "10.0.0.3"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        // Only the first address has ever produced a score.
        let samples = vec![Some(sample("10.0.0.1", shared::score_from_rtt(20_000)))];
        let w = compute_weights(&candidates, &samples, 6, 0);
        assert!(
            w.iter().all(|(_, x)| *x > 0),
            "no candidate may drop to zero: {w:?}"
        );
        // The untouched ones must still be sampled, thanks to the floor and the
        // rotating probe share.
        let s = slots(&w, 6);
        assert!(
            s.iter().any(|i| *i != 0),
            "unmeasured candidates must be explored: {s:?}"
        );
    }

    #[test]
    fn a_worker_with_no_score_does_not_win() {
        let candidates: Vec<IpAddr> = ["10.0.0.1", "10.0.0.2"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        // Second worker is connected but has not completed a request yet, so
        // its score is 0 and its address must not outrank a measured one.
        let samples = vec![
            Some(sample("10.0.0.1", shared::score_from_rtt(20_000))),
            Some(sample("10.0.0.2", 0)),
        ];
        let w = compute_weights(&candidates, &samples, 6, 99); // probe elsewhere
        assert_eq!(w[0].0, "10.0.0.1".parse::<IpAddr>().unwrap());
        let s = slots(&w, 6);
        assert!(s.iter().filter(|i| **i == 0).count() > 3, "{s:?}");
    }

    #[test]
    fn the_probe_share_rotates_over_unmeasured_addresses() {
        let candidates: Vec<IpAddr> = ["10.0.0.1", "10.0.0.2", "10.0.0.3"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let samples = vec![Some(sample("10.0.0.1", shared::score_from_rtt(20_000)))];
        let weight_of =
            |w: &Vec<(IpAddr, u32)>, s: &str| w.iter().find(|(a, _)| a.to_string() == s).unwrap().1;
        // Only .1 has ever been measured, so .2/.3 are the ones worth probing.
        // Cursor 1 promotes .2, cursor 2 promotes .3: the extra share moves on,
        // so a dead address cannot hog the probe forever.
        let c1 = compute_weights(&candidates, &samples, 6, 1);
        let c2 = compute_weights(&candidates, &samples, 6, 2);
        assert!(weight_of(&c1, "10.0.0.2") > weight_of(&c1, "10.0.0.3"));
        assert!(weight_of(&c2, "10.0.0.3") > weight_of(&c2, "10.0.0.2"));
    }

    #[test]
    fn weights_stop_moving_once_everything_is_measured() {
        let candidates: Vec<IpAddr> = ["10.0.0.1", "10.0.0.2"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let samples = vec![
            Some(sample("10.0.0.1", shared::score_from_rtt(20_000))),
            Some(sample("10.0.0.2", shared::score_from_rtt(40_000))),
        ];
        // No unmeasured candidate => the probe cursor is irrelevant, so the
        // supervisor stops republishing and the fleet stops re-evaluating.
        assert_eq!(
            compute_weights(&candidates, &samples, 6, 0),
            compute_weights(&candidates, &samples, 6, 1)
        );
    }

    #[test]
    fn weights_are_a_pure_function_of_the_inputs() {
        let candidates: Vec<IpAddr> = ["10.0.0.1", "10.0.0.2"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let samples = vec![Some(sample("10.0.0.1", shared::score_from_rtt(20_000)))];
        // The supervisor only republishes when this changes.
        assert_eq!(
            compute_weights(&candidates, &samples, 6, 3),
            compute_weights(&candidates, &samples, 6, 3)
        );
    }

    #[test]
    fn nothing_measured_means_an_even_split() {
        let candidates: Vec<IpAddr> = ["10.0.0.1", "10.0.0.2"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let samples = vec![None, None];
        let w = compute_weights(&candidates, &samples, 6, 0);
        assert_eq!(w[0].1, w[1].1);
        let s = slots(&w, 6);
        assert_eq!(s.iter().filter(|i| **i == 0).count(), 3);
    }
}
