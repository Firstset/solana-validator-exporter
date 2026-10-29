# Solana Validator Exporter

This is a Prometheus exporter for Solana validators that supports monitoring multiple validators across different networks (mainnet, testnet, devnet).

## Features

- **Multi-Network Support**: Monitor validators on mainnet, testnet, and devnet simultaneously
- **Multi-Validator Support**: Monitor multiple validators per network
- **Comprehensive Metrics**: Track validator performance, financial metrics, and network statistics
- **Network Labels**: All metrics include network and vote account labels for easy identification

## Setup

1. **Create a configuration file.**

    Copy the `config.example.yaml` to `config.yaml` and update the values:

    ```bash
    cp config.example.yaml config.yaml
    ```

    Then, edit `config.yaml`:

    ### Global Configuration

    - `port`: The port the exporter will listen on (e.g., `9090`).

    ### Networks

    Each entry under the `networks` map defines a network to monitor. Example:

    ```yaml
    networks:
      solana_mainnet:
        rpc_url: "https://api.mainnet-beta.solana.com"
        validators:
          - vote_address: "YourVoteAccount"
            identity_address: "YourIdentityAccount"
        labels:
          stage: mainnet
          environment: production
    ```

    For every network provide:
    - `rpc_url`: The RPC endpoint for the network.
    - `validators`: A list of validator objects with `vote_address` and `identity_address` keys.
    - `labels` _(optional)_: Additional key/value pairs that will be attached to every metric for that network.
    - `delinquency_seconds` _(optional, default `60`)_: A validator whose last vote is older than this is reported as delinquent (`solana_voting_status` = 0). It is converted to slots every loop iteration with the measured slot time (`max(32, round(seconds * 1000 / slot_time_ms))`), so it is 300 slots at 200 ms slots and 150 slots at 400 ms slots.
    - `reward_window_seconds` _(optional, default `900`)_: Sliding window over which `solana_epoch_credits_rate_ratio` is computed.
    - `recent_window_seconds` _(optional, default `600`)_: How far back (in wall-clock time) leader slots are fetched for `solana_epoch_block_rewards`.

    The exporter automatically adds `network` and `vote_account` labels. If you supply a `network` entry inside `labels`, it overrides the default value derived from the config key. Note that you can also set labels for each validator, which overrides the values configured at the network level.

## Running the Exporter

You have two options to run the exporter: direct build or Docker container.

### Option 1: Direct Build

1. **Install Rust** (if not already installed):

   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

2. **Build the exporter**:

   ```bash
   cargo build --release
   ```

3. **Run the exporter**:

   ```bash
   ./target/release/solana-validator-exporter --config-file config.yaml
   ```

### Option 2: Docker Container

1. **Build the Docker image**:

   ```bash
   docker build -t solana-validator-exporter -f docker/Dockerfile .
   ```

2. **Run the container**:

   ```bash
   docker run -d \
     --name solana-validator-exporter \
     -p 9090:9090 \
     -v $(pwd)/config.yaml:/home/exporter/config.yaml \
     solana-validator-exporter --config-file /home/exporter/config.yaml
   ```

## Verifying the Exporter

After running either method, you can verify the exporter is working by accessing the metrics endpoint:

```bash
curl http://localhost:9090/metrics
```

The exporter will expose metrics on the port specified in your configuration file (e.g., `http://localhost:9090/metrics`).

## Metrics

All metrics are gauges and carry the labels described in the next section.

### Cluster and epoch

| Metric | Description |
| --- | --- |
| `solana_slot` | Current slot of the cluster, as seen by the configured RPC node. |
| `solana_epoch` | Current epoch. |
| `solana_epoch_progress` | Progress through the epoch, in basis points (0-10000). |
| `solana_slot_time_ms` | Average slot time measured from the last 60 performance samples. Every slot/time conversion in the exporter uses this value; when the samples contain no slots it falls back to 200 ms and logs a warning. |
| `solana_usd_price` | SOL/USD price from Kraken, multiplied by 1e5. |

### Validator state

