use crate::solana;
use crate::solana::validator::{self, StakeState};
use crate::solana::vote_accounts::{self, VoteAccountAnalysis};
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
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::Mutex;

type MetricLabels = Vec<(String, String)>;
type FloatGauge = Gauge<f64, AtomicU64>;

/// Per-network collection settings, all expressed in wall-clock seconds and
/// converted to slots at runtime with the measured slot time.
#[derive(Debug, Clone, Copy)]
pub struct CollectorSettings {
    /// A validator whose last vote is older than this is reported as delinquent.
    pub delinquency_seconds: u64,
    /// Sliding window for `solana_epoch_credits_rate_ratio`.
    pub reward_window_seconds: u64,
    /// Lookback window for fetching block rewards of recent leader slots.
    pub recent_window_seconds: u64,
}

pub struct Metrics {
    rpc_url: String,
    identity_account: String,
    vote_account: String,
    static_labels: Arc<BTreeMap<String, String>>,
    settings: CollectorSettings,
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
    pub slot_time_ms: Family<MetricLabels, Gauge>,
    pub vote_distance_slots: Family<MetricLabels, Gauge>,
    pub root_distance_slots: Family<MetricLabels, Gauge>,
    pub epoch_credits: Family<MetricLabels, Gauge>,
    pub epoch_credits_per_stake_ratio: Family<MetricLabels, FloatGauge>,
    pub epoch_credits_percentile: Family<MetricLabels, FloatGauge>,
    pub vote_account_epoch: Family<MetricLabels, Gauge>,
}

