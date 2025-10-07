mod metrics;
mod solana;
use axum::routing::get;
use axum::Router;
use clap::Parser;
use env_logger::Env;
use log::info;
use metrics::exporter::metrics_handler;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Prometheus exporter for solana validators
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct CliArgs {
    /// Path to config file
    #[arg(long)]
    config_file: String,
}

#[derive(Debug, Deserialize)]
struct Config {
    port: u16,
    networks: BTreeMap<String, NetworkConfig>,
}

#[derive(Debug, Deserialize)]
struct NetworkConfig {
    rpc_url: String,
    validators: Vec<NetworkValidator>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct NetworkValidator {
    vote_address: String,
    identity_address: String,
}

#[derive(Debug, Clone)]
struct ValidatorConfig {
    network_key: String,
    rpc_url: String,
    vote_account: String,
    identity_account: String,
    labels: BTreeMap<String, String>,
}

#[tokio::main]
async fn main() {
    let args = CliArgs::parse();
    let file = File::open(args.config_file).unwrap();
    let reader = BufReader::new(file);
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    // Parse the YAML file
    let config: Config = serde_yaml::from_reader(reader).expect("Error parsing yaml file");

    // Validate configuration and build validator configs
    let mut validator_configs = Vec::new();

    if config.networks.is_empty() {
        panic!("At least one network must be provided under the `networks` configuration section");
    }

    for (network_name, network_config) in &config.networks {
        for validator in &network_config.validators {
            let mut labels: BTreeMap<String, String> = network_config
                .labels
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();

            labels
                .entry("network".to_string())
                .or_insert(network_name.clone());
            labels.insert("vote_account".to_string(), validator.vote_address.clone());

            validator_configs.push(ValidatorConfig {
                network_key: network_name.clone(),
                rpc_url: network_config.rpc_url.clone(),
                vote_account: validator.vote_address.clone(),
                identity_account: validator.identity_address.clone(),
                labels,
            });
        }
    }

    if validator_configs.is_empty() {
        panic!("At least one validator must be configured under `networks`");
    }

    info!(
        "Starting exporter with {} validator(s)!",
        validator_configs.len()
    );

    // Create shared metrics registry
    let shared_state = Arc::new(Mutex::new(metrics::exporter::AppState {
        registry: prometheus_client::registry::Registry::default(),
    }));

    // Create and start metrics collection for each validator
    let mut handles = Vec::new();
    for validator_config in validator_configs {
        let ValidatorConfig {
            network_key,
            rpc_url,
            vote_account,
            identity_account,
            labels,
        } = validator_config;

        info!(
            "Starting metrics collection for network `{}` validator: {}",
            &network_key, &vote_account
        );

        let metrics = Arc::new(metrics::exporter::Metrics::new(
            rpc_url,
            identity_account,
            vote_account,
            labels,
        ));

        // Initialize metrics in shared registry
        metrics.init_registry(shared_state.clone()).await;

        // Start metrics collection loop
        let handle = metrics.clone().run_loop();
        handles.push(handle);
    }

    let router = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(shared_state.clone());

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.port))
        .await
        .unwrap();

    axum::serve(listener, router).await.unwrap();
}
