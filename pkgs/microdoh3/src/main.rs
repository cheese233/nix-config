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

use worker::{WorkerConfig, run as worker_run};

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
        let n = if cli.workers == 0 {
            cores.len()
        } else {
            (cli.workers as usize).min(cores.len())
        };
        cores[..n.max(1)].to_vec()
    };

    // ── Create the read-only shared-memory IPC for upstream addresses.
    // The supervisor is the only writer; children map it PROT_READ.
    let (shm_fd, mut shm_writer) = match shared::ResolveWriter::create() {
        Ok(v) => v,
        Err(e) => {
            log::error!("shared memory init: {e}");
            exit(2);
        }
    };
    let shm_fd = std::os::fd::AsRawFd::as_raw_fd(&shm_fd);

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
                log::error!("bootstrap resolve of {} failed ({e}), retrying in 2s", upstream.host);
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    };
    // Publish IPv6-first (the sorted order children should prefer).
    let sorted_ips: Vec<IpAddr> = remotes.iter().map(|r| r.ip()).collect();
    shm_writer.publish(&sorted_ips, resolve_state.expires_at);
    log::info!(
        "microdoh3: {} worker(s) on cpu(s) {:?}, upstream https://{}:{}{}",
        cpus.len(),
        cpus,
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
        spawn_worker(cpu, idx, 0, &cli, &listen, &upstream, &token, shm_fd, &mut children);
    }

    // ── Supervise: reap children, keep bootstrap resolution fresh in
    // shared memory (cold path), shut down cleanly. All blocking DNS work
    // happens here in the supervisor, never in a child.
    let mut next_refresh = resolve_state.expires_at;
    /// Delay before retrying a failed bootstrap refresh.
    const RETRY_INTERVAL: Duration = Duration::from_secs(60);
    let bootstrap_server = SocketAddr::new(bootstrap_dns, 53);
    let mut pending: Option<bootstrap::AsyncResolve> = None;
    let mut readable = false;
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
                    spawn_worker(cpu, idx, fails, &cli, &listen, &upstream, &token, shm_fd, &mut children);
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
                        // Keep IPv6-first ordering for the children.
                        let mut v6: Vec<IpAddr> = ips.iter().filter(|a| a.is_ipv6()).copied().collect();
                        let mut v4: Vec<IpAddr> = ips.iter().filter(|a| a.is_ipv4()).copied().collect();
                        v6.append(&mut v4);
                        shm_writer.publish(&v6, expiry);
                        next_refresh = expiry;
                        pending = None;
                        log::info!("bootstrap refreshed {} → {v6:?} (ttl={ttl}s)", upstream.host);
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
        // Wake in time for the next scheduled worker restart.
        if let Some(t) = pending_restarts.values().map(|(_, t, _)| *t).min() {
            let ms = t
                .saturating_duration_since(now)
                .as_millis()
                .clamp(1, 1000);
            poll_timeout_ms = poll_timeout_ms.min(ms as u16);
        }

        // 4. Wait for the bootstrap socket (or timeout) — replaces sleep;
        //    signals interrupt the poll, children are reaped within a second.
        {
            use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
            readable = false;
            if let Some(r) = pending.as_ref() {
                let mut fds = [PollFd::new({ use std::os::fd::AsFd; r.socket().as_fd() }, PollFlags::POLLIN)];
                match poll(&mut fds, PollTimeout::try_from(poll_timeout_ms).unwrap_or(PollTimeout::MAX)) {
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
            "-l", "[::1]:5443",
            "-u", "https://dns.nextdns.io/abc",
            "--bootstrap-dns", "1.1.1.1",
            "--timeout-secs", "10",
            "--token", "secret",
            "--workers", "2",
            "--cpus", "0,2",
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
}
