//! Active upstream health checks. A background task per `Balancer` probes
//! every non-`down` peer at a configurable interval and reports the
//! outcome through the existing `Peer::record_success` / `record_failure`
//! API — so active and passive health share one state machine.
//!
//! The probe task holds a `Weak<Balancer>`; when the balancer is dropped
//! (e.g. by `SIGHUP` reload replacing the runtime), the task exits on its
//! next tick. No explicit JoinHandle bookkeeping is required.

use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use http_body_util::{BodyExt, Empty};
use hyper::{Method, Request};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tracing::{debug, warn};

use crate::app::Balancer;
use crate::body::ElrondBody;
use crate::config::HealthCheckCfg;

/// Spawn an active health-check loop for one balancer.
pub fn start(balancer: &Arc<Balancer>, cfg: HealthCheckCfg) {
    let weak: Weak<Balancer> = Arc::downgrade(balancer);
    let name = balancer.name.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let client = Client::builder(TokioExecutor::new()).build_http();
        loop {
            tick.tick().await;
            let b = match weak.upgrade() {
                Some(b) => b,
                None => {
                    debug!("health: balancer '{name}' dropped — exiting probe loop");
                    return;
                }
            };
            for peer in &b.peers {
                if peer.down {
                    continue;
                }
                let peer = peer.clone();
                let cfg = cfg.clone();
                let client = client.clone();
                tokio::spawn(async move {
                    probe_once(&client, &peer, &cfg).await;
                });
            }
        }
    });
}

async fn probe_once(
    client: &Client<HttpConnector, ElrondBody>,
    peer: &Arc<crate::app::Peer>,
    cfg: &HealthCheckCfg,
) {
    let url = format!("http://{}{}", peer.addr, cfg.uri);
    let req = match Request::builder()
        .method(Method::GET)
        .uri(&url)
        .header("user-agent", "elrond-healthcheck")
        .body(empty_body())
    {
        Ok(r) => r,
        Err(e) => {
            warn!("health: bad uri '{url}': {e}");
            peer.record_failure();
            return;
        }
    };
    let fut = client.request(req);
    let outcome = tokio::time::timeout(cfg.timeout, fut).await;
    match outcome {
        Ok(Ok(resp)) => {
            let status = resp.status().as_u16();
            if status == cfg.expected_status {
                peer.record_success();
                debug!("health: {} -> {} (ok)", peer.addr, status);
            } else {
                peer.record_failure();
                debug!(
                    "health: {} -> {} (expected {})",
                    peer.addr, status, cfg.expected_status
                );
            }
        }
        Ok(Err(e)) => {
            peer.record_failure();
            debug!("health: {} -> error {e}", peer.addr);
        }
        Err(_) => {
            peer.record_failure();
            debug!(
                "health: {} -> timeout after {:?}",
                peer.addr, cfg.timeout
            );
        }
    }
    let _ = (cfg.fails, cfg.passes); // tunables reserved for a richer state machine
}

fn empty_body() -> ElrondBody {
    Empty::<bytes::Bytes>::new()
        .map_err(|n: std::convert::Infallible| match n {})
        .boxed()
}

#[allow(unused)]
fn _ensure_duration_used(_: Duration) {}
