use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server, StatusCode};
use solana_client::{
    nonblocking::rpc_client::RpcClient, rpc_request::RpcRequest, rpc_response::RpcVoteAccountStatus,
};
use solana_sdk::commitment_config::CommitmentConfig;

use std::convert::Infallible;
use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::task;
use tokio::time::{sleep, timeout};

const RPC_URL: &str = "https://api.testnet.solana.com";
const MIGRATION_MARKER: (u64, u64, u64) = (u64::MAX, u64::MAX, u64::MAX);
type FetchError = Box<dyn Error + Send + Sync>;

const LAMPORTS_PER_SOL: u128 = 1_000_000_000;

#[derive(Debug, Clone)]
struct ValidatorMetrics {
    pub vote_pubkey: String,
    pub root_distance: u64,
    pub vote_distance: u64,
    pub credits_earned: u64,
    /// Stake the runtime used for this epoch's rewards, from the same RPC response.
    pub activated_stake: u64,
    /// Rank by reward per staked SOL; 0 = unranked (no activated stake).
    pub rank: usize,
}

impl ValidatorMetrics {
    /// Reward lamports earned per staked SOL this epoch. Alpenglow pays each included
    /// slot as `pot * stake / total_stake`, so the raw lamport total is a stake table;
    /// dividing by the stake that earned it leaves inclusion performance.
    fn reward_per_sol(&self) -> Option<f64> {
        if self.activated_stake == 0 {
            return None;
        }
        Some(self.credits_earned as f64 * LAMPORTS_PER_SOL as f64 / self.activated_stake as f64)
    }
}

/// Exact descending order of `credits / stake` without floats: compares cross products.
fn cmp_reward_per_stake(a: &ValidatorMetrics, b: &ValidatorMetrics) -> std::cmp::Ordering {
    // a.credits/a.stake > b.credits/b.stake  <=>  a.credits*b.stake > b.credits*a.stake.
    // Descending order: a sorts first (Ordering::Less) when its ratio is the larger one.
    let a_cross = a.credits_earned as u128 * b.activated_stake as u128;
    let b_cross = b.credits_earned as u128 * a.activated_stake as u128;
    b_cross.cmp(&a_cross)
}

fn same_reward_per_stake(a: &ValidatorMetrics, b: &ValidatorMetrics) -> bool {
    cmp_reward_per_stake(a, b) == std::cmp::Ordering::Equal
}

#[derive(Debug, Clone)]
struct MetricsCache {
    pub data: String,
}

impl MetricsCache {
    fn new() -> Self {
        Self {
            data: export_prometheus_metrics(vec![], 0, 0, 0.0, 0),
        }
    }
}

// Alpenglow rewards are lamports (voter + leader), not Tower voting points.
fn epoch_rewards(
    history: &[(u64, u64, u64)],
    epoch: u64,
    migration_epoch: u64,
) -> Result<u64, FetchError> {
    if epoch < migration_epoch {
        return Err("RPC epoch precedes Alpenglow migration".into());
    }
    let segment = match history.iter().position(|entry| *entry == MIGRATION_MARKER) {
        Some(index) => &history[index + 1..],
        // During migration, an account without the marker has only Tower credits.
        None if epoch == migration_epoch => return Ok(0),
        None => history,
    };
    match segment.iter().rev().find(|entry| entry.0 == epoch) {
        Some((_, total, baseline)) => total
            .checked_sub(*baseline)
            .ok_or_else(|| "RPC credit total is below its baseline".into()),
        None => Ok(0),
    }
}