impl Metrics {
    pub fn new(
        rpc_url: String,
        identity_account: String,
        vote_account: String,
        labels: BTreeMap<String, String>,
        settings: CollectorSettings,
    ) -> Metrics {
        Metrics {
            rpc_url,
            identity_account,
            vote_account,
            static_labels: Arc::new(labels),
            settings,
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
            slot_time_ms: Family::default(),
            vote_distance_slots: Family::default(),
            root_distance_slots: Family::default(),
            epoch_credits: Family::default(),
            epoch_credits_per_stake_ratio: Family::default(),
            epoch_credits_percentile: Family::default(),
            vote_account_epoch: Family::default(),
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

        state.registry.register(
            "solana_blocks",
            "Block production. 0 scheduled slots with non-zero activated stake means the validator was not admitted to the epoch's leader schedule (VAT or missing BLS pubkey)",
            self.blocks.clone(),
        );

        state
            .registry
            .register("solana_jito_tips", "Jito tips", self.jito_tips.clone());

        state.registry.register(
            "solana_vote_credit_rank",
            "DEPRECATED since Alpenglow: epoch credits are reward lamports proportional to stake, so this ranks by stake. Use solana_epoch_credits_rate_ratio / solana_epoch_credits_percentile instead. Will be removed in the next release",
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

        state.registry.register(
            "solana_slot_time_ms",
            "Average slot time in milliseconds measured from recent performance samples; used for every slot/time conversion in this exporter",
            self.slot_time_ms.clone(),
        );

        state.registry.register(
            "solana_vote_distance_slots",
            "Current slot minus the vote account's lastVote, from one getVoteAccounts/getSlot pair on the same RPC node",
            self.vote_distance_slots.clone(),
        );

        state.registry.register(
            "solana_root_distance_slots",
            "Current slot minus the vote account's rootSlot, from one getVoteAccounts/getSlot pair on the same RPC node",
            self.root_distance_slots.clone(),
        );

        state.registry.register(
            "solana_epoch_credits",
            "Credits earned in the current epoch (reward lamports since Alpenglow); 0 if the account has no entry for the current epoch",
            self.epoch_credits.clone(),
        );

        state.registry.register(
            "solana_epoch_credits_per_stake_ratio",
            "Epoch-to-date credits per activated stake divided by the cluster median over staked accounts with a current-epoch entry (1.0 = median)",
            self.epoch_credits_per_stake_ratio.clone(),
        );

        state.registry.register(
            "solana_epoch_credits_percentile",
            "Percentage (0-100) of staked vote accounts whose epoch-to-date credits per stake is strictly below ours; accounts without a current-epoch entry count as 0",
            self.epoch_credits_percentile.clone(),
        );

        state.registry.register(
            "solana_vote_account_epoch",
            "Epoch of the newest usable epochCredits entry; lags solana_epoch when the vote account or the RPC node is stale",
            self.vote_account_epoch.clone(),
        );
    }

    pub fn run_loop(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let client = solana::validator::SolanaClient::new(
                &self.rpc_url,
                &self.identity_account,
                &self.vote_account,
            );

            // Create a channel for communicating current slot, epoch, leader slots and the
            // recent-slot window to the background task
            let (slot_tx, mut slot_rx) =
                tokio::sync::mpsc::unbounded_channel::<(u64, u64, Vec<u64>, u64)>();

            // Spawn background task for block rewards fetching
            let mut bg_client = solana::validator::SolanaClient::new(
                &self.rpc_url,
                &self.identity_account,
                &self.vote_account,
            );
            let bg_self = self.clone();
            tokio::spawn(async move {
                while let Some((current_slot, current_epoch, leader_slots, recent_window_slots)) =
                    slot_rx.recv().await
                {
                    if !leader_slots.is_empty() {
                        match bg_client
                            .get_block_rewards_sum(
                                current_slot,
                                current_epoch,
                                leader_slots,
                                recent_window_slots,
                            )
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

                // Measured slot time drives every slot/time conversion below.
                let slot_time_ms = match client.get_slot_time_ms().await {
                    Ok(ms) => ms,
                    Err(e) => {
                        error!(
                            "Error fetching performance samples: {}; assuming {} ms slots",
                            e,
                            validator::DEFAULT_SLOT_TIME_MS
                        );
                        validator::DEFAULT_SLOT_TIME_MS
                    }
                };
                self.set_slot_time_ms(slot_time_ms);
                let recent_window_slots =
                    validator::seconds_to_slots(self.settings.recent_window_seconds, slot_time_ms);
                let delinquent_slot_distance =
                    validator::delinquency_slots(self.settings.delinquency_seconds, slot_time_ms);

                // Single getVoteAccounts call per iteration; everything vote-related derives from it.
                if let (Some(current_slot), Some((epoch, _))) = (slot, epoch_info) {
                    match client.get_vote_accounts(delinquent_slot_distance).await {
                        Ok(status) => {
                            let analysis = vote_accounts::analyze(
                                &status,
                                &self.vote_account,
                                epoch as u64,
                                current_slot,
                            );
                            self.set_vote_account_analysis(&analysis);
                        }
                        Err(e) => {
                            error!("Error fetching vote accounts: {}", e);
                        }
                    }
                }

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

                    let _ = slot_tx.send((
                        slot.unwrap_or(0),
                        epoch as u64,
                        leader_slots.clone(),
                        recent_window_slots,
                    ));

                    // Update solana_blocks{block_type="scheduled"}
                    self.set_block_scheduled(leader_slots.len() as u64);

                    self.set_ms_to_next_slot(validator::ms_to_next_slot(
                        slot.unwrap_or(0),
                        &leader_slots,
                        slot_time_ms,
                    ));
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

    pub fn set_vote_account_analysis(&self, analysis: &VoteAccountAnalysis) {
        self.set_voting_status(analysis.voting_status);
        self.set_vote_credit_rank(analysis.vote_credit_rank);
        self.set_epoch_credits(analysis.epoch_credits);
        self.set_or_remove_i64(&self.vote_distance_slots, analysis.vote_distance_slots);
        self.set_or_remove_i64(&self.root_distance_slots, analysis.root_distance_slots);
        self.set_or_remove_i64(
            &self.vote_account_epoch,
            analysis.vote_account_epoch.map(|epoch| epoch as i64),
        );
        self.set_or_remove_f64(
            &self.epoch_credits_per_stake_ratio,
            analysis.credits_per_stake_ratio,
        );
        self.set_or_remove_f64(&self.epoch_credits_percentile, analysis.credits_percentile);
    }

    pub fn set_epoch_credits(&self, credits: u64) {
        let labels = self.base_label_pairs();
        self.epoch_credits
            .get_or_create(&labels)
            .set(credits.min(i64::MAX as u64) as i64);
    }

    /// Set the gauge, or drop its sample when there is no value to report.
    fn set_or_remove_i64(&self, family: &Family<MetricLabels, Gauge>, value: Option<i64>) {
        let labels = self.base_label_pairs();
        match value {
            Some(v) => {
                family.get_or_create(&labels).set(v);
            }
            None => {
                family.remove(&labels);
            }
        }
    }

    /// Set the gauge, or drop its sample when there is no value to report (NaN/omitted).
    fn set_or_remove_f64(&self, family: &Family<MetricLabels, FloatGauge>, value: Option<f64>) {
        let labels = self.base_label_pairs();
        match value {
            Some(v) if v.is_finite() => {
                family.get_or_create(&labels).set(v);
            }
            _ => {
                family.remove(&labels);
            }
        }
    }

    pub fn set_slot_time_ms(&self, slot_time_ms: u64) {
        let labels = self.base_label_pairs();
        self.slot_time_ms
            .get_or_create(&labels)
            .set(slot_time_ms as i64);
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
