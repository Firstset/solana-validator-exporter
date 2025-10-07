use crate::solana;
use crate::solana::validator::StakeState;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use log::error;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;

type MetricLabels = Vec<(String, String)>;

pub struct Metrics {
    rpc_url: String,
    identity_account: String,
    vote_account: String,
    static_labels: Arc<BTreeMap<String, String>>,
    pub slot: Family<MetricLabels, Gauge>,
    pub epoch: Family<MetricLabels, Gauge>,
    pub epoch_progress: Family<MetricLabels, Gauge>,
    pub stake: Family<MetricLabels, Gauge>,
    pub identity_balance: Family<MetricLabels, Gauge>,
    pub vote_account_balance: Family<MetricLabels, Gauge>,
    pub blocks: Family<MetricLabels, Gauge>,
    pub jito_tips: Family<MetricLabels, Gauge>,
    pub vote_credit_rank: Family<MetricLabels, Gauge>,
    pub usd_price: Family<MetricLabels, Gauge>,
    pub epoch_block_rewards: Family<MetricLabels, Gauge>,
    pub ms_to_next_slot: Family<MetricLabels, Gauge>,
    pub last_block_rewards: Family<MetricLabels, Gauge>,
    pub voting_status: Family<MetricLabels, Gauge>,
}

impl Metrics {
    pub fn new(
        rpc_url: String,
        identity_account: String,
        vote_account: String,
        labels: BTreeMap<String, String>,
    ) -> Metrics {
        Metrics {
            rpc_url,
            identity_account,
            vote_account,
            static_labels: Arc::new(labels),
            slot: Family::default(),
            epoch: Family::default(),
            epoch_progress: Family::default(),
            stake: Family::default(),
            identity_balance: Family::default(),
            vote_account_balance: Family::default(),
            blocks: Family::default(),
            jito_tips: Family::default(),
            vote_credit_rank: Family::default(),
            usd_price: Family::default(),
            epoch_block_rewards: Family::default(),
            ms_to_next_slot: Family::default(),
            last_block_rewards: Family::default(),
            voting_status: Family::default(),
        }
    }

    pub async fn init_registry(&self, shared_state: Arc<Mutex<AppState>>) {
        let mut state = shared_state.lock().await;

        state
            .registry
            .register("solana_slot", "Slot of cluster", self.slot.clone());

        state
            .registry
            .register("solana_epoch", "Current epoch", self.epoch.clone());

        state.registry.register(
            "solana_epoch_progress",
            "Epoch progress",
            self.epoch_progress.clone(),
        );

        state
            .registry
            .register("solana_stake", "Stake info", self.stake.clone());

        state.registry.register(
            "solana_identity_balance",
            "Identity balance",
            self.identity_balance.clone(),
        );

        state.registry.register(
            "solana_vote_account_balance",
            "Vote account balance",
            self.vote_account_balance.clone(),
        );

        state
            .registry
            .register("solana_blocks", "Block production", self.blocks.clone());

        state
            .registry
            .register("solana_jito_tips", "Jito tips", self.jito_tips.clone());

        state.registry.register(
            "solana_vote_credit_rank",
            "Vote credit rank",
            self.vote_credit_rank.clone(),
        );

        state
            .registry
            .register("solana_usd_price", "USD Price", self.usd_price.clone());

        state.registry.register(
            "solana_epoch_block_rewards",
            "Sum of block rewards this epoch",
            self.epoch_block_rewards.clone(),
        );

        state.registry.register(
            "solana_ms_to_next_slot",
            "Time to next leader slot",
            self.ms_to_next_slot.clone(),
        );

        state.registry.register(
            "solana_last_block_rewards",
            "Average of last non-zero block rewards",
            self.last_block_rewards.clone(),
        );

        state.registry.register(
            "solana_voting_status",
            "Validator voting status (-1=not found from the RPC response, 0=delinquent, 1=voting normally)",
            self.voting_status.clone(),
        );
    }