fn calculate_metrics(
    accounts: RpcVoteAccountStatus,
    epoch: u64,
    migration_epoch: u64,
) -> Result<(Vec<ValidatorMetrics>, usize), FetchError> {
    let top_root = accounts
        .current
        .iter()
        .map(|a| a.root_slot)
        .max()
        .unwrap_or(0);
    let top_vote = accounts
        .current
        .iter()
        .map(|a| a.last_vote)
        .max()
        .unwrap_or(0);
    let active_count = accounts.current.len();
    let mut validators = Vec::with_capacity(active_count);
    for account in accounts.current {
        validators.push(ValidatorMetrics {
            vote_pubkey: account.vote_pubkey,
            root_distance: top_root.saturating_sub(account.root_slot),
            vote_distance: top_vote.saturating_sub(account.last_vote),
            credits_earned: epoch_rewards(&account.epoch_credits, epoch, migration_epoch)?,
            activated_stake: account.activated_stake,
            rank: 0,
        });
    }
    // Staked accounts first, ordered by reward per staked SOL (exact, no floats);
    // zero-stake accounts last, unranked. Pubkey keeps output order deterministic.
    validators.sort_by(|a, b| {
        (a.activated_stake == 0)
            .cmp(&(b.activated_stake == 0))
            .then_with(|| cmp_reward_per_stake(a, b))
            .then_with(|| a.vote_pubkey.cmp(&b.vote_pubkey))
    });
    // Equal reward-per-stake shares a rank.
    for i in 0..validators.len() {
        if validators[i].activated_stake == 0 {
            validators[i].rank = 0;
            continue;
        }
        validators[i].rank = if i > 0 && same_reward_per_stake(&validators[i], &validators[i - 1]) {
            validators[i - 1].rank
        } else {
            i + 1
        };
    }
    Ok((validators, active_count))
}

async fn fetch_and_calculate_metrics(
    client: &RpcClient,
    migration_epoch: &mut Option<u64>,
) -> Result<(Vec<ValidatorMetrics>, usize, u64), FetchError> {
    let migration = match *migration_epoch {
        Some(epoch) => epoch,
        None => {
            let cert: serde_json::Value = client
                .send(
                    RpcRequest::Custom {
                        method: "getAgGenesisCert",
                    },
                    serde_json::json!([]),
                )
                .await?;
            let slot = cert
                .get("block")
                .and_then(|b| b.get("slot"))
                .and_then(|s| s.as_u64())
                .ok_or("RPC has no Alpenglow genesis certificate")?;
            let schedule = client.get_epoch_schedule().await?;
            let epoch = schedule.get_epoch(slot);
            *migration_epoch = Some(epoch);
            epoch
        }
    };
    let before = client.get_epoch_info().await?;
    let accounts = client.get_vote_accounts().await?;
    let after = client.get_epoch_info().await?;
    if before.epoch != after.epoch {
        return Err("epoch changed during RPC sample; retrying next poll".into());
    }
    let (validators, count) = calculate_metrics(accounts, after.epoch, migration)?;
    Ok((validators, count, after.epoch))
}

