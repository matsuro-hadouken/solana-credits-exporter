use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server, StatusCode};
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_request::RpcRequest,
    rpc_response::{RpcBlockProduction, RpcVoteAccountInfo, RpcVoteAccountStatus},
};
use solana_sdk::commitment_config::CommitmentConfig;
use std::collections::HashMap;

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
/// Blocks the cluster must have produced this epoch before the leader-lump model is
/// exported; below this the per-block average is too fresh to subtract safely.
const MIN_BLOCKS_FOR_MODEL: u64 = 1_000;

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
    /// Leader slots elapsed this epoch, from getBlockProduction. Apportioned by stake
    /// when one identity runs several vote accounts, hence fractional.
    pub leader_slots: f64,
    /// Blocks produced this epoch, same source and apportioning as `leader_slots`.
    pub blocks_produced: f64,
    /// Estimated voter half of `credits_earned`: total minus the leader lumps
    /// (half of the cluster-average reward per block, times blocks produced).
    pub voter_reward_lamports: Option<f64>,
    /// Voter reward per staked SOL relative to the stake-weighted cluster mean.
    /// 1.0 = included in reward certificates as often as the average stake.
    pub inclusion_relative: Option<f64>,
    /// Share (0..1) of the stake of all validators with an `inclusion_relative` value
    /// whose inclusion is strictly below this validator's. 1.0 = best-included.
    pub stake_beaten_share: Option<f64>,
}

/// Fills `stake_beaten_share` from `inclusion_relative` and stake in one sorted pass.
fn fill_stake_beaten_share(validators: &mut [ValidatorMetrics]) {
    let mut scored: Vec<(f64, u64, usize)> = validators
        .iter()
        .enumerate()
        .filter_map(|(i, v)| v.inclusion_relative.map(|x| (x, v.activated_stake, i)))
        .collect();
    let total: u128 = scored.iter().map(|(_, stake, _)| *stake as u128).sum();
    if total == 0 {
        return;
    }
    scored.sort_by(|a, b| a.0.total_cmp(&b.0));
    // `below` = stake of everyone with strictly smaller inclusion; ties share a value.
    let mut below: u128 = 0;
    let mut i = 0;
    while i < scored.len() {
        let mut j = i;
        while j < scored.len() && scored[j].0 == scored[i].0 {
            j += 1;
        }
        let share = below as f64 / total as f64;
        for entry in &scored[i..j] {
            validators[entry.2].stake_beaten_share = Some(share);
            below += entry.1 as u128;
        }
        i = j;
    }
}

/// Cluster-wide sums behind the leader-lump model, from one sample.
///
/// Per block with a reward certificate the runtime pays every included validator
/// `floor(pot * stake / total / 2)` and the block's leader the sum of the other
/// halves (`runtime/src/block_component_processor/vote_reward.rs`, v4.3.0). So a
/// validator's leader income is half of the reward of each block it produced, and
/// the cluster-average reward per block times blocks produced estimates it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ClusterRewardModel {
    /// Blocks produced this epoch by all identities, from getBlockProduction.
    pub blocks_produced: u64,
    /// Current-epoch reward lamports over current plus delinquent accounts.
    pub reward_lamports: u64,
    /// Activated stake over current plus delinquent accounts.
    pub activated_stake: u64,
    /// `reward_lamports / blocks_produced`.
    pub reward_per_block: f64,
    /// Voter half of all rewards per staked SOL: `reward_lamports / 2 / stake_sol`.
    pub voter_reward_per_sol: f64,
}

impl ClusterRewardModel {
    fn new(blocks_produced: u64, reward_lamports: u64, activated_stake: u64) -> Option<Self> {
        if blocks_produced < MIN_BLOCKS_FOR_MODEL || reward_lamports == 0 || activated_stake == 0 {
            return None;
        }
        let reward_per_block = reward_lamports as f64 / blocks_produced as f64;
        let voter_reward_per_sol =
            reward_lamports as f64 / 2.0 * LAMPORTS_PER_SOL as f64 / activated_stake as f64;
        Some(Self {
            blocks_produced,
            reward_lamports,
            activated_stake,
            reward_per_block,
            voter_reward_per_sol,
        })
    }

    /// Estimated leader income for `blocks` blocks produced.
    fn leader_lamports(&self, blocks: f64) -> f64 {
        self.reward_per_block / 2.0 * blocks
    }
}

