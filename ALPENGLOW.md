# Alpenglow investigation — 2026-09-24

The `alpenglow` branch uses `https://api.testnet.solana.com`. The observations
below come from that public endpoint. Mainnet behavior remains on `main`.

## Live evidence

Public testnet responded successfully to `getHealth`, `getVersion`,
`getEpochInfo`, and `getVoteAccounts` around 18:24–18:28 UTC:

- Health: `ok`; Agave version: `4.3.0`; feature set: `3383571666`.
- Epoch: `1042`; absolute slot: `444630541`; slot index: `10285`.
- Vote accounts: 435 current, 56 delinquent.
- All 435 current accounts had `[18446744073709551615,
  18446744073709551615, 18446744073709551615]` as their penultimate
  `epochCredits` entry. These are exact unsigned 64-bit maximum values.
- Replaying the exporter's existing selection condition against this response
  selected **zero** current validators.

Example history from vote account
`CiHvrTZsFoPpziogcnfMWfWnnZiNvxjSz3et5PehMA5D`:

```text
[1040, 2266983401, 2260203642]
[1041, 2273687040, 2266983401]
[1042, 2273751687, 2273687040]
[18446744073709551615, 18446744073709551615, 18446744073709551615]
[1042, 7714679107, 2273751687]
```

The exporter compares the second field of the last row with the second field
of the preceding row. `7714679107 > u64::MAX` is false, so the account disappears.
The successful RPC response still produces `last_rpc_status = 1`, while active
count is zero and all validator and top-N samples disappear.

The latest row's own delta is `7714679107 - 2273751687 = 5440927420`.
This demonstrates how to read that segment; it does **not** establish that the
result is comparable to legacy timely vote credits. The two epoch-1042 rows
also show why blindly summing everything with the same epoch is unsafe.

## Meaning changes

Verified against Agave **v4.3.0**, commit
`825efd18292aff6ffcf9daa0f7612f21b3531a72`:

- [Reward-account updates](https://github.com/anza-xyz/agave/blob/v4.3.0/runtime/src/block_component_processor/vote_reward.rs):
  `increment_credits` inserts the migration marker and starts a separate entry
  whose baseline is the last Tower total. Voter rewards and additional leader
  rewards both increment this history. `maybe_update_votes` advances `lastVote`
  from reward certificates; final certificates advance the root.
- [Reward distribution points](https://github.com/anza-xyz/agave/blob/v4.3.0/runtime/src/inflation_rewards/points.rs):
  Alpenglow interprets the delta as reward lamports and allocates it across stake.
  It checks the rewarded epoch rather than reusing an old epoch's credits.

These facts support exporting accrued reward amounts, but not reconstructing the
old voting-performance score from `getVoteAccounts`. Leader and voter components
are combined. Even stake-normalizing the total does not isolate voting quality.

Current testnet `lastVote` values were close to the cluster slot. The sampled
RPC classified hundreds of validators as current; the current/delinquent split
was not the cause of the empty export.

## Implemented fix

- Testnet stays hardcoded; `main` is untouched.
- Genesis certificate plus epoch schedule identify the activation epoch. Missing
  certificates fail the poll explicitly instead of mislabeling Tower points.
- Only the Alpenglow segment is counted during migration, even when the epoch
  appears twice. Each entry uses its own baseline. Later epochs work even after
  the marker falls out of the bounded RPC history.
- Current accounts remain visible with zero rewards when the epoch has no entry.
- Reward lamports have an explicit metric; existing credits and ranking metrics
  remain documented compatibility aliases/rankings of raw rewards.
- Async RPC makes the sample deadline effective. Errors are logged, failure
  duration is measured, and startup exposes an unsuccessful health sample.
- Epoch information brackets each vote-account request to reject rollover races.
- Existing vote-account label values and the local metrics port are preserved.

## Validation

The fixture in `tests/fixtures/testnet-migration.json` is a small
subset of the actual initial public-testnet response: two current accounts and
one delinquent account. Regression tests cover the observed marker, duplicated
activation epoch, single-entry histories, missing rewards, rollover, invalid
baselines, tied rankings, HTTP output, missing genesis certificates, and stalled
RPC cancellation. The opt-in live test also exercises RPC decoding with the
existing Solana client dependency and confirms nonempty reward exports.

Validation completed on 2026-09-24: eight regression tests passed, the opt-in
live test passed, formatting and Clippy passed, and the release build succeeded.
The release binary's `/metrics` endpoint returned HTTP 200 with 434 reward series
in epoch 1042 and RPC status 1. Legacy credit aliases matched every reward value;
an unknown route returned HTTP 404. The temporary test process was stopped.

This is a reward tracker under Alpenglow. A separate participation leaderboard
would require additional certificate-level accounting to distinguish voting
participation from stake and leader-reward effects.