fn export_prometheus_metrics(
    validators: Vec<ValidatorMetrics>,
    active_count: usize,
    rpc_status: u8,
    rpc_duration: f64,
    rpc_timeout: u8,
) -> String {
    let mut output = String::new();

    // Rating position for each staked validator (Rank by reward per staked SOL)
    output.push_str("# HELP solana_validator_rating_position Rank by current-epoch Alpenglow reward lamports per staked SOL (voter plus leader); zero-stake accounts are unranked\n");
    output.push_str("# TYPE solana_validator_rating_position gauge\n");
    for validator in validators.iter().filter(|v| v.rank > 0) {
        output.push_str(&format!(
            "solana_validator_rating_position{{identity=\"{}\"}} {}\n",
            validator.vote_pubkey, validator.rank
        ));
    }

    // Reward per staked SOL for each staked validator
    output.push_str("# HELP solana_validator_alpenglow_reward_per_sol Current-epoch accrued reward lamports divided by activated stake in SOL, from one getVoteAccounts response\n");
    output.push_str("# TYPE solana_validator_alpenglow_reward_per_sol gauge\n");
    for validator in &validators {
        if let Some(per_sol) = validator.reward_per_sol() {
            output.push_str(&format!(
                "solana_validator_alpenglow_reward_per_sol{{identity=\"{}\"}} {}\n",
                validator.vote_pubkey, per_sol
            ));
        }
    }

    // Activated stake for each validator
    output.push_str("# HELP solana_validator_activated_stake_lamports Stake delegated to the vote account for the current epoch, in lamports\n");
    output.push_str("# TYPE solana_validator_activated_stake_lamports gauge\n");
    for validator in &validators {
        output.push_str(&format!(
            "solana_validator_activated_stake_lamports{{identity=\"{}\"}} {}\n",
            validator.vote_pubkey, validator.activated_stake
        ));
    }

    // Root distance for each validator
    output.push_str("# HELP solana_validator_root_distance Final-certificate root distance from the highest current validator root, in slots\n");
    output.push_str("# TYPE solana_validator_root_distance gauge\n");
    for validator in &validators {
        output.push_str(&format!(
            "solana_validator_root_distance{{identity=\"{}\"}} {}\n",
            validator.vote_pubkey, validator.root_distance
        ));
    }

    // Vote distance for each validator
    output.push_str("# HELP solana_validator_vote_distance Reward-account lastVote distance from the highest current validator lastVote, in slots\n");
    output.push_str("# TYPE solana_validator_vote_distance gauge\n");
    for validator in &validators {
        output.push_str(&format!(
            "solana_validator_vote_distance{{identity=\"{}\"}} {}\n",
            validator.vote_pubkey, validator.vote_distance
        ));
    }

    // Credits earned for each validator
    output.push_str("# HELP solana_validator_credits_so_far Compatibility alias for current-epoch Alpenglow reward lamports (not Tower credits)\n");
    output.push_str("# TYPE solana_validator_credits_so_far gauge\n");
    for validator in &validators {
        output.push_str(&format!(
            "solana_validator_credits_so_far{{identity=\"{}\"}} {}\n",
            validator.vote_pubkey, validator.credits_earned
        ));
    }

    output.push_str("# HELP solana_validator_alpenglow_rewards_lamports Current-epoch accrued voter plus leader reward lamports before distribution\n");
    output.push_str("# TYPE solana_validator_alpenglow_rewards_lamports gauge\n");
    for validator in &validators {
        output.push_str(&format!(
            "solana_validator_alpenglow_rewards_lamports{{identity=\"{}\"}} {}\n",
            validator.vote_pubkey, validator.credits_earned
        ));
    }

    // Top validator metrics: reward per staked SOL at ranked positions (staked accounts only)
    let ranked: Vec<&ValidatorMetrics> = validators.iter().filter(|v| v.rank > 0).collect();
    for (name, position) in [("top_1", 0usize), ("top_100", 99), ("top_200", 199)] {
        output.push_str(&format!(
            "# HELP solana_validator_{name} Alpenglow reward lamports per staked SOL at ranked position {}\n",
            position + 1
        ));
        output.push_str(&format!("# TYPE solana_validator_{name} gauge\n"));
        if let Some(per_sol) = ranked.get(position).and_then(|v| v.reward_per_sol()) {
            output.push_str(&format!("solana_validator_{name} {per_sol}\n"));
        }
    }

    // Active validator count
    output.push_str("# HELP solana_validator_active Number of RPC current validators, including zero-reward accounts\n");
    output.push_str("# TYPE solana_validator_active gauge\n");
    output.push_str(&format!("solana_validator_active {}\n", active_count));

    // RPC response status
    output.push_str("# HELP solana_validator_exporter_last_rpc_status RPC response status (1=success, 0=failure)\n");
    output.push_str("# TYPE solana_validator_exporter_last_rpc_status gauge\n");
    output.push_str(&format!(
        "solana_validator_exporter_last_rpc_status {}\n",
        rpc_status
    ));

    // RPC response timeout
    output.push_str("# HELP solana_validator_exporter_rpc_response_timeout RPC response timeout (1=timeout, 0=no timeout)\n");
    output.push_str("# TYPE solana_validator_exporter_rpc_response_timeout gauge\n");
    output.push_str(&format!(
        "solana_validator_exporter_rpc_response_timeout {}\n",
        rpc_timeout
    ));

    // RPC duration
    output.push_str(
        "# HELP solana_validator_exporter_rpc_duration_seconds RPC response time in seconds\n",
    );
    output.push_str("# TYPE solana_validator_exporter_rpc_duration_seconds gauge\n");
    output.push_str(&format!(
        "solana_validator_exporter_rpc_duration_seconds {}\n",
        rpc_duration
    ));

    output
}

// HTTP handler for serving Prometheus metrics
async fn serve_metrics(
    req: Request<Body>,
    cache: Arc<Mutex<MetricsCache>>,
) -> Result<Response<Body>, Infallible> {
    if req.uri().path() == "/metrics" {
        let cache = cache.lock().await;
        Ok(Response::builder()
            .header(
                hyper::header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )
            .body(Body::from(cache.data.clone()))
            .unwrap())
    } else {
        let not_found = Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("404 Not Found"))
            .unwrap();
        Ok(not_found)
    }
}