/// Leader slots and blocks produced per vote account, apportioned by stake across the
/// vote accounts that share an identity (nearly always exactly one).
fn apportion_block_production(
    production: &RpcBlockProduction,
    accounts: &[&RpcVoteAccountInfo],
) -> HashMap<String, (f64, f64)> {
    let mut by_node: HashMap<&str, Vec<&RpcVoteAccountInfo>> = HashMap::new();
    for &account in accounts {
        by_node
            .entry(account.node_pubkey.as_str())
            .or_default()
            .push(account);
    }
    let mut result = HashMap::new();
    for (node, (slots, produced)) in &production.by_identity {
        let Some(vote_accounts) = by_node.get(node.as_str()) else {
            continue;
        };
        let total_stake: u128 = vote_accounts
            .iter()
            .map(|a| a.activated_stake as u128)
            .sum();
        for account in vote_accounts {
            let share = if total_stake == 0 {
                1.0 / vote_accounts.len() as f64
            } else {
                account.activated_stake as f64 / total_stake as f64
            };
            result.insert(
                account.vote_pubkey.clone(),
                (*slots as f64 * share, *produced as f64 * share),
            );
        }
    }
    result
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

/// The last successful sample of an epoch, frozen at the boundary and exported for the
/// whole following epoch as the previous-epoch scoreboard.
#[derive(Debug, Clone, PartialEq)]
struct EpochSnapshot {
    pub epoch: u64,
    /// `(vote pubkey, inclusion_relative, stake_beaten_share)` for every validator that
    /// had an inclusion value.
    pub inclusion_relative: Vec<(String, f64, f64)>,
}

/// Keeps the newest sample of the running epoch and freezes it when the first sample of
/// a later epoch arrives. Nothing is frozen until the exporter has crossed a boundary.
#[derive(Debug, Default)]
struct EpochMemory {
    latest: Option<EpochSnapshot>,
    frozen: Option<EpochSnapshot>,
}

impl EpochMemory {
    fn observe(&mut self, epoch: u64, validators: &[ValidatorMetrics]) {
        match self.latest.as_ref().map(|latest| latest.epoch) {
            Some(latest_epoch) if epoch > latest_epoch => self.frozen = self.latest.take(),
            // An RPC node answering behind the one that produced `latest`; ignore.
            Some(latest_epoch) if epoch < latest_epoch => return,
            _ => {}
        }
        self.latest = Some(EpochSnapshot {
            epoch,
            inclusion_relative: validators
                .iter()
                .filter_map(|v| {
                    v.inclusion_relative.map(|i| {
                        (
                            v.vote_pubkey.clone(),
                            i,
                            v.stake_beaten_share.unwrap_or(0.0),
                        )
                    })
                })
                .collect(),
        });
    }

    fn previous(&self) -> Option<&EpochSnapshot> {
        self.frozen.as_ref()
    }
}

#[derive(Debug, Clone)]
struct MetricsCache {
    pub data: String,
}

impl MetricsCache {
    fn new() -> Self {
        Self {
            data: export_prometheus_metrics(vec![], None, None, 0, 0, 0, 0.0, 0),
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

/// One successful sample of the cluster.
#[derive(Debug, Clone)]
struct Sample {
    pub validators: Vec<ValidatorMetrics>,
    /// RPC current accounts, including zero-reward and skipped ones.
    pub active_count: usize,
    /// Current accounts dropped from this sample because their reward history was
    /// inconsistent (total below baseline). Never fatal for the other accounts.
    pub skipped_accounts: usize,
    pub model: Option<ClusterRewardModel>,
}

fn calculate_metrics(
    accounts: RpcVoteAccountStatus,
    production: Option<&RpcBlockProduction>,
    epoch: u64,
    migration_epoch: u64,
) -> Result<Sample, FetchError> {
    if epoch < migration_epoch {
        return Err("RPC epoch precedes Alpenglow migration".into());
    }
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

    // Leader-lump model inputs. Cluster sums span current AND delinquent accounts:
    // both hold epoch stake and both may have produced blocks this epoch. A delinquent
    // account's history is taken as-is (zero on any inconsistency), never fatal.
    let model = production.and_then(|production| {
        let blocks: u64 = production
            .by_identity
            .values()
            .map(|(_, produced)| *produced as u64)
            .sum();
        let reward: u64 = accounts
            .current
            .iter()
            .chain(accounts.delinquent.iter())
            .map(|a| epoch_rewards(&a.epoch_credits, epoch, migration_epoch).unwrap_or(0))
            .fold(0u64, u64::saturating_add);
        let stake: u64 = accounts
            .current
            .iter()
            .chain(accounts.delinquent.iter())
            .map(|a| a.activated_stake)
            .fold(0u64, u64::saturating_add);
        ClusterRewardModel::new(blocks, reward, stake)
    });
    let apportioned = production.map(|production| {
        let all: Vec<&RpcVoteAccountInfo> = accounts
            .current
            .iter()
            .chain(accounts.delinquent.iter())
            .collect();
        apportion_block_production(production, &all)
    });

    let mut validators = Vec::with_capacity(active_count);
    let mut skipped_accounts = 0;
    for account in accounts.current {
        let credits_earned = match epoch_rewards(&account.epoch_credits, epoch, migration_epoch) {
            Ok(credits) => credits,
            Err(error) => {
                // One account's bad history must not blank the whole cluster.
                eprintln!(
                    "Skipping vote account {} this sample: {error}",
                    account.vote_pubkey
                );
                skipped_accounts += 1;
                continue;
            }
        };
        let (leader_slots, blocks_produced) = apportioned
            .as_ref()
            .and_then(|m| m.get(&account.vote_pubkey).copied())
            .unwrap_or((0.0, 0.0));
        let voter_reward_lamports = model
            .map(|model| (credits_earned as f64 - model.leader_lamports(blocks_produced)).max(0.0));
        let inclusion_relative = match (voter_reward_lamports, model) {
            (Some(voter), Some(model)) if account.activated_stake > 0 => {
                let per_sol = voter * LAMPORTS_PER_SOL as f64 / account.activated_stake as f64;
                Some(per_sol / model.voter_reward_per_sol)
            }
            _ => None,
        };
        validators.push(ValidatorMetrics {
            vote_pubkey: account.vote_pubkey,
            root_distance: top_root.saturating_sub(account.root_slot),
            vote_distance: top_vote.saturating_sub(account.last_vote),
            credits_earned,
            activated_stake: account.activated_stake,
            rank: 0,
            leader_slots,
            blocks_produced,
            voter_reward_lamports,
            inclusion_relative,
            stake_beaten_share: None,
        });
    }
    fill_stake_beaten_share(&mut validators);
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
    Ok(Sample {
        validators,
        active_count,
        skipped_accounts,
        model,
    })
}

async fn fetch_and_calculate_metrics(
    client: &RpcClient,
    migration_epoch: &mut Option<u64>,
) -> Result<(Sample, u64), FetchError> {
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
    // Block production first, then vote accounts: a leader block landing between the
    // two calls then shows in credits but not yet in blocks, which only understates
    // the leader lump for one sample. The epoch check below brackets both.
    let production = client.get_block_production().await?.value;
    let accounts = client.get_vote_accounts().await?;
    let after = client.get_epoch_info().await?;
    if before.epoch != after.epoch {
        return Err("epoch changed during RPC sample; retrying next poll".into());
    }
    let sample = calculate_metrics(accounts, Some(&production), after.epoch, migration)?;
    Ok((sample, after.epoch))
}

fn export_prometheus_metrics(
    validators: Vec<ValidatorMetrics>,
    model: Option<ClusterRewardModel>,
    previous: Option<&EpochSnapshot>,
    active_count: usize,
    skipped_accounts: usize,
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

    // Leader-lump model, per validator: leader slots, blocks, voter-only reward, inclusion
    output.push_str("# HELP solana_validator_alpenglow_leader_slots Leader slots elapsed this epoch (getBlockProduction), apportioned by stake across vote accounts sharing an identity\n");
    output.push_str("# TYPE solana_validator_alpenglow_leader_slots gauge\n");
    for validator in &validators {
        output.push_str(&format!(
            "solana_validator_alpenglow_leader_slots{{identity=\"{}\"}} {}\n",
            validator.vote_pubkey, validator.leader_slots
        ));
    }

    output.push_str("# HELP solana_validator_alpenglow_blocks_produced Blocks produced this epoch (getBlockProduction), apportioned by stake across vote accounts sharing an identity\n");
    output.push_str("# TYPE solana_validator_alpenglow_blocks_produced gauge\n");
    for validator in &validators {
        output.push_str(&format!(
            "solana_validator_alpenglow_blocks_produced{{identity=\"{}\"}} {}\n",
            validator.vote_pubkey, validator.blocks_produced
        ));
    }

    output.push_str("# HELP solana_validator_alpenglow_voter_reward_lamports Estimated voter half of current-epoch reward: total minus half the cluster-average reward per block times blocks produced\n");
    output.push_str("# TYPE solana_validator_alpenglow_voter_reward_lamports gauge\n");
    for validator in &validators {
        if let Some(voter) = validator.voter_reward_lamports {
            output.push_str(&format!(
                "solana_validator_alpenglow_voter_reward_lamports{{identity=\"{}\"}} {}\n",
                validator.vote_pubkey, voter
            ));
        }
    }

    output.push_str("# HELP solana_validator_alpenglow_inclusion_relative Estimated voter reward per staked SOL relative to the stake-weighted cluster mean; 1.0 = included in reward certificates as often as the average stake\n");
    output.push_str("# TYPE solana_validator_alpenglow_inclusion_relative gauge\n");
    for validator in &validators {
        if let Some(inclusion) = validator.inclusion_relative {
            output.push_str(&format!(
                "solana_validator_alpenglow_inclusion_relative{{identity=\"{}\"}} {}\n",
                validator.vote_pubkey, inclusion
            ));
        }
    }

    output.push_str("# HELP solana_validator_alpenglow_stake_beaten_share Share (0-1) of the stake of all validators with an inclusion value whose inclusion is strictly below this validator's; 1 = best-included in the cluster\n");
    output.push_str("# TYPE solana_validator_alpenglow_stake_beaten_share gauge\n");
    for validator in &validators {
        if let Some(share) = validator.stake_beaten_share {
            output.push_str(&format!(
                "solana_validator_alpenglow_stake_beaten_share{{identity=\"{}\"}} {}\n",
                validator.vote_pubkey, share
            ));
        }
    }

    // Previous-epoch scoreboard: the last sample before the boundary, frozen
    output.push_str("# HELP solana_validator_exporter_previous_epoch Epoch described by the *_previous_epoch gauges (its last sample before the boundary); absent until the exporter has crossed an epoch boundary\n");
    output.push_str("# TYPE solana_validator_exporter_previous_epoch gauge\n");
    if let Some(previous) = previous {
        output.push_str(&format!(
            "solana_validator_exporter_previous_epoch {}\n",
            previous.epoch
        ));
    }
    output.push_str("# HELP solana_validator_alpenglow_inclusion_relative_previous_epoch Final solana_validator_alpenglow_inclusion_relative of the previous epoch, frozen at its last sample before the boundary\n");
    output.push_str("# TYPE solana_validator_alpenglow_inclusion_relative_previous_epoch gauge\n");
    if let Some(previous) = previous {
        for (vote_pubkey, inclusion, _) in &previous.inclusion_relative {
            output.push_str(&format!(
                "solana_validator_alpenglow_inclusion_relative_previous_epoch{{identity=\"{vote_pubkey}\"}} {inclusion}\n"
            ));
        }
    }
    output.push_str("# HELP solana_validator_alpenglow_stake_beaten_share_previous_epoch Final solana_validator_alpenglow_stake_beaten_share of the previous epoch, frozen at its last sample before the boundary\n");
    output.push_str("# TYPE solana_validator_alpenglow_stake_beaten_share_previous_epoch gauge\n");
    if let Some(previous) = previous {
        for (vote_pubkey, _, share) in &previous.inclusion_relative {
            output.push_str(&format!(
                "solana_validator_alpenglow_stake_beaten_share_previous_epoch{{identity=\"{vote_pubkey}\"}} {share}\n"
            ));
        }
    }

    // Leader-lump model, cluster level (absent until the epoch has enough blocks)
    output.push_str("# HELP solana_cluster_alpenglow_blocks_produced Blocks produced this epoch by all identities (getBlockProduction)\n");
    output.push_str("# TYPE solana_cluster_alpenglow_blocks_produced gauge\n");
    output.push_str("# HELP solana_cluster_alpenglow_reward_lamports Current-epoch reward lamports summed over current and delinquent vote accounts\n");
    output.push_str("# TYPE solana_cluster_alpenglow_reward_lamports gauge\n");
    output.push_str("# HELP solana_cluster_alpenglow_activated_stake_lamports Activated stake summed over current and delinquent vote accounts\n");
    output.push_str("# TYPE solana_cluster_alpenglow_activated_stake_lamports gauge\n");
    output.push_str("# HELP solana_cluster_alpenglow_reward_per_block_lamports Cluster reward lamports divided by blocks produced this epoch; half of it is one block's leader income\n");
    output.push_str("# TYPE solana_cluster_alpenglow_reward_per_block_lamports gauge\n");
    output.push_str("# HELP solana_cluster_alpenglow_voter_reward_per_sol Voter half of all cluster rewards per staked SOL; denominator of solana_validator_alpenglow_inclusion_relative\n");
    output.push_str("# TYPE solana_cluster_alpenglow_voter_reward_per_sol gauge\n");
    if let Some(model) = model {
        output.push_str(&format!(
            "solana_cluster_alpenglow_blocks_produced {}\n",
            model.blocks_produced
        ));
        output.push_str(&format!(
            "solana_cluster_alpenglow_reward_lamports {}\n",
            model.reward_lamports
        ));
        output.push_str(&format!(
            "solana_cluster_alpenglow_activated_stake_lamports {}\n",
            model.activated_stake
        ));
        output.push_str(&format!(
            "solana_cluster_alpenglow_reward_per_block_lamports {}\n",
            model.reward_per_block
        ));
        output.push_str(&format!(
            "solana_cluster_alpenglow_voter_reward_per_sol {}\n",
            model.voter_reward_per_sol
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

    // Accounts dropped from this sample for an inconsistent reward history
    output.push_str("# HELP solana_validator_exporter_skipped_accounts RPC current accounts dropped from this sample because their reward history was inconsistent\n");
    output.push_str("# TYPE solana_validator_exporter_skipped_accounts gauge\n");
    output.push_str(&format!(
        "solana_validator_exporter_skipped_accounts {}\n",
        skipped_accounts
    ));

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
        let mut epoch_memory = EpochMemory::default();
        // Log failures on change only: first occurrence, a different message, recovery.
        let mut last_error: Option<String> = None;

        loop {
            let start = Instant::now();
            let result = timeout(Duration::from_secs_f32(4.5), async {
                fetch_and_calculate_metrics(&client, &mut migration_epoch).await
            })
            .await;

            // Only lock the cache when updating it
            let new_data = match result {
                Ok(Ok((sample, epoch))) => {
                    let duration = start.elapsed().as_secs_f64();
                    if last_error.take().is_some() {
                        eprintln!("RPC sampling recovered (epoch {epoch})");
                    }
                    epoch_memory.observe(epoch, &sample.validators);
                    let mut data = export_prometheus_metrics(
                        sample.validators,
                        sample.model,
                        epoch_memory.previous(),
                        sample.active_count,
                        sample.skipped_accounts,
                        1,
                        duration,
                        0,
                    );
                    data.push_str(&format!("# HELP solana_validator_exporter_epoch Epoch of this successful sample\n# TYPE solana_validator_exporter_epoch gauge\nsolana_validator_exporter_epoch {}\n", epoch));
                    data
                }
                Ok(Err(error)) => {
                    let message = format!("RPC sample failed: {error}");
                    if last_error.as_deref() != Some(message.as_str()) {
                        eprintln!("{message}");
                        last_error = Some(message);
                    }
                    export_prometheus_metrics(
                        vec![],
                        None,
                        epoch_memory.previous(),
                        0,
                        0,
                        0,
                        start.elapsed().as_secs_f64(),
                        0,
                    )
                }
                Err(_) => {
                    let message = "RPC sample timed out after 4.5 seconds".to_string();
                    if last_error.as_deref() != Some(message.as_str()) {
                        eprintln!("{message}");
                        last_error = Some(message);
                    }
                    export_prometheus_metrics(
                        vec![],
                        None,
                        epoch_memory.previous(),
                        0,
                        0,
                        0,
                        start.elapsed().as_secs_f64(),
                        1,
                    )
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
        let Sample {
            validators,
            active_count: count,
            skipped_accounts,
            model,
        } = calculate_metrics(accounts, None, 1042, 1042).unwrap();
        assert_eq!((count, skipped_accounts), (2, 0));
        assert_eq!(model, None);
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
        let text = export_prometheus_metrics(validators, model, None, count, 0, 1, 0.2, 0);
        assert!(text.contains("solana_validator_active 2\n"));
        assert!(text.contains("solana_validator_top_1 16600.029678075633\n"));
        // Without block production the model gauges keep HELP/TYPE but carry no samples.
        assert!(text.contains("# TYPE solana_cluster_alpenglow_blocks_produced gauge\n"));
        assert!(!text
            .lines()
            .any(|l| l.starts_with("solana_cluster_alpenglow_") && !l.starts_with('#')));
        assert!(!text
            .lines()
            .any(|l| l.starts_with("solana_validator_alpenglow_inclusion_relative{")));
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
        let Sample {
            validators,
            active_count: count,
            model,
            ..
        } = calculate_metrics(accounts, None, 1042, 1042).unwrap();
        assert_eq!(count, 2);
        // The staked account ranks first; the zero-stake one is last with rank 0.
        assert_eq!(validators[0].rank, 1);
        assert_eq!(validators[0].activated_stake, 296_603_043_095_931);
        assert_eq!(validators[1].rank, 0);
        assert_eq!(validators[1].activated_stake, 0);
        assert_eq!(validators[1].reward_per_sol(), None);
        let text = export_prometheus_metrics(validators, model, None, count, 0, 1, 0.2, 0);
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
    fn leader_lump_model_matches_hand_computation() {
        // Fixture: CiHv (Txh6…, 347,671 SOL, 5.44e9 lamports), 9LjQ (vyRa…, 296,603 SOL,
        // 4.92e9), delinquent pitR (3muw…, 319,591 SOL, 0). Blocks 20 / 16 / 0.
        // MIN_BLOCKS_FOR_MODEL is scaled by 1000 unknown-identity blocks so the gate opens.
        let accounts: RpcVoteAccountStatus =
            serde_json::from_str(include_str!("../tests/fixtures/testnet-migration.json")).unwrap();
        let production: RpcBlockProduction = serde_json::from_value(serde_json::json!({
            "byIdentity": {
                "Txh6mKsvA8rwesigHzALTiFggyrkGDVtMP2vicuepAy": [20, 20],
                "vyRa8J7ULHfUAdnkTHP3YGhcLWaLURXLmD7CiZkMzWg": [16, 16],
                "3muw1JVkkBJoSA3F4qScCvLgTVCpP3wHEXjCtkiq277w": [4, 0]
            },
            "range": {"firstSlot": 444620256, "lastSlot": 444630541}
        }))
        .unwrap();
        // Gate: 36 blocks is below MIN_BLOCKS_FOR_MODEL -> no model, but counts still flow.
        let Sample {
            validators, model, ..
        } = calculate_metrics(accounts.clone(), Some(&production), 1042, 1042).unwrap();
        assert_eq!(model, None);
        assert!(validators
            .iter()
            .all(|v| v.inclusion_relative.is_none() && v.voter_reward_lamports.is_none()));
        let cihv = validators
            .iter()
            .find(|v| v.vote_pubkey.starts_with("CiHv"))
            .unwrap();
        assert_eq!((cihv.leader_slots, cihv.blocks_produced), (20.0, 20.0));

        // Open the gate with an identity that has no vote account: its blocks count in the
        // cluster total but map to nobody. Hand-computed with the same arithmetic:
        //   B = 1036, sumC = 10_364_546_738, sumS = 963_865_814_675_954
        //   reward_per_block = sumC / B
        //   voter(v) = C(v) - reward_per_block / 2 * blocks(v)
        //   cluster_voter_per_sol = sumC / 2 * 1e9 / sumS
        //   inclusion(v) = (voter(v) * 1e9 / S(v)) / cluster_voter_per_sol
        let mut production = production;
        production
            .by_identity
            .insert("unknownIdentity".to_string(), (1000, 1000));
        let Sample {
            validators, model, ..
        } = calculate_metrics(accounts, Some(&production), 1042, 1042).unwrap();
        let model = model.unwrap();
        assert_eq!(model.blocks_produced, 1036);
        assert_eq!(model.reward_lamports, 10_364_546_738);
        assert_eq!(model.activated_stake, 963_865_814_675_954);
        let close = |a: f64, b: f64| (a - b).abs() <= 1e-9 * b.abs().max(1.0);
        assert!(close(model.reward_per_block, 10_364_546_738.0 / 1036.0));
        assert!(close(
            model.voter_reward_per_sol,
            10_364_546_738.0 / 2.0 * 1e9 / 963_865_814_675_954.0
        ));
        let cihv = validators
            .iter()
            .find(|v| v.vote_pubkey.starts_with("CiHv"))
            .unwrap();
        let ljq = validators
            .iter()
            .find(|v| v.vote_pubkey.starts_with("9LjQ"))
            .unwrap();
        let voter_cihv = 5_440_927_420.0 - model.reward_per_block / 2.0 * 20.0;
        let voter_ljq = 4_923_619_318.0 - model.reward_per_block / 2.0 * 16.0;
        assert!(close(cihv.voter_reward_lamports.unwrap(), voter_cihv));
        assert!(close(ljq.voter_reward_lamports.unwrap(), voter_ljq));
        assert!(close(
            cihv.inclusion_relative.unwrap(),
            (voter_cihv * 1e9 / 347_671_486_449_531.0) / model.voter_reward_per_sol
        ));
        assert!(close(
            ljq.inclusion_relative.unwrap(),
            (voter_ljq * 1e9 / 296_603_043_095_931.0) / model.voter_reward_per_sol
        ));
        // The delinquent third of the stake earned nothing, so both active accounts sit
        // above the stake-weighted mean.
        assert!(cihv.inclusion_relative.unwrap() > 1.0);
        assert!(ljq.inclusion_relative.unwrap() > 1.0);
        // 9LjQ is included more often than CiHv, so it beats CiHv's stake and CiHv beats
        // nobody; the delinquent account has no inclusion value and is outside the base.
        assert!(ljq.inclusion_relative.unwrap() > cihv.inclusion_relative.unwrap());
        assert_eq!(cihv.stake_beaten_share, Some(0.0));
        assert!(close(
            ljq.stake_beaten_share.unwrap(),
            347_671_486_449_531.0 / (347_671_486_449_531.0 + 296_603_043_095_931.0)
        ));

        let count = validators.len();
        let text = export_prometheus_metrics(validators, Some(model), None, count, 0, 1, 0.2, 0);
        assert!(text.contains("solana_cluster_alpenglow_blocks_produced 1036\n"));
        assert!(text.contains("solana_cluster_alpenglow_reward_lamports 10364546738\n"));
        assert!(text.contains(
            "solana_validator_alpenglow_blocks_produced{identity=\"CiHvrTZsFoPpziogcnfMWfWnnZiNvxjSz3et5PehMA5D\"} 20\n"
        ));
        assert_eq!(
            text.lines()
                .filter(|l| l.starts_with("solana_validator_alpenglow_inclusion_relative{"))
                .count(),
            2
        );
        assert!(text.contains(
            "solana_validator_alpenglow_stake_beaten_share{identity=\"CiHvrTZsFoPpziogcnfMWfWnnZiNvxjSz3et5PehMA5D\"} 0\n"
        ));
        // Every gauge name appears in exactly one HELP and one TYPE line.
        let mut names: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("# TYPE "))
            .filter_map(|l| l.split(' ').next())
            .collect();
        let unique = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(unique, names.len());
        // No NaN or inf anywhere in the exposition.
        assert!(!text.contains("NaN") && !text.contains("inf"));
    }

    #[test]
    fn leader_lump_model_apportions_shared_identity_by_stake() {
        let mut json: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/testnet-migration.json")).unwrap();
        // Make both current accounts share one identity: 347,671 vs 296,603 SOL.
        json["current"][1]["nodePubkey"] = json["current"][0]["nodePubkey"].clone();
        let accounts: RpcVoteAccountStatus = serde_json::from_value(json).unwrap();
        let production: RpcBlockProduction = serde_json::from_value(serde_json::json!({
            "byIdentity": {"Txh6mKsvA8rwesigHzALTiFggyrkGDVtMP2vicuepAy": [100, 90]},
            "range": {"firstSlot": 0, "lastSlot": 0}
        }))
        .unwrap();
        let Sample { validators, .. } =
            calculate_metrics(accounts, Some(&production), 1042, 1042).unwrap();
        let total = 347_671_486_449_531.0 + 296_603_043_095_931.0;
        let slots: f64 = validators.iter().map(|v| v.leader_slots).sum();
        let blocks: f64 = validators.iter().map(|v| v.blocks_produced).sum();
        assert!((slots - 100.0).abs() < 1e-9 && (blocks - 90.0).abs() < 1e-9);
        let cihv = validators
            .iter()
            .find(|v| v.vote_pubkey.starts_with("CiHv"))
            .unwrap();
        assert!((cihv.blocks_produced - 90.0 * 347_671_486_449_531.0 / total).abs() < 1e-9);
    }

    #[test]
    fn epoch_memory_freezes_last_sample_before_boundary() {
        let v = |key: &str, inclusion: Option<f64>| ValidatorMetrics {
            vote_pubkey: key.to_string(),
            root_distance: 0,
            vote_distance: 0,
            credits_earned: 0,
            activated_stake: 1,
            rank: 0,
            leader_slots: 0.0,
            blocks_produced: 0.0,
            voter_reward_lamports: None,
            inclusion_relative: inclusion,
            stake_beaten_share: inclusion.map(|_| 0.25),
        };
        let mut memory = EpochMemory::default();
        memory.observe(1042, &[v("A", Some(0.990)), v("B", None)]);
        memory.observe(1042, &[v("A", Some(0.995)), v("B", Some(1.001))]);
        assert_eq!(memory.previous(), None);
        // First sample of the next epoch freezes the LAST sample of the old one.
        memory.observe(1043, &[v("A", Some(0.5))]);
        let frozen = memory.previous().unwrap();
        assert_eq!(frozen.epoch, 1042);
        assert_eq!(
            frozen.inclusion_relative,
            vec![
                ("A".to_string(), 0.995, 0.25),
                ("B".to_string(), 1.001, 0.25)
            ]
        );
        // A stale sample from a lagging RPC never overwrites or freezes anything.
        memory.observe(1042, &[v("A", Some(0.1))]);
        assert_eq!(memory.previous().unwrap().epoch, 1042);
        memory.observe(1043, &[v("A", Some(0.7))]);
        memory.observe(1044, &[]);
        let frozen = memory.previous().unwrap();
        assert_eq!(frozen.epoch, 1043);
        assert_eq!(
            frozen.inclusion_relative,
            vec![("A".to_string(), 0.7, 0.25)]
        );

        let text = export_prometheus_metrics(vec![], None, memory.previous(), 0, 0, 1, 0.1, 0);
        assert!(text.contains("solana_validator_exporter_previous_epoch 1043\n"));
        assert!(text.contains(
            "solana_validator_alpenglow_inclusion_relative_previous_epoch{identity=\"A\"} 0.7\n"
        ));
        assert!(text.contains(
            "solana_validator_alpenglow_stake_beaten_share_previous_epoch{identity=\"A\"} 0.25\n"
        ));
        let text = export_prometheus_metrics(vec![], None, None, 0, 0, 1, 0.1, 0);
        assert!(text.contains("# TYPE solana_validator_exporter_previous_epoch gauge\n"));
        assert!(!text
            .lines()
            .any(|l| l.starts_with("solana_validator_exporter_previous_epoch ")));
    }

    #[test]
    fn stake_beaten_share_is_strict_and_tie_aware() {
        let v = |key: &str, stake: u64, inclusion: Option<f64>| ValidatorMetrics {
            vote_pubkey: key.to_string(),
            root_distance: 0,
            vote_distance: 0,
            credits_earned: 0,
            activated_stake: stake,
            rank: 0,
            leader_slots: 0.0,
            blocks_produced: 0.0,
            voter_reward_lamports: None,
            inclusion_relative: inclusion,
            stake_beaten_share: None,
        };
        // A: 1.0 (stake 10), B: 1.1 (stake 30), C: 1.1 (stake 20), D: 0.9 (stake 40),
        // E: no inclusion (stake 1000, outside the base).
        let mut validators = vec![
            v("A", 10, Some(1.0)),
            v("B", 30, Some(1.1)),
            v("C", 20, Some(1.1)),
            v("D", 40, Some(0.9)),
            v("E", 1000, None),
        ];
        fill_stake_beaten_share(&mut validators);
        let share = |k: &str| {
            validators
                .iter()
                .find(|v| v.vote_pubkey == k)
                .unwrap()
                .stake_beaten_share
        };
        assert_eq!(share("D"), Some(0.0));
        assert_eq!(share("A"), Some(0.4)); // beats D: 40 of 100
        assert_eq!(share("B"), Some(0.5)); // beats D + A: 50 of 100, tie with C
        assert_eq!(share("C"), Some(0.5));
        assert_eq!(share("E"), None);
    }

    #[test]
    fn inconsistent_account_is_skipped_not_fatal() {
        let mut json: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/testnet-migration.json")).unwrap();
        // Corrupt CiHv's current-epoch row: total below baseline.
        json["current"][0]["epochCredits"][4] = serde_json::json!([1042, 1, 2273751687_u64]);
        let accounts: RpcVoteAccountStatus = serde_json::from_value(json).unwrap();
        let Sample {
            validators,
            active_count,
            skipped_accounts,
            ..
        } = calculate_metrics(accounts, None, 1042, 1042).unwrap();
        assert_eq!((active_count, skipped_accounts), (2, 1));
        assert_eq!(validators.len(), 1);
        assert!(validators[0].vote_pubkey.starts_with("9LjQ"));
        assert_eq!(validators[0].rank, 1);
        let text = export_prometheus_metrics(
            validators,
            None,
            None,
            active_count,
            skipped_accounts,
            1,
            0.2,
            0,
        );
        assert!(text.contains("solana_validator_active 2\n"));
        assert!(text.contains("solana_validator_exporter_skipped_accounts 1\n"));
        // A sample before the migration epoch is still rejected as a whole.
        let accounts: RpcVoteAccountStatus =
            serde_json::from_str(include_str!("../tests/fixtures/testnet-migration.json")).unwrap();
        assert!(calculate_metrics(accounts, None, 1041, 1042).is_err());
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
            leader_slots: 0.0,
            blocks_produced: 0.0,
            voter_reward_lamports: None,
            inclusion_relative: None,
            stake_beaten_share: None,
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
        let Sample {
            validators,
            active_count: count,
            ..
        } = calculate_metrics(accounts, None, 1043, 1042).unwrap();
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
        let production = serde_json::json!({
            "context": {"slot": 444620256},
            "value": {"byIdentity": {}, "range": {"firstSlot": 0, "lastSlot": 0}}
        });
        let (client, server) = mock_rpc(
            vec![epoch(1042), production, accounts, epoch(1043)],
            Duration::ZERO,
        )
        .await;
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
        let (sample, epoch) = timeout(
            Duration::from_secs_f32(4.5),
            fetch_and_calculate_metrics(&client, &mut None),
        )
        .await
        .unwrap()
        .unwrap();
        let Sample {
            validators,
            active_count: count,
            skipped_accounts,
            model,
        } = sample;
        assert!(count > 0);
        assert_eq!(count, validators.len() + skipped_accounts);
        assert!(validators.iter().any(|v| v.credits_earned > 0));
        let output =
            export_prometheus_metrics(validators, model, None, count, skipped_accounts, 1, 0.0, 0);
        assert!(output.contains("solana_validator_top_1 "));
        println!("Testnet epoch {epoch}: exported {count} validators with Alpenglow rewards");
    }
}
