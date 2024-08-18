use hyper::{Body, Request, Response, Server, StatusCode};
use hyper::service::{make_service_fn, service_fn};
use solana_client::rpc_client::RpcClient;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task;
use tokio::time::{sleep, timeout};
use tokio::sync::Mutex;
use std::error::Error;
use std::convert::Infallible;

#[derive(Debug, Clone)]
struct ValidatorMetrics {
    pub vote_pubkey: String,
    pub root_distance: u64,
    pub vote_distance: u64,
    pub credits_earned: u64,
    pub rank: usize,
}

#[derive(Debug, Clone)]
struct MetricsCache {
    pub data: String,
}

impl MetricsCache {
    fn new() -> Self {
        Self {
            data: String::new(),
        }
    }
}

fn fetch_and_calculate_metrics(client: &RpcClient) -> Result<(Vec<ValidatorMetrics>, usize), Box<dyn Error + Send + Sync>> {
    let vote_accounts = client.get_vote_accounts()?;
    let top_root_slot = vote_accounts.current.iter().map(|v| v.root_slot).max().unwrap_or(0);
    let top_vote_slot = vote_accounts.current.iter().map(|v| v.last_vote).max().unwrap_or(0);
    let mut validator_metrics: Vec<ValidatorMetrics> = Vec::new();
    let mut active_count = 0;

    for account in vote_accounts.current {
        if let Some((_, credits_earned, _)) = account.epoch_credits.last() {
            if *credits_earned > 0 {
                active_count += 1;
                let root_distance = top_root_slot.saturating_sub(account.root_slot);
                let vote_distance = top_vote_slot.saturating_sub(account.last_vote);

                validator_metrics.push(ValidatorMetrics {
                    vote_pubkey: account.vote_pubkey.clone(),
                    root_distance,
                    vote_distance,
                    credits_earned: *credits_earned,
                    rank: 0,
                });
            }
        }
    }

    validator_metrics.sort_by(|a, b| b.credits_earned.cmp(&a.credits_earned));
    for (rank, validator) in validator_metrics.iter_mut().enumerate() {
        validator.rank = rank + 1;
    }

    Ok((validator_metrics, active_count))
}

fn export_prometheus_metrics(validators: Vec<ValidatorMetrics>, active_count: usize, rpc_status: u8, rpc_duration: f64, rpc_timeout: u8) -> String {
    let mut output = String::new();
    
    // per-validator metrics
    output.push_str("# HELP solana_validator Metrics for each validator\n");
    output.push_str("# TYPE solana_validator gauge\n");
    for validator in &validators {
        output.push_str(&format!(
            "solana_validator{{identity=\"{}\",root_distance=\"{}\",vote_distance=\"{}\",credits_so_far=\"{}\"}} {}\n",
            validator.vote_pubkey,
            validator.root_distance,
            validator.vote_distance,
            validator.credits_earned,
            validator.rank,
        ));
    }

    // top validators
    output.push_str("# HELP solana_validator_top_1 Credits earned by the top 1 validator\n");
    output.push_str("# TYPE solana_validator_top_1 gauge\n");
    if let Some(top_1) = validators.get(0) {
        output.push_str(&format!("solana_validator_top_1 {}\n", top_1.credits_earned));
    }

    output.push_str("# HELP solana_validator_top_100 Credits earned by the top 100 validator\n");
    output.push_str("# TYPE solana_validator_top_100 gauge\n");
    if let Some(top_100) = validators.get(99) {
        output.push_str(&format!("solana_validator_top_100 {}\n", top_100.credits_earned));
    }

    output.push_str("# HELP solana_validator_top_200 Credits earned by the top 200 validator\n");
    output.push_str("# TYPE solana_validator_top_200 gauge\n");
    if let Some(top_200) = validators.get(199) {
        output.push_str(&format!("solana_validator_top_200 {}\n", top_200.credits_earned));
    }

    // Active validator count
    output.push_str("# HELP solana_validator_active Total number of active validators\n");
    output.push_str("# TYPE solana_validator_active gauge\n");
    output.push_str(&format!("solana_validator_active {}\n", active_count));

    // RPC response status
    output.push_str("# HELP solana_validator_exporter_last_rpc_status RPC response status (1=success, 0=failure)\n");
    output.push_str("# TYPE solana_validator_exporter_last_rpc_status gauge\n");
    output.push_str(&format!("solana_validator_exporter_last_rpc_status {}\n", rpc_status));

    // RPC response timeout
    output.push_str("# HELP solana_validator_exporter_rpc_response_timeout RPC response timeout (1=timeout, 0=no timeout)\n");
    output.push_str("# TYPE solana_validator_exporter_rpc_response_timeout gauge\n");
    output.push_str(&format!("solana_validator_exporter_rpc_response_timeout {}\n", rpc_timeout));

    // RPC duration
    output.push_str("# HELP solana_validator_exporter_rpc_duration_seconds RPC response time in seconds\n");
    output.push_str("# TYPE solana_validator_exporter_rpc_duration_seconds gauge\n");
    output.push_str(&format!("solana_validator_exporter_rpc_duration_seconds {}\n", rpc_duration));

    output
}

// HTTP handler for serving Prometheus metrics
async fn serve_metrics(
    req: Request<Body>,
    cache: Arc<Mutex<MetricsCache>>,
) -> Result<Response<Body>, Infallible> {
    if req.uri().path() == "/metrics" {
        let cache = cache.lock().await;
        Ok(Response::new(Body::from(cache.data.clone())))
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
        let client = RpcClient::new("https://api.mainnet-beta.solana.com".to_string());

        loop {
            let start = Instant::now();
            let result = timeout(Duration::from_secs_f32(4.5), async {
                fetch_and_calculate_metrics(&client)
            })
            .await;

            // Only lock the cache when updating it
            let new_data = match result {
                Ok(Ok((validator_metrics, active_count))) => {
                    let duration = start.elapsed().as_secs_f64();
                    export_prometheus_metrics(validator_metrics, active_count, 1, duration, 0)
                }
                Ok(Err(_)) => export_prometheus_metrics(vec![], 0, 0, 0.0, 0),  // RPC failure
                Err(_) => export_prometheus_metrics(vec![], 0, 0, 0.0, 1),     // Timeout case
            };

            // Update the cache outside the main loop to minimize the lock time
            {
                let mut cache = cache_clone.lock().await;
                cache.data = new_data;
            }

            // Calculate next delay based on RPC call time + 2 seconds
            let duration = start.elapsed().as_secs_f64();
            sleep(Duration::from_secs_f64(duration + 2.0)).await;
        }
    });

    // Serve metrics on 127.0.0.1:59872 only for `/metrics` route
    let addr = ([127, 0, 0, 1], 59872).into();
    let make_svc = make_service_fn(move |_conn| {
        let cache = Arc::clone(&cache);
        async move { Ok::<_, Infallible>(service_fn(move |req| {
            let cache = Arc::clone(&cache);
            async move { serve_metrics(req, cache).await }  // Pass `req` and `cache`
        })) }
    });
    let server = Server::bind(&addr).serve(make_svc);

    println!("Serving metrics on http://127.0.0.1:59872/metrics");
    server.await?;

    Ok(())
}