    pub fn run_loop(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let client = solana::validator::SolanaClient::new(
                &self.rpc_url,
                &self.identity_account,
                &self.vote_account,
            );

            // Create a channel for communicating current slot, epoch, and leader slots to background task
            let (slot_tx, mut slot_rx) =
                tokio::sync::mpsc::unbounded_channel::<(u64, u64, Vec<u64>)>();

            // Spawn background task for block rewards fetching
            let mut bg_client = solana::validator::SolanaClient::new(
                &self.rpc_url,
                &self.identity_account,
                &self.vote_account,
            );
            let bg_self = self.clone();
            tokio::spawn(async move {
                while let Some((current_slot, current_epoch, leader_slots)) = slot_rx.recv().await {
                    if !leader_slots.is_empty() {
                        match bg_client
                            .get_block_rewards_sum(current_slot, current_epoch, leader_slots)
                            .await
                        {
                            Ok(block_rewards) => {
                                bg_self.set_epoch_block_rewards(block_rewards);

                                match bg_client.get_last_block_rewards().await {
                                    Ok(last_rewards) => {
                                        bg_self.set_last_block_rewards(last_rewards);
                                    }
                                    Err(e) => {
                                        error!("Error fetching last block rewards: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Error fetching block rewards: {}", e);
                            }
                        }
                    }
                }
            });

            loop {
                let slot = match client.get_slot().await {
                    Ok(s) => Some(s),
                    Err(e) => {
                        error!("Error fetching slot: {}", e);
                        None
                    }
                };

                if let Some(slot) = slot {
                    self.set_slot(slot);
                }

                let epoch_info = match client.get_epoch().await {
                    Ok(e) => Some(e),
                    Err(e) => {
                        error!("Error fetching epoch: {}", e);
                        None
                    }
                };

                if let Some(epoch_info) = epoch_info {
                    let (epoch, epoch_progress) = epoch_info;
                    self.set_epoch(epoch);
                    self.set_epoch_progress(epoch_progress);

                    let jito_tips = match client.get_jito_tips(epoch).await {
                        Ok(tips) => Some(tips),
                        Err(e) => {
                            error!("Error fetching jito tips: {}", e);
                            None
                        }
                    };

                    if let Some(jito_tips) = jito_tips {
                        self.set_jito_tips(jito_tips);
                    }

                    // Send slot/epoch info to background task
                    let leader_slots = match client.get_leader_info().await {
                        Ok(slots) => slots,
                        Err(e) => {
                            error!("Error fetching leader slots: {}", e);
                            Vec::new()
                        }
                    };

                    let _ = slot_tx.send((slot.unwrap_or(0), epoch as u64, leader_slots.clone()));

                    // Update solana_blocks{block_type="scheduled"}
                    self.set_block_scheduled(leader_slots.len() as u64);

                    let next_slot_ms = match client
                        .get_ms_to_next_slot(slot.unwrap_or(0), leader_slots)
                        .await
                    {
                        Ok(ms) => Some(ms),
                        Err(e) => {
                            error!("Error fetching ms to next slot: {}", e);
                            None
                        }
                    };

                    if let Some(next_slot_ms) = next_slot_ms {
                        self.set_ms_to_next_slot(next_slot_ms);
                    }
                }

                let stake_details = match client.get_stake_details().await {
                    Ok(s) => Some(s),
                    Err(e) => {
                        error!("Error fetching stake details: {}", e);
                        None
                    }
                };

                if let Some(stake_details) = stake_details {
                    self.set_stake(stake_details);
                }

                let identity_balance = match client.get_identity_balance().await {
                    Ok(b) => Some(b),
                    Err(e) => {
                        error!("Error fetching identity balance: {}", e);
                        None
                    }
                };

                if let Some(identity_balance) = identity_balance {
                    self.set_identity_balance(identity_balance);
                }

                let vote_account_balance = match client.get_vote_balance().await {
                    Ok(b) => Some(b),
                    Err(e) => {
                        error!("Error fetching vote account balance: {}", e);
                        None
                    }
                };

                if let Some(vote_account_balance) = vote_account_balance {
                    self.set_vote_account_balance(vote_account_balance);
                }

                let block_production = match client.get_block_production().await {
                    Ok(b) => Some(b),
                    Err(e) => {
                        error!("Error fetching block production: {}", e);
                        None
                    }
                };

                if let Some(block_production) = block_production {
                    let (blocks_produced, blocks_total) = block_production;
                    let blocks_skipped = blocks_total - blocks_produced;
                    self.set_block_production(
                        blocks_total as u64,
                        blocks_produced as u64,
                        blocks_skipped as u64,
                    );
                }

                let vote_credit_rank = match client.get_vote_credit_rank().await {
                    Ok(r) => Some(r),
                    Err(e) => {
                        error!("Error fetching vote credit rank: {}", e);
                        None
                    }
                };

                if let Some(vote_credit_rank) = vote_credit_rank {
                    self.set_vote_credit_rank(vote_credit_rank);
                }

                let usd_price = match client.get_sol_usd_price().await {
                    Ok(p) => Some(p),
                    Err(e) => {
                        error!("Error fetching USD price: {}", e);
                        None
                    }
                };

                if let Some(usd_price) = usd_price {
                    self.set_usd_price(usd_price);
                }

                let voting_status = match client.get_voting_status().await {
                    Ok(status) => Some(status),
                    Err(e) => {
                        error!("Error fetching voting status: {}", e);
                        None
                    }
                };

                if let Some(voting_status) = voting_status {
                    self.set_voting_status(voting_status);
                }

                // Sleep between metric updates
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
            }
        })
    }

