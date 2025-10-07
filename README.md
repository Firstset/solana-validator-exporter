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

    The exporter automatically adds `network` and `vote_account` labels. If you supply a `network` entry inside `labels`, it overrides the default value derived from the config key.

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
