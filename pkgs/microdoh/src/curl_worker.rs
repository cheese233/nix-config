//! curl Multi worker thread.
//!
//! Runs a `curl::multi::Multi` handle on a dedicated OS thread.
//! Uses `multi.wait()` + `multi.action()` for cross-platform event driving.
//! Communicates with the tokio UDP listener via channels.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use curl::easy::Easy2;
use curl::multi::{Easy2Handle, Events, Multi, WaitFd};
use tokio::sync::{mpsc, oneshot};

use crate::doh::{
    build_easy2_request, ensure_fresh, DnsRuntime, DohHandler,
};

/// Max new tasks to drain per loop iteration to avoid response starvation.
const MAX_DRAIN_PER_LOOP: usize = 64;
use crate::error::Result;
use crate::udp::DnsTask;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Spawn the curl worker thread.
pub fn spawn(
    rx: mpsc::UnboundedReceiver<DnsTask>,
    upstream: String,
    host: String,
    port: u16,
    bootstrap_dns: IpAddr,
    token: Option<Arc<str>>,
    timeout_secs: u64,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("curl-worker".into())
        .spawn(move || {
            if let Err(e) = worker_loop(rx, &upstream, &host, port, bootstrap_dns, &token, timeout_secs) {
                log::error!("curl worker exiting: {e}");
            }
        })
        .expect("failed to spawn curl worker thread")
}

// ---------------------------------------------------------------------------
// Pending transfer tracking
// ---------------------------------------------------------------------------

/// One in-flight transfer, keyed by a monotonically increasing token.
struct Pending {
    handle: Easy2Handle<DohHandler>,
    resp_tx: oneshot::Sender<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// Worker loop (blocking, runs on its own OS thread)
// ---------------------------------------------------------------------------

fn worker_loop(
    mut rx: mpsc::UnboundedReceiver<DnsTask>,
    upstream: &str,
    host: &str,
    port: u16,
    bootstrap_dns: IpAddr,
    token: &Option<Arc<str>>,
    timeout_secs: u64,
) -> Result<()> {
    // ── Bootstrap DNS runtime ──
    let dns_rt = DnsRuntime::new(bootstrap_dns)?;
    let state = Arc::new(RwLock::new(dns_rt.resolve(host, port)?));

    // ── curl Multi handle ──
    let mut multi = Multi::new();

    // ── Pending transfers keyed by token ──
    let mut pending: HashMap<usize, Pending> = HashMap::new();
    let mut next_token: usize = 0;

    // ── Kickstart ──
    multi.action(0, &Events::new()).map_err(|e| crate::error::Error::curl_multi(e))?;

    // ── Main event loop using multi.wait() — cross-platform ──
    loop {
        // 0. Ensure bootstrap DNS is fresh.
        ensure_fresh(&state, &dns_rt, host, port);

        // 1. Drain new tasks from the channel (non-blocking, bounded).
        let mut drained = 0;
        while drained < MAX_DRAIN_PER_LOOP {
            match rx.try_recv() {
                Ok(task) => {
                    let resolve_state = state.read().unwrap();
                    let easy = build_easy2_request(
                        task.query,
                        upstream,
                        token.as_deref(),
                        &resolve_state,
                        timeout_secs,
                    );
                    drop(resolve_state);
                    add_transfer(&mut multi, &mut pending, &mut next_token, easy, task.resp_tx);
                    drained += 1;
                }
                Err(_) => break,
            }
        }

        // 2. Drive transfers until there's nothing more to do right now.
        //    multi.action() returns the number of still-running handles.
        let running = multi.action(0, &Events::new()).map_err(|e| crate::error::Error::curl_multi(e))?;

        // 3. If nothing is running and channel is closed, exit.
        if running == 0 && rx.is_closed() && pending.is_empty() {
            break;
        }

        // 4. Wait for the next event.
        if running == 0 {
            // No transfers in flight — block until a new DNS query arrives.
            // blocking_recv() works from non-tokio threads.
            match rx.blocking_recv() {
                Some(task) => {
                    let resolve_state = state.read().unwrap();
                    let easy = build_easy2_request(
                        task.query,
                        upstream,
                        token.as_deref(),
                        &resolve_state,
                        timeout_secs,
                    );
                    drop(resolve_state);
                    add_transfer(&mut multi, &mut pending, &mut next_token, easy, task.resp_tx);
                }
                None => {
                    // Channel closed — finish remaining work.
                }
            }
        } else {
            // Transfers are in flight — wait for socket activity.
            let mut wait_fds: Vec<WaitFd> = Vec::new();
            match multi.wait(&mut wait_fds, Duration::from_millis(100)) {
                Ok(_n) => {}
                Err(e) => {
                    log::warn!("multi.wait: {e}");
                }
            }
        }

        // 5. Drive again after wait returns.
        multi.action(0, &Events::new()).map_err(|e| crate::error::Error::curl_multi(e))?;

        // 6. Collect completed transfers.
        collect_completed(&multi, &mut pending);

        // 7. Check if done.
        if rx.is_closed() && pending.is_empty() {
            break;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Add a new easy handle to the multi, tracking it in `pending`.
fn add_transfer(
    multi: &Multi,
    pending: &mut HashMap<usize, Pending>,
    next_token: &mut usize,
    easy: Easy2<DohHandler>,
    resp_tx: oneshot::Sender<Vec<u8>>,
) {
    let tok = *next_token;
    *next_token += 1;

    match multi.add2(easy) {
        Ok(mut handle) => {
            if let Err(e) = handle.set_token(tok) {
                log::warn!("set_token: {e}");
            }
            pending.insert(tok, Pending { handle, resp_tx });
        }
        Err(e) => {
            log::warn!("multi.add2: {e}");
        }
    }
}

/// Walk completed messages, extract responses, send them back, and remove
/// the easy handle from the multi and from `pending`.
fn collect_completed(
    multi: &Multi,
    pending: &mut HashMap<usize, Pending>,
) {
    let mut completed: Vec<usize> = Vec::new();

    multi.messages(|msg| {
        if msg.result().is_some() {
            match msg.token() {
                Ok(tok) => completed.push(tok),
                Err(e) => log::warn!("msg.token(): {e}"),
            }
        }
    });

    for tok in completed {
        if let Some(mut p) = pending.remove(&tok) {
            let status = p.handle.response_code().unwrap_or(0);
            let failed = status == 0 || !(200..300).contains(&status);

            if status == 0 {
                log::debug!("transfer {tok}: no HTTP response");
            } else if failed {
                log::warn!("transfer {tok}: HTTP {status}");
            }

            if failed {
                // Drop resp_tx → oneshot returns Canceled → UDP task skips reply.
                // The client retries, which is correct for a failed upstream query.
                drop(p.resp_tx);
            } else {
                let response = std::mem::take(&mut p.handle.get_mut().response);
                let _ = p.resp_tx.send(response);
            }
        }
    }
}