    pub fn set_slot(&self, slot: u64) {
        let labels = self.base_label_pairs();
        self.slot.get_or_create(&labels).set(slot as i64);
    }

    pub fn set_epoch(&self, epoch: i64) {
        let labels = self.base_label_pairs();
        self.epoch.get_or_create(&labels).set(epoch);
    }

    pub fn set_epoch_progress(&self, progress: i64) {
        let labels = self.base_label_pairs();
        self.epoch_progress.get_or_create(&labels).set(progress);
    }

    pub fn set_stake(&self, stake_state: StakeState) {
        self.set_stake_metric("activated", stake_state.activated_stake);
        self.set_stake_metric("activating", stake_state.activating_stake);
        self.set_stake_metric("deactivating", stake_state.deactivating_stake);
        self.set_stake_metric("locked", stake_state.locked_stake);
        self.set_stake_metric("activated_accounts", stake_state.activated_stake_accounts);
        self.set_stake_metric("activating_accounts", stake_state.activating_stake_accounts);
        self.set_stake_metric(
            "deactivating_accounts",
            stake_state.deactivating_stake_accounts,
        );
    }

    pub fn set_identity_balance(&self, balance: u64) {
        let labels = self.base_label_pairs();
        self.identity_balance
            .get_or_create(&labels)
            .set(balance as i64);
    }

    pub fn set_vote_account_balance(&self, balance: u64) {
        let labels = self.base_label_pairs();
        self.vote_account_balance
            .get_or_create(&labels)
            .set(balance as i64);
    }

    pub fn set_block_scheduled(&self, scheduled: u64) {
        self.set_block_metric("scheduled", scheduled);
    }

    pub fn set_block_production(&self, total: u64, produced: u64, skipped: u64) {
        self.set_block_metric("total", total);
        self.set_block_metric("produced", produced);
        self.set_block_metric("skipped", skipped);
    }

    pub fn set_jito_tips(&self, tips: u64) {
        let labels = self.base_label_pairs();
        self.jito_tips.get_or_create(&labels).set(tips as i64);
    }

    pub fn set_vote_credit_rank(&self, rank: u32) {
        let labels = self.base_label_pairs();
        self.vote_credit_rank
            .get_or_create(&labels)
            .set(rank as i64);
    }

    pub fn set_usd_price(&self, price: i64) {
        let labels = self.base_label_pairs();
        self.usd_price.get_or_create(&labels).set(price);
    }

    pub fn set_epoch_block_rewards(&self, block_rewards: i64) {
        let labels = self.base_label_pairs();
        self.epoch_block_rewards
            .get_or_create(&labels)
            .set(block_rewards);
    }

    pub fn set_ms_to_next_slot(&self, ms_to_next_slot: i64) {
        let labels = self.base_label_pairs();
        self.ms_to_next_slot
            .get_or_create(&labels)
            .set(ms_to_next_slot);
    }

    pub fn set_last_block_rewards(&self, last_block_rewards: i64) {
        let labels = self.base_label_pairs();
        self.last_block_rewards
            .get_or_create(&labels)
            .set(last_block_rewards);
    }

    pub fn set_voting_status(&self, status: i64) {
        let labels = self.base_label_pairs();
        self.voting_status.get_or_create(&labels).set(status);
    }

    fn set_stake_metric(&self, stake_type: &str, value: u64) {
        let labels = self.labels_with("stake_type", stake_type);
        self.stake.get_or_create(&labels).set(value as i64);
    }

    fn set_block_metric(&self, block_type: &str, value: u64) {
        let labels = self.labels_with("block_type", block_type);
        self.blocks.get_or_create(&labels).set(value as i64);
    }

    fn base_label_pairs(&self) -> MetricLabels {
        self.static_labels
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    fn labels_with(&self, key: &str, value: impl Into<String>) -> MetricLabels {
        let mut labels = self.base_label_pairs();
        labels.push((key.to_string(), value.into()));
        labels
    }
}

pub struct AppState {
    pub registry: Registry,
}

pub async fn metrics_handler(State(state): State<Arc<Mutex<AppState>>>) -> impl IntoResponse {
    let state = state.lock().await;
    let mut body = String::new();
    if let Err(e) = encode(&mut body, &state.registry) {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from(format!("Error encoding metrics: {}", e)))
            .unwrap();
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/openmetrics-text")
        .body(Body::from(body))
        .unwrap()
}