// Main function to run the exporter
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cache = Arc::new(Mutex::new(MetricsCache::new()));
    let cache_clone = Arc::clone(&cache);

    // Background task to fetch and update metrics
    task::spawn(async move {
        let client =
            RpcClient::new_with_commitment(RPC_URL.to_string(), CommitmentConfig::finalized());
        let mut migration_epoch = None;

        loop {
            let start = Instant::now();
            let result = timeout(Duration::from_secs_f32(4.5), async {
                fetch_and_calculate_metrics(&client, &mut migration_epoch).await
            })
            .await;

            // Only lock the cache when updating it
            let new_data = match result {
                Ok(Ok((validator_metrics, active_count, epoch))) => {
                    let duration = start.elapsed().as_secs_f64();
                    let mut data =
                        export_prometheus_metrics(validator_metrics, active_count, 1, duration, 0);
                    data.push_str(&format!("# HELP solana_validator_exporter_epoch Epoch of this successful sample\n# TYPE solana_validator_exporter_epoch gauge\nsolana_validator_exporter_epoch {}\n", epoch));
                    data
                }
                Ok(Err(error)) => {
                    eprintln!("Testnet RPC sample failed: {error}");
                    export_prometheus_metrics(vec![], 0, 0, start.elapsed().as_secs_f64(), 0)
                }
                Err(_) => {
                    eprintln!("Testnet RPC sample timed out after 4.5 seconds");
                    export_prometheus_metrics(vec![], 0, 0, start.elapsed().as_secs_f64(), 1)
                }
            };

            // Update the cache outside the main loop to minimize the lock time
            {
                let mut cache = cache_clone.lock().await;
                cache.data = new_data;
            }

            // Allow two seconds between completed polling attempts.
            sleep(Duration::from_secs(2)).await;
        }
    });

    // Serve metrics on all interfaces, port 59872, only for the `/metrics` route.
    // Reachability is governed by the host firewall, not by the bind address.
    let addr = ([0, 0, 0, 0], 59872).into();
    let make_svc = make_service_fn(move |_conn| {
        let cache = Arc::clone(&cache);
        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                let cache = Arc::clone(&cache);
                async move { serve_metrics(req, cache).await } // Pass `req` and `cache`
            }))
        }
    });
    let server = Server::bind(&addr).serve(make_svc);

    println!("Serving metrics on http://0.0.0.0:59872/metrics");
    server.await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_fixture_restores_accounts_without_tower_rewards() {
        let accounts =
            serde_json::from_str(include_str!("../tests/fixtures/testnet-migration.json")).unwrap();
        let (validators, count) = calculate_metrics(accounts, 1042, 1042).unwrap();
        assert_eq!(count, 2);
        // More raw lamports (5.44e9 on 347,671 SOL) ranks BELOW fewer lamports on less
        // stake (4.92e9 on 296,603 SOL): 15,649.6 vs 16,600.0 lamports per staked SOL.
        assert_eq!(validators[0].credits_earned, 4_923_619_318);
        assert_eq!(validators[0].activated_stake, 296_603_043_095_931);
        assert_eq!(validators[1].credits_earned, 5_440_927_420);
        assert_eq!(validators[1].activated_stake, 347_671_486_449_531);
        assert_eq!(validators[0].rank, 1);
        assert_eq!(validators[1].rank, 2);
        assert_eq!(validators[0].reward_per_sol(), Some(16600.029678075633));
        assert_eq!(validators[1].reward_per_sol(), Some(15649.622221147609));
        let text = export_prometheus_metrics(validators, count, 1, 0.2, 0);
        assert!(text.contains("solana_validator_active 2\n"));
        assert!(text.contains("solana_validator_top_1 16600.029678075633\n"));
        assert!(text.contains("solana_validator_alpenglow_reward_per_sol{identity=\"9LjQ5UC1"));
        assert!(text.contains(
            "solana_validator_activated_stake_lamports{identity=\"CiHvrTZsFoPpziogcnfMWfWnnZiNvxjSz3et5PehMA5D\"} 347671486449531\n"
        ));
        assert_eq!(
            text.lines()
                .filter(|l| l.starts_with("solana_validator_alpenglow_rewards_lamports{"))
                .count(),
            2
        );
    }

    #[test]
    fn zero_stake_accounts_are_exported_but_unranked() {
        let mut json: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/testnet-migration.json")).unwrap();
        json["current"][0]["activatedStake"] = serde_json::json!(0);
        let accounts = serde_json::from_value(json).unwrap();
        let (validators, count) = calculate_metrics(accounts, 1042, 1042).unwrap();
        assert_eq!(count, 2);
        // The staked account ranks first; the zero-stake one is last with rank 0.
        assert_eq!(validators[0].rank, 1);
        assert_eq!(validators[0].activated_stake, 296_603_043_095_931);
        assert_eq!(validators[1].rank, 0);
        assert_eq!(validators[1].activated_stake, 0);
        assert_eq!(validators[1].reward_per_sol(), None);
        let text = export_prometheus_metrics(validators, count, 1, 0.2, 0);
        assert_eq!(
            text.lines()
                .filter(|l| l.starts_with("solana_validator_rating_position{"))
                .count(),
            1
        );
        assert_eq!(
            text.lines()
                .filter(|l| l.starts_with("solana_validator_alpenglow_reward_per_sol{"))
                .count(),
            1
        );
        // Raw lamports and stake are still exported for the unranked account.
        assert!(text.contains(
            "solana_validator_activated_stake_lamports{identity=\"CiHvrTZsFoPpziogcnfMWfWnnZiNvxjSz3et5PehMA5D\"} 0\n"
        ));
        assert!(text.contains(
            "solana_validator_alpenglow_rewards_lamports{identity=\"CiHvrTZsFoPpziogcnfMWfWnnZiNvxjSz3et5PehMA5D\"} 5440927420\n"
        ));
        // No top-100/200 values with one ranked account; top_1 is present.
        assert!(text.contains("solana_validator_top_1 16600.029678075633\n"));
        assert!(!text
            .lines()
            .any(|l| l.starts_with("solana_validator_top_100 ")));
    }

    #[test]
    fn reward_per_stake_order_is_exact() {
        let v = |credits, stake| ValidatorMetrics {
            vote_pubkey: String::new(),
            root_distance: 0,
            vote_distance: 0,
            credits_earned: credits,
            activated_stake: stake,
            rank: 0,
        };
        // 3/2 > 4/3 ; 2/4 == 1/2 ; u64::MAX cross products must not overflow.
        assert_eq!(
            cmp_reward_per_stake(&v(3, 2), &v(4, 3)),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            cmp_reward_per_stake(&v(4, 3), &v(3, 2)),
            std::cmp::Ordering::Greater
        );
        assert!(same_reward_per_stake(&v(2, 4), &v(1, 2)));
        assert_eq!(
            cmp_reward_per_stake(&v(u64::MAX, u64::MAX), &v(u64::MAX, u64::MAX)),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn migration_marker_and_duplicate_epoch_are_separate_segments() {
        let history = [(1042, 100, 50), MIGRATION_MARKER, (1042, 125, 100)];
        assert_eq!(epoch_rewards(&history, 1042, 1042).unwrap(), 25);
        assert_eq!(epoch_rewards(&history[..2], 1042, 1042).unwrap(), 0);
        assert_eq!(epoch_rewards(&history[..1], 1042, 1042).unwrap(), 0);
        assert_eq!(
            epoch_rewards(&[MIGRATION_MARKER, (1042, 25, 0)], 1042, 1042).unwrap(),
            25
        );
    }

    #[test]
    fn post_migration_single_entry_and_history_without_marker() {
        assert_eq!(epoch_rewards(&[(1043, 150, 100)], 1043, 1042).unwrap(), 50);
        assert_eq!(epoch_rewards(&[(1100, 900, 700)], 1100, 1042).unwrap(), 200);
        assert_eq!(epoch_rewards(&[(1043, 150, 100)], 1044, 1042).unwrap(), 0);
        assert_eq!(epoch_rewards(&[], 1043, 1042).unwrap(), 0);
        assert!(epoch_rewards(&[(1043, 1, 100)], 1043, 1042).is_err());
        assert!(epoch_rewards(&[], 1041, 1042).is_err());
    }

    #[test]
    fn epoch_rollover_keeps_current_accounts_and_ties_have_same_rank() {
        let accounts =
            serde_json::from_str(include_str!("../tests/fixtures/testnet-migration.json")).unwrap();
        let (validators, count) = calculate_metrics(accounts, 1043, 1042).unwrap();
        assert_eq!(count, 2);
        assert!(validators
            .iter()
            .all(|v| v.credits_earned == 0 && v.rank == 1));
        assert!(validators[0].vote_pubkey < validators[1].vote_pubkey);
    }

    #[tokio::test]
    async fn http_routes_and_startup_health_are_exposed() {
        let cache = Arc::new(Mutex::new(MetricsCache::new()));
        let response = serve_metrics(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
            cache.clone(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers()[hyper::header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .contains("version=0.0.4"));
        let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
        assert!(std::str::from_utf8(&body)
            .unwrap()
            .contains("solana_validator_exporter_last_rpc_status 0\n"));
        let response = serve_metrics(Request::new(Body::empty()), cache)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // A real local HTTP transport checks the async RPC path, including cancellation.
    async fn mock_rpc(
        results: Vec<serde_json::Value>,
        delay: Duration,
    ) -> (RpcClient, tokio::task::JoinHandle<()>) {
        let results = Arc::new(Mutex::new(std::collections::VecDeque::from(results)));
        let service = make_service_fn(move |_| {
            let results = results.clone();
            async move {
                Ok::<_, Infallible>(service_fn(move |request: Request<Body>| {
                    let results = results.clone();
                    async move {
                        sleep(delay).await;
                        let body = hyper::body::to_bytes(request.into_body()).await.unwrap();
                        let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        let result = if request["method"] == "getVersion" {
                            serde_json::json!({"solana-core":"4.3.0", "feature-set":3383571666_u64})
                        } else {
                            results
                                .lock()
                                .await
                                .pop_front()
                                .unwrap_or(serde_json::Value::Null)
                        };
                        Ok::<_, Infallible>(Response::new(Body::from(
                            serde_json::json!({"jsonrpc":"2.0", "id":request["id"], "result":result})
                                .to_string(),
                        )))
                    }
                }))
            }
        });
        let server = Server::bind(&([127, 0, 0, 1], 0).into()).serve(service);
        let client = RpcClient::new(format!("http://{}", server.local_addr()));
        let task = tokio::spawn(async move {
            server.await.unwrap();
        });
        (client, task)
    }

    #[tokio::test]
    async fn crossing_epoch_boundary_rejects_inconsistent_sample() {
        let epoch = |n| {
            serde_json::json!({"epoch":n, "slotIndex":0, "slotsInEpoch":432000,
            "absoluteSlot":444620256, "blockHeight":1, "transactionCount":1})
        };
        let accounts =
            serde_json::from_str(include_str!("../tests/fixtures/testnet-migration.json")).unwrap();
        let (client, server) =
            mock_rpc(vec![epoch(1042), accounts, epoch(1043)], Duration::ZERO).await;
        let result = fetch_and_calculate_metrics(&client, &mut Some(1042)).await;
        server.abort();
        let error = result.unwrap_err().to_string();
        assert!(error.contains("epoch changed"), "{error}");
    }

    #[tokio::test]
    async fn stalled_rpc_is_cancellable() {
        let (client, server) = mock_rpc(vec![], Duration::from_secs(5)).await;
        let result = timeout(
            Duration::from_millis(50),
            fetch_and_calculate_metrics(&client, &mut None),
        )
        .await;
        server.abort();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn missing_genesis_certificate_is_not_treated_as_rewards() {
        let (client, server) = mock_rpc(vec![serde_json::Value::Null], Duration::ZERO).await;
        let result = fetch_and_calculate_metrics(&client, &mut None).await;
        server.abort();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no Alpenglow genesis"));
    }

    #[tokio::test]
    #[ignore = "queries the live public testnet RPC"]
    async fn live_testnet_sample() {
        let client =
            RpcClient::new_with_commitment(RPC_URL.to_string(), CommitmentConfig::finalized());
        let (validators, count, epoch) = timeout(
            Duration::from_secs_f32(4.5),
            fetch_and_calculate_metrics(&client, &mut None),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(count > 0);
        assert_eq!(count, validators.len());
        assert!(validators.iter().any(|v| v.credits_earned > 0));
        let output = export_prometheus_metrics(validators, count, 1, 0.0, 0);
        assert!(output.contains("solana_validator_top_1 "));
        println!("Testnet epoch {epoch}: exported {count} validators with Alpenglow rewards");
    }
}
