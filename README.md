# Credits Exporter

This branch targets **Alpenglow on testnet only**, using the hardcoded RPC
`https://api.testnet.solana.com`. The `main` branch is unchanged.

Build and run:

```bash
cargo build --release --locked
./target/release/solana-credits-exporter
```

Prometheus endpoint: `http://<host>:59872/metrics` (binds all interfaces; restrict with the host firewall).
Rebuild and replace your existing service binary to apply the change; the service
configuration below is an example, not an automatic installation.

Alpenglow records accrued **reward lamports**, including voter and leader rewards,
in `epochCredits`. They are not the old timely vote credits or a pure measure of
voting performance. Amounts are before reward distribution, not wallet balances.

| Metric | Meaning on this branch |
| --- | --- |
| `solana_validator_alpenglow_rewards_lamports` | Current-epoch Alpenglow voter plus leader reward lamports |
| `solana_validator_credits_so_far` | Compatibility alias for the same reward amount |
| `solana_validator_alpenglow_reward_per_sol` | Reward lamports divided by activated stake in SOL, both from the same `getVoteAccounts` response (atomic; no second call) |
| `solana_validator_activated_stake_lamports` | Activated stake the runtime used for this epoch's rewards |
| `solana_validator_rating_position` | Rank by reward per staked SOL; ties share a rank; zero-stake accounts are unranked |
| `solana_validator_alpenglow_inclusion_relative` | Estimated voter reward per staked SOL relative to the stake-weighted cluster mean; 1.0 = included in reward certificates as often as the average stake. Leader lumps removed (see below) |
| `solana_validator_alpenglow_voter_reward_lamports` | Estimated voter half of the epoch reward: total minus half the cluster-average reward per block times blocks produced |
| `solana_validator_alpenglow_leader_slots`, `_blocks_produced` | Leader slots elapsed and blocks produced this epoch (`getBlockProduction`), apportioned by stake across vote accounts sharing an identity |
| `solana_cluster_alpenglow_blocks_produced`, `_reward_lamports`, `_activated_stake_lamports`, `_reward_per_block_lamports`, `_voter_reward_per_sol` | Cluster sums behind the model (current plus delinquent accounts); absent until the epoch has 1,000 blocks |
| `solana_validator_top_1`, `_top_100`, `_top_200` | Reward per staked SOL at that ranked position |
| `solana_validator_root_distance` | Slots behind the highest current validator's final-certificate root |
| `solana_validator_vote_distance` | Slots behind the highest current validator's reward-account `lastVote` |
| `solana_validator_active` | RPC current account count, including zero-reward accounts |
| `solana_validator_exporter_epoch` | Epoch of the successful sample |

The existing `identity` label remains a **vote-account public key**, preserving
existing queries. Update dashboard titles/units from credits to reward lamports
(or divide by `1e9` for SOL). Reward rank depends on stake and leader rewards;
it must not be presented as the old voting-performance leaderboard.

Leader-lump model: per block with a reward certificate the runtime pays every
included validator `floor(pot * stake / total / 2)` and the block's leader the sum of
the other halves (`runtime/src/block_component_processor/vote_reward.rs`). A
validator's reward is therefore its smooth voter income plus one lump per block it
produced. The exporter estimates the lumps as half of the cluster-average reward per
block times the validator's blocks produced, subtracts them, and divides what is
left by stake. Each sample is one `getBlockProduction` plus one `getVoteAccounts`
call, bracketed by the epoch check.

The exporter detects the migration epoch using `getAgGenesisCert` and
`getEpochSchedule`, separates the migration history at its marker, and uses each
entry's own baseline. It exports zero for accounts without rewards in the current
epoch. It excludes RPC delinquent accounts, as the previous exporter did.

RPC samples have a 4.5-second deadline and a two-second pause between attempts.
Failures are logged and exposed through the existing health metrics; failed
samples omit per-validator data rather than serving it as fresh. Alert on
`solana_validator_exporter_last_rpc_status == 0`. A sample spanning an epoch
boundary is rejected and retried on the next poll. The genesis migration epoch is
cached until restart; restart after a testnet reset.

Validation:

```bash
cargo test --locked
cargo test --locked live_testnet_sample -- --ignored --nocapture
```

The second command contacts public testnet. See [ALPENGLOW.md](ALPENGLOW.md) for the
migration evidence and deployed Agave source references.

```
Alice follows a white rabbit with pink eyes because she saw the rabbit checking a pocket watch.
She chases the rabbit, and it bounds into a rabbit hole. (c) Charles Lutwidge Dodgson
```

```ini
[Unit]
Description=Solana Credits Exporter
After=network.target

[Service]
ExecStart=/home/exporter/bin/solana-credits-exporter

WorkingDirectory=/home/exporter

User=exporter
Group=exporter

Restart=on-failure

RestartSec=5

StandardOutput=journal
StandardError=journal

LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```