| Metric | Description |
| --- | --- |
| `solana_voting_status` | 1 when the vote account is current, 0 when delinquent (see `delinquency_seconds`), -1 when not found in the RPC response. |
| `solana_vote_distance_slots` | Current slot minus the vote account's `lastVote`. Taken from the same RPC node in the same loop iteration as `solana_slot`, so alerting thresholds can be chosen in Grafana without an exporter release. |
| `solana_root_distance_slots` | Current slot minus the vote account's `rootSlot`, same source as above. |
| `solana_vote_account_epoch` | Epoch of the newest usable `epochCredits` entry. It lags `solana_epoch` when the vote account or the RPC node is stale. |
| `solana_bls_pubkey_registered` | 1 when the vote account's state contains a BLS pubkey (`blsPubkeyCompressed`), else 0. Admission to the Alpenglow validator set requires it. Polled once per epoch; 0 with a one-time warning when the RPC node does not report the field. |
| `solana_stake{stake_type=...}` | Stake delegated to the vote account, in lamports, by `activated`, `activating`, `deactivating`, `locked`, and account counts `activated_accounts`, `activating_accounts`, `deactivating_accounts`. |
| `solana_identity_balance` | Identity account balance in lamports. |
| `solana_vote_account_balance` | Vote account balance in lamports. |

### Vote rewards (Alpenglow)

Since Alpenglow (mainnet epoch 1064), `epochCredits` entries are `[epoch, cumulative_reward_lamports, previous_cumulative]`, so the per-epoch delta is reward lamports and is almost proportional to the activated stake. The metrics below therefore compare credits **per lamport of stake** against the cluster. A validator can be non-delinquent and still earn well below the cluster when its votes do not reach the leaders inside the aggregation window; these metrics are what catches that.

| Metric | Description |
| --- | --- |
| `solana_epoch_credits` | Credits (reward lamports) earned in the current epoch: the `credits - prev_credits` delta of the entry whose epoch equals the current epoch. 0 when the account has no entry for the current epoch. |
| `solana_epoch_credits_per_stake_ratio` | Our epoch-to-date credits per activated stake, divided by the cluster median of the same quantity over all vote accounts with `activatedStake > 0` and a current-epoch entry. 1.0 means median. |
| `solana_epoch_credits_percentile` | Percentage (0-100) of staked vote accounts whose credits per stake is strictly below ours. Accounts without a current-epoch entry count as 0 earners. |
| `solana_epoch_credits_rate_ratio` | Same ratio as `solana_epoch_credits_per_stake_ratio`, but computed on the increase over the last `reward_window_seconds` instead of epoch-to-date, so a problem that starts mid-epoch shows immediately. This is the metric to alert on. Absent until the window holds at least two snapshots at least 60 s apart, and reset at epoch boundaries. |
| `solana_vote_credit_rank` | **Deprecated since Alpenglow.** Rank of the vote account by raw epoch credits among current vote accounts. Because credits are now proportional to stake this is effectively a rank by stake. It is still emitted for one release so dashboards do not break and will be removed in the next release; switch alerts and dashboards to `solana_epoch_credits_rate_ratio` / `solana_epoch_credits_percentile`. |

All vote-related metrics are derived from a single `getVoteAccounts` call per loop iteration (with `keepUnstakedDelinquents: true`), so they are consistent with each other. The `u64::MAX` migration marker that some vote accounts carry as their last `epochCredits` entry is skipped and never affects the statistics or the `/metrics` endpoint.

### Block production and rewards

| Metric | Description |
| --- | --- |
| `solana_blocks{block_type=...}` | `scheduled`: leader slots in the current epoch; `produced`, `skipped` and `total` from `getBlockProduction`. 0 scheduled slots with non-zero activated stake means the validator was not admitted to the leader schedule (VAT or missing BLS pubkey). |
| `solana_ms_to_next_slot` | Milliseconds until the next leader slot, computed with `solana_slot_time_ms`; -1 when there is no upcoming leader slot. |
| `solana_epoch_block_rewards` | Sum of block rewards (lamports) earned in leader slots this epoch. Slots within the last `recent_window_seconds` are fetched. |
| `solana_last_block_rewards` | Average of the last four non-zero block rewards. |
| `solana_jito_tips` | Balance of the Jito tip distribution account for the current epoch. |

## Metrics Labels

All metrics include the following labels for easy filtering and identification:

- `network`: Defaults to the network name from the config (override with `labels.network`).
- `vote_account`: The validator's vote account public key.
- Any additional key/value pairs provided in the network's `labels` map.

Example metric with labels:

```
solana_slot{network="solana_mainnet",vote_account="YourVoteAccount...",stage="mainnet"} 123456789
```

This makes it straightforward to build Grafana dashboards or alerts scoped to specific environments, regions, or clusters.
