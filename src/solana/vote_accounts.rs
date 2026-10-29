//! Pure analysis of a `getVoteAccounts` response.
//!
//! Everything in this module operates on a single [`RpcVoteAccountStatus`]
//! snapshot so that voting status, vote/root distances, epoch credits and the
//! cluster-wide statistics are derived from one consistent RPC response.
//!
//! Reading `epochCredits` since Alpenglow:
//! - entries are `(epoch, cumulative_reward_lamports, previous_cumulative)` as `(u64, u64, u64)`;
//! - a few accounts carry a migration marker `(u64::MAX, u64::MAX, u64::MAX)` as their last
//!   entry, which must be skipped and never cast to a narrower integer;
//! - the per-epoch delta is roughly proportional to the activated stake, so the cluster
//!   statistics are computed on credits per lamport of stake.

use solana_rpc_client_api::response::{RpcVoteAccountInfo, RpcVoteAccountStatus};
use std::collections::{HashMap, VecDeque};

/// Marker epoch written into `epochCredits` by the Alpenglow vote-state migration.
pub const MIGRATION_MARKER_EPOCH: u64 = u64::MAX;

/// Voting status values exported as `solana_voting_status`.
pub const VOTING_STATUS_CURRENT: i64 = 1;
pub const VOTING_STATUS_DELINQUENT: i64 = 0;
pub const VOTING_STATUS_NOT_FOUND: i64 = -1;

/// `(epoch, credits_earned_in_epoch)` for every usable entry: migration markers and entries
/// from epochs later than `current_epoch` (stale or misbehaving RPC) are skipped.
fn valid_epoch_credits(
    info: &RpcVoteAccountInfo,
    current_epoch: u64,
) -> impl Iterator<Item = (u64, u64)> + '_ {
    info.epoch_credits
        .iter()
        .filter(move |(epoch, _, _)| *epoch != MIGRATION_MARKER_EPOCH && *epoch <= current_epoch)
        .map(|(epoch, credits, prev_credits)| (*epoch, credits.saturating_sub(*prev_credits)))
}

/// Credits earned in `current_epoch`, or `None` when the account has no entry for it.
pub fn current_epoch_credits(info: &RpcVoteAccountInfo, current_epoch: u64) -> Option<u64> {
    valid_epoch_credits(info, current_epoch)
        .find(|(epoch, _)| *epoch == current_epoch)
        .map(|(_, credits)| credits)
}

/// The newest usable `(epoch, credits_earned)` entry, if any.
pub fn latest_epoch_credits(info: &RpcVoteAccountInfo, current_epoch: u64) -> Option<(u64, u64)> {
    valid_epoch_credits(info, current_epoch).max_by_key(|(epoch, _)| *epoch)
}

/// Median of a slice (any order); `None` for an empty slice.
pub fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let len = values.len();
    let mid = len / 2;
    let (_, upper, _) = values.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
    let upper = *upper;
    if len % 2 == 1 {
        Some(upper)
    } else {
        // The lower-middle element is the maximum of the (unsorted) left partition.
        let lower = values[..mid]
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        Some((lower + upper) / 2.0)
    }
}

/// Percentage (0-100) of `population` strictly below `value`.
pub fn percentile(value: f64, population: &[f64]) -> Option<f64> {
    if population.is_empty() {
        return None;
    }
    let below = population.iter().filter(|v| **v < value).count();
    Some(100.0 * below as f64 / population.len() as f64)
}

/// Distance from an observed slot, clamped at zero if two sequential RPC reads straddle a slot
/// boundary and at `i64::MAX` so it is always representable by the Prometheus gauge.
fn slot_distance(current_slot: u64, observed_slot: u64) -> i64 {
    current_slot
        .saturating_sub(observed_slot)
        .min(i64::MAX as u64) as i64
}

/// Result of analysing one `getVoteAccounts` snapshot for one vote account.
#[derive(Debug, Clone, PartialEq)]
pub struct VoteAccountAnalysis {
    /// 1 current, 0 delinquent, -1 not present in the response.
    pub voting_status: i64,
    /// `current_slot - lastVote`; `None` when the account is not in the response.
    pub vote_distance_slots: Option<i64>,
    /// `current_slot - rootSlot`; `None` when the account is not in the response.
    pub root_distance_slots: Option<i64>,
    /// Credits (lamports since Alpenglow) earned in the current epoch; 0 without a current-epoch entry.
    pub epoch_credits: u64,
    /// Epoch of the newest usable `epochCredits` entry (differs from the current epoch when the
    /// account or the RPC node is stale).
    pub vote_account_epoch: Option<u64>,
    /// `activatedStake` from the same response.
    pub activated_stake: u64,
    /// Our credits per lamport of activated stake; `None` without stake.
    pub credits_per_stake: Option<f64>,
    /// Cluster median credits per stake over staked accounts with a current-epoch entry.
    pub cluster_median_credits_per_stake: Option<f64>,
    /// `credits_per_stake / cluster_median_credits_per_stake`.
    pub credits_per_stake_ratio: Option<f64>,
    /// Percentile (0-100) of our credits per stake among all staked accounts.
    pub credits_percentile: Option<f64>,
    /// Legacy rank by raw epoch credits among current (non-delinquent) accounts; 0 if not ranked.
    pub vote_credit_rank: u32,
}

/// Analyse `status` for `vote_pubkey` at `current_epoch` / `current_slot`.
pub fn analyze(
    status: &RpcVoteAccountStatus,
    vote_pubkey: &str,
    current_epoch: u64,
    current_slot: u64,
) -> VoteAccountAnalysis {
    let in_current = status
        .current
        .iter()
        .find(|info| info.vote_pubkey == vote_pubkey);
    let in_delinquent = status
        .delinquent
        .iter()
        .find(|info| info.vote_pubkey == vote_pubkey);

    let (voting_status, ours) = match (in_current, in_delinquent) {
        (Some(info), _) => (VOTING_STATUS_CURRENT, Some(info)),
        (None, Some(info)) => (VOTING_STATUS_DELINQUENT, Some(info)),
        (None, None) => (VOTING_STATUS_NOT_FOUND, None),
    };

    let vote_distance_slots = ours.map(|info| slot_distance(current_slot, info.last_vote));
    let root_distance_slots = ours.map(|info| slot_distance(current_slot, info.root_slot));
    let activated_stake = ours.map(|info| info.activated_stake).unwrap_or(0);
    let epoch_credits = ours
        .and_then(|info| current_epoch_credits(info, current_epoch))
        .unwrap_or(0);
    let vote_account_epoch = ours
        .and_then(|info| latest_epoch_credits(info, current_epoch))
        .map(|(epoch, _)| epoch);

    // Cluster population: every staked account in the response. Accounts without a
    // current-epoch entry earned nothing this epoch: they are excluded from the median
    // but count as 0 in the percentile population.
    let mut median_values: Vec<f64> = Vec::new();
    let mut population: Vec<f64> = Vec::new();
    for info in status.current.iter().chain(status.delinquent.iter()) {
        if info.activated_stake == 0 {
            continue;
        }
        match current_epoch_credits(info, current_epoch) {
            Some(credits) => {
                let cps = credits as f64 / info.activated_stake as f64;
                median_values.push(cps);
                population.push(cps);
            }
            None => population.push(0.0),
        }
    }
    let cluster_median_credits_per_stake = median(&mut median_values);

    let credits_per_stake = if activated_stake > 0 {
        Some(epoch_credits as f64 / activated_stake as f64)
    } else {
        None
    };
    let credits_per_stake_ratio = match (credits_per_stake, cluster_median_credits_per_stake) {
        (Some(ours), Some(median)) if median > 0.0 => Some(ours / median),
        _ => None,
    };
    let credits_percentile = credits_per_stake.and_then(|ours| percentile(ours, &population));

    VoteAccountAnalysis {
        voting_status,
        vote_distance_slots,
        root_distance_slots,
        epoch_credits,
        vote_account_epoch,
        activated_stake,
        credits_per_stake,
        cluster_median_credits_per_stake,
        credits_per_stake_ratio,
        credits_percentile,
        vote_credit_rank: vote_credit_rank(&status.current, vote_pubkey, current_epoch),
    }
}

/// Deprecated since Alpenglow: ranks `current` accounts by the credits of their newest
/// `epochCredits` entry, which is now roughly a ranking by stake. Equal credits share a rank.
/// Returns 0 when `vote_pubkey` is not among the current accounts.
pub fn vote_credit_rank(
    current: &[RpcVoteAccountInfo],
    vote_pubkey: &str,
    current_epoch: u64,
) -> u32 {
    let mut validator_credits: Vec<(&str, u64)> = current
        .iter()
        .map(|info| {
            let credits = latest_epoch_credits(info, current_epoch)
                .map(|(_, credits)| credits)
                .unwrap_or(0);
            (info.vote_pubkey.as_str(), credits)
        })
        .collect();
    validator_credits.sort_by_key(|entry| std::cmp::Reverse(entry.1));

    let mut place = 0u32;
    let mut current_credits = None;
    for (index, (pubkey, credits)) in validator_credits.iter().enumerate() {
        match current_credits {
            Some(prev) if prev == *credits => {}
            _ => {
                place = (index + 1) as u32;
                current_credits = Some(*credits);
            }
        }
        if *pubkey == vote_pubkey {
            return place;
        }
    }
    0
}

/// Minimum span the sliding window must cover before a rate ratio is reported.
pub const MIN_RATE_WINDOW_SECONDS: u64 = 60;

/// `(activatedStake, current_epoch_credits)` of one staked account in a snapshot.
type StakeAndCredits = (u64, u64);

#[derive(Debug, Clone, PartialEq)]
struct RateSnapshot {
    timestamp_secs: u64,
    slot: u64,
    epoch: u64,
    /// Every account with `activatedStake > 0`, keyed by vote pubkey. Accounts without a
    /// current-epoch entry are stored with 0 credits; their delta is never positive, so they
    /// drop out of the cluster statistics on their own.
    accounts: HashMap<String, StakeAndCredits>,
}

/// Windowed reward rate of our vote account relative to the cluster.
#[derive(Debug, Clone, PartialEq)]
pub struct RewardRate {
    /// Our windowed credits per stake divided by the cluster median of the per-account
    /// windowed credits per stake (1.0 = median).
    pub ratio: f64,
    /// Percentile (0-100) of our windowed credits per stake among the accounts in the median.
    pub percentile: f64,
    /// Slot of the oldest snapshot in the window.
    pub first_slot: u64,
    /// Slot of the newest snapshot in the window.
    pub last_slot: u64,
}

impl RewardRate {
    /// Number of `leader_slots` inside the window: slots in `(first_slot, last_slot]`, the
    /// range whose votes and leader rewards the windowed credit delta covers.
    pub fn leader_slots_in_window(&self, leader_slots: &[u64]) -> u64 {
        leader_slots
            .iter()
            .filter(|slot| **slot > self.first_slot && **slot <= self.last_slot)
            .count() as u64
    }
}

/// Sliding window of per-account `(stake, credits)` snapshots used for
/// `solana_epoch_credits_rate_ratio` and `solana_epoch_credits_rate_percentile`.
///
/// For every staked account present in both the oldest and the newest snapshot, the windowed
/// credits per stake is `(credits_newest - credits_oldest) / stake_newest`. Ours is divided by
/// the **median of these per-account values**, not by the growth of the cluster median
/// cumulative value: under SIMD-0326 the leader of a vote aggregate is paid as much as every
/// voter it includes, so over an epoch the median cumulative credits grow at about twice the
/// pure voting rate, while over a 15-minute window the median validator has had no leader slot.
/// Dividing by the growth of the cumulative median therefore reads ~0.5 for a healthy
/// validator; the median of per-account deltas reads ~1.0.
///
/// The epoch-to-date ratio dilutes a problem that starts mid-epoch; this one does not.
#[derive(Debug)]
pub struct RewardRateWindow {
    window_seconds: u64,
    snapshots: VecDeque<RateSnapshot>,
}

impl RewardRateWindow {
    pub fn new(window_seconds: u64) -> Self {
        Self {
            window_seconds,
            snapshots: VecDeque::new(),
        }
    }

    /// Record a snapshot of `status` taken at `timestamp_secs` / `slot` in `epoch` and return
    /// our reward rate, or `None` while the window holds fewer than two snapshots, spans less
    /// than [`MIN_RATE_WINDOW_SECONDS`], we are unstaked or absent from either end of the
    /// window, or no account in the cluster earned anything over the window.
    pub fn push(
        &mut self,
        timestamp_secs: u64,
        slot: u64,
        epoch: u64,
        status: &RpcVoteAccountStatus,
        vote_pubkey: &str,
    ) -> Option<RewardRate> {
        // Credits reset at the epoch boundary: snapshots from another epoch are useless.
        if self
            .snapshots
            .back()
            .is_some_and(|last| last.epoch != epoch)
        {
            self.snapshots.clear();
        }
        // Timestamps that move backwards would corrupt the deltas; start over.
        if self
            .snapshots
            .back()
            .is_some_and(|last| last.timestamp_secs > timestamp_secs)
        {
            self.snapshots.clear();
        }

        let accounts = status
            .current
            .iter()
            .chain(status.delinquent.iter())
            .filter(|info| info.activated_stake > 0)
            .map(|info| {
                let credits = current_epoch_credits(info, epoch).unwrap_or(0);
                (info.vote_pubkey.clone(), (info.activated_stake, credits))
            })
            .collect();
        self.snapshots.push_back(RateSnapshot {
            timestamp_secs,
            slot,
            epoch,
            accounts,
        });
        // Keep the newest snapshot that is already outside the window, so the covered span
        // is always at least `window_seconds` once enough history exists. Dropping every
        // snapshot older than the window would leave a span of one poll interval for small
        // windows and never reach MIN_RATE_WINDOW_SECONDS.
        let outside_window = |snapshot: &RateSnapshot| {
            timestamp_secs - snapshot.timestamp_secs > self.window_seconds
        };
        while self.snapshots.get(1).is_some_and(outside_window) {
            self.snapshots.pop_front();
        }

        let (first, last) = (self.snapshots.front()?, self.snapshots.back()?);
        if self.snapshots.len() < 2
            || last.timestamp_secs - first.timestamp_secs < MIN_RATE_WINDOW_SECONDS
        {
            return None;
        }

        // Windowed credits per stake of every account present at both ends of the window.
        // Accounts that earned nothing (delta <= 0) are excluded: they are delinquent, stale,
        // or joined mid-window, and would otherwise drag the median towards zero.
        let windowed_credits_per_stake = |pubkey: &str| -> Option<f64> {
            let (stake, newest) = *last.accounts.get(pubkey)?;
            let (_, oldest) = *first.accounts.get(pubkey)?;
            Some(newest.saturating_sub(oldest) as f64 / stake as f64)
        };
        let mut population: Vec<f64> = last
            .accounts
            .keys()
            .filter_map(|pubkey| windowed_credits_per_stake(pubkey))
            .filter(|rate| *rate > 0.0)
            .collect();

        let ours = windowed_credits_per_stake(vote_pubkey)?;
        let percentile = percentile(ours, &population)?;
        let median = median(&mut population)?;
        Some(RewardRate {
            ratio: ours / median,
            percentile,
            first_slot: first.slot,
            last_slot: last.slot,
        })
    }

    /// Number of snapshots currently held.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.snapshots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPOCH: u64 = 1070;
    const SLOT: u64 = 462_000_500;

    fn assert_close(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("expected a value");
        assert!(
            (actual - expected).abs() < 1e-12,
            "expected {expected}, got {actual}"
        );
    }

    fn fixture() -> RpcVoteAccountStatus {
        serde_json::from_str(include_str!("fixtures/vote_accounts.json")).unwrap()
    }

    fn account(status: &RpcVoteAccountStatus, pubkey: &str) -> RpcVoteAccountInfo {
        status
            .current
            .iter()
            .chain(status.delinquent.iter())
            .find(|info| info.vote_pubkey == pubkey)
            .cloned()
            .unwrap()
    }

    #[test]
    fn trailing_migration_marker_is_skipped_without_panicking() {
        let status = fixture();
        let marker = account(&status, "Marker11111111111111111111111111111111111111");
        assert_eq!(
            marker.epoch_credits.last().unwrap().0,
            MIGRATION_MARKER_EPOCH
        );
        // Current-epoch entry is selected, not the marker.
        assert_eq!(current_epoch_credits(&marker, EPOCH), Some(500));
        assert_eq!(latest_epoch_credits(&marker, EPOCH), Some((EPOCH, 500)));

        let analysis = analyze(&status, &marker.vote_pubkey, EPOCH, SLOT);
        assert_eq!(analysis.epoch_credits, 500);
        assert_eq!(analysis.vote_account_epoch, Some(EPOCH));
        assert_eq!(analysis.voting_status, VOTING_STATUS_DELINQUENT);
        // Unstaked: contributes to no statistic.
        assert_eq!(analysis.credits_per_stake, None);
        assert_eq!(analysis.credits_per_stake_ratio, None);
        assert_eq!(analysis.credits_percentile, None);
    }

    #[test]
    fn entries_from_future_epochs_are_ignored() {
        let status = fixture();
        let mut info = account(&status, "Ours11111111111111111111111111111111111111111");
        info.epoch_credits.push((EPOCH + 1, 9_000, 8_000));
        assert_eq!(current_epoch_credits(&info, EPOCH), Some(1_000));
        assert_eq!(latest_epoch_credits(&info, EPOCH), Some((EPOCH, 1_000)));
    }

    #[test]
    fn slot_distances_never_go_negative_when_rpc_reads_straddle_a_slot() {
        let status = fixture();
        let ours = account(&status, "Ours11111111111111111111111111111111111111111");
        let analysis = analyze(&status, &ours.vote_pubkey, EPOCH, ours.last_vote - 1);
        assert_eq!(analysis.vote_distance_slots, Some(0));
        assert!(analysis.root_distance_slots.unwrap() >= 0);
    }

    #[test]
    fn stale_account_earns_zero_and_is_excluded_from_median() {
        let status = fixture();
        let analysis = analyze(
            &status,
            "Stale1111111111111111111111111111111111111111",
            EPOCH,
            SLOT,
        );
        assert_eq!(analysis.voting_status, VOTING_STATUS_DELINQUENT);
        assert_eq!(analysis.epoch_credits, 0);
        assert_eq!(analysis.vote_account_epoch, Some(EPOCH - 1));
        // Staked, so part of the percentile population, with 0 credits per stake:
        // nothing is strictly below it.
        assert_eq!(analysis.credits_percentile, Some(0.0));
        assert_eq!(analysis.credits_per_stake_ratio, Some(0.0));
        // Median is over the 4 staked accounts with a current-epoch entry only (see below).
        assert_close(analysis.cluster_median_credits_per_stake, 0.0105);
    }

    #[test]
    fn median_and_percentile_match_hand_computation() {
        // Staked accounts with a current-epoch entry (credits / stake):
        //   Ours      1000 / 100_000 = 0.0100
        //   Big       5500 / 500_000 = 0.0110
        //   Small      240 /  20_000 = 0.0120
        //   Slow       400 / 100_000 = 0.0040
        // Excluded from the median: Stale (no current-epoch entry), Marker and
        // Zero (activatedStake == 0).
        // Sorted: 0.0040, 0.0100, 0.0110, 0.0120 => median = (0.0100 + 0.0110) / 2 = 0.0105
        // Percentile population (5): Ours, Big, Small, Slow and Stale (0).
        let status = fixture();
        let analysis = analyze(
            &status,
            "Ours11111111111111111111111111111111111111111",
            EPOCH,
            SLOT,
        );
        assert_eq!(analysis.voting_status, VOTING_STATUS_CURRENT);
        assert_eq!(analysis.epoch_credits, 1_000);
        assert_eq!(analysis.activated_stake, 100_000);
        assert_eq!(analysis.vote_distance_slots, Some(3));
        assert_eq!(analysis.root_distance_slots, Some(35));
        assert_eq!(analysis.credits_per_stake, Some(0.01));
        assert_close(analysis.cluster_median_credits_per_stake, 0.0105);
        assert_close(analysis.credits_per_stake_ratio, 0.01 / 0.0105);
        // Below 0.0100: Slow (0.0040) and Stale (0) => 2 of 5 => 40%.
        assert_eq!(analysis.credits_percentile, Some(40.0));

        let small = analyze(
            &status,
            "Small111111111111111111111111111111111111111",
            EPOCH,
            SLOT,
        );
        assert_eq!(small.credits_percentile, Some(80.0));
        let slow = analyze(
            &status,
            "Slow1111111111111111111111111111111111111111",
            EPOCH,
            SLOT,
        );
        assert_eq!(slow.credits_percentile, Some(20.0));
    }

    #[test]
    fn zero_stake_account_is_excluded_from_statistics() {
        let status = fixture();
        let zero = analyze(
            &status,
            "Zero1111111111111111111111111111111111111111",
            EPOCH,
            SLOT,
        );
        assert_eq!(zero.voting_status, VOTING_STATUS_CURRENT);
        assert_eq!(zero.epoch_credits, 10);
        assert_eq!(zero.credits_per_stake, None);
        assert_eq!(zero.credits_per_stake_ratio, None);
        assert_eq!(zero.credits_percentile, None);
        // Its very high credits/stake (10 / 0) must not have leaked into the median.
        assert_close(zero.cluster_median_credits_per_stake, 0.0105);
    }

    #[test]
    fn unknown_account_is_not_found() {
        let status = fixture();
        let analysis = analyze(&status, "Nobody", EPOCH, SLOT);
        assert_eq!(analysis.voting_status, VOTING_STATUS_NOT_FOUND);
        assert_eq!(analysis.vote_distance_slots, None);
        assert_eq!(analysis.root_distance_slots, None);
        assert_eq!(analysis.epoch_credits, 0);
        assert_eq!(analysis.vote_account_epoch, None);
        assert_eq!(analysis.vote_credit_rank, 0);
    }

    #[test]
    fn legacy_rank_is_by_raw_credits_among_current_accounts() {
        // Current accounts by newest-entry credits: Big 5500, Ours 1000, Slow 400, Small 240, Zero 10.
        let status = fixture();
        assert_eq!(
            vote_credit_rank(
                &status.current,
                "Big11111111111111111111111111111111111111111",
                EPOCH
            ),
            1
        );
        assert_eq!(
            vote_credit_rank(
                &status.current,
                "Ours11111111111111111111111111111111111111111",
                EPOCH
            ),
            2
        );
        assert_eq!(
            vote_credit_rank(
                &status.current,
                "Zero1111111111111111111111111111111111111111",
                EPOCH
            ),
            5
        );
        // Delinquent accounts are not ranked.
        assert_eq!(
            vote_credit_rank(
                &status.current,
                "Stale1111111111111111111111111111111111111111",
                EPOCH
            ),
            0
        );
    }

    #[test]
    fn rank_shares_place_on_equal_credits() {
        let mut status = fixture();
        // Give Slow the same credits as Ours.
        for info in status.current.iter_mut() {
            if info.vote_pubkey.starts_with("Slow") {
                info.epoch_credits = vec![(EPOCH, 1_000, 0)];
            }
        }
        assert_eq!(
            vote_credit_rank(
                &status.current,
                "Ours11111111111111111111111111111111111111111",
                EPOCH
            ),
            2
        );
        assert_eq!(
            vote_credit_rank(
                &status.current,
                "Slow1111111111111111111111111111111111111111",
                EPOCH
            ),
            2
        );
        assert_eq!(
            vote_credit_rank(
                &status.current,
                "Small111111111111111111111111111111111111111",
                EPOCH
            ),
            4
        );
    }

    #[test]
    fn median_of_odd_and_even_sets() {
        assert_eq!(median(&mut []), None);
        assert_eq!(median(&mut [3.0]), Some(3.0));
        assert_eq!(median(&mut [5.0, 1.0, 3.0]), Some(3.0));
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(&mut [1.0, 1.0, 1.0, 1.0]), Some(1.0));
    }

    #[test]
    fn percentile_is_strictly_below() {
        assert_eq!(percentile(1.0, &[]), None);
        assert_eq!(percentile(1.0, &[1.0]), Some(0.0));
        assert_eq!(percentile(2.0, &[1.0, 2.0, 3.0, 4.0]), Some(25.0));
        assert_eq!(percentile(5.0, &[1.0, 2.0, 3.0, 4.0]), Some(100.0));
    }

    const OURS: &str = "Ours11111111111111111111111111111111111111111";
    const BIG: &str = "Big11111111111111111111111111111111111111111";
    const SMALL: &str = "Small111111111111111111111111111111111111111";
    const SLOW: &str = "Slow1111111111111111111111111111111111111111";
    const STEADY: &str = "Steady11111111111111111111111111111111111111";

    /// Fixture plus a fifth staked account so the windowed population has an odd size.
    fn five_accounts() -> RpcVoteAccountStatus {
        let mut status = fixture();
        let mut steady = account(&status, SLOW);
        steady.vote_pubkey = STEADY.to_string();
        steady.node_pubkey = "SteadyIdentity111111111111111111111111111111".to_string();
        steady.activated_stake = 200_000;
        steady.epoch_credits = vec![(EPOCH, 2_000, 0)];
        status.current.push(steady);
        status
    }

    /// `status` some time later: every listed account's current-epoch credits grew by `delta`.
    fn advanced(status: &RpcVoteAccountStatus, deltas: &[(&str, u64)]) -> RpcVoteAccountStatus {
        let mut later = status.clone();
        for info in later.current.iter_mut().chain(later.delinquent.iter_mut()) {
            if let Some((_, delta)) = deltas
                .iter()
                .find(|(pubkey, _)| *pubkey == info.vote_pubkey)
            {
                let entry = info
                    .epoch_credits
                    .iter_mut()
                    .find(|(epoch, _, _)| *epoch == EPOCH)
                    .expect("account has a current-epoch entry");
                entry.1 += delta;
            }
        }
        later
    }

    /// Everyone earns 0.005 credits per lamport of stake, except Slow (0.002) and Steady (0.006).
    const STEP: &[(&str, u64)] = &[
        (OURS, 500),
        (BIG, 2_500),
        (SMALL, 100),
        (SLOW, 200),
        (STEADY, 1_200),
    ];

    #[test]
    fn rate_ratio_needs_two_snapshots_15_minutes_apart() {
        let mut window = RewardRateWindow::new(900);
        let start = five_accounts();
        // Single snapshot: no sample.
        assert_eq!(window.push(1_000, SLOT, EPOCH, &start, OURS), None);
        assert_eq!(window.len(), 1);
        // 15 min later. Windowed credits per stake: Slow 0.002, Big/Small 0.005,
        // Steady 0.006, ours 625 / 100_000 = 0.00625. Median of the five = 0.005.
        let later = advanced(
            &start,
            &[
                (OURS, 625),
                (BIG, 2_500),
                (SMALL, 100),
                (SLOW, 200),
                (STEADY, 1_200),
            ],
        );
        let rate = window
            .push(1_900, SLOT + 4_500, EPOCH, &later, OURS)
            .unwrap();
        assert_close(Some(rate.ratio), 1.25);
        // Strictly below 0.00625: Slow, Big, Small, Steady => 4 of 5.
        assert_eq!(rate.percentile, 80.0);
        assert_eq!((rate.first_slot, rate.last_slot), (SLOT, SLOT + 4_500));
        assert_eq!(window.len(), 2);
    }

    #[test]
    fn leader_jump_of_one_account_does_not_move_the_median() {
        let start = five_accounts();
        let quiet = advanced(&start, STEP);
        // Same window, but Big also posted vote aggregates as leader and earned 50_000 extra:
        // its windowed credits per stake jumps from 0.005 to 0.105.
        let jump = advanced(&quiet, &[(BIG, 50_000)]);

        let mut quiet_window = RewardRateWindow::new(900);
        quiet_window.push(0, SLOT, EPOCH, &start, OURS);
        let quiet_rate = quiet_window
            .push(900, SLOT + 4_500, EPOCH, &quiet, OURS)
            .unwrap();

        let mut jump_window = RewardRateWindow::new(900);
        jump_window.push(0, SLOT, EPOCH, &start, OURS);
        let jump_rate = jump_window
            .push(900, SLOT + 4_500, EPOCH, &jump, OURS)
            .unwrap();

        // Sorted without the jump: 0.002, 0.005, 0.005, 0.005, 0.006 => median 0.005.
        // With the jump:           0.002, 0.005, 0.005, 0.006, 0.105 => median 0.005.
        assert_close(Some(quiet_rate.ratio), 1.0);
        assert_close(Some(jump_rate.ratio), 1.0);
        assert_eq!(quiet_rate.percentile, 20.0);
        assert_eq!(jump_rate.percentile, 20.0);

        // Big's own ratio does spike for this window: 0.105 / 0.005.
        let mut big_window = RewardRateWindow::new(900);
        big_window.push(0, SLOT, EPOCH, &start, BIG);
        let big_rate = big_window
            .push(900, SLOT + 4_500, EPOCH, &jump, BIG)
            .unwrap();
        assert_close(Some(big_rate.ratio), 21.0);
        assert_eq!(big_rate.percentile, 80.0);
    }

    #[test]
    fn rate_ratio_waits_for_the_minimum_span() {
        let mut window = RewardRateWindow::new(900);
        let start = five_accounts();
        assert_eq!(window.push(1_000, SLOT, EPOCH, &start, OURS), None);
        // 30 s later: two snapshots but under the 60 s minimum.
        let mid = advanced(&start, STEP);
        assert_eq!(window.push(1_030, SLOT + 150, EPOCH, &mid, OURS), None);
        // 60 s after the first snapshot: reported, over the whole window. Ours earned
        // 500 + 1_000 on 100_000 = 0.015; everyone else earned two steps => median 0.010.
        let end = advanced(
            &mid,
            &[
                (OURS, 1_000),
                (BIG, 2_500),
                (SMALL, 100),
                (SLOW, 200),
                (STEADY, 1_200),
            ],
        );
        let rate = window.push(1_060, SLOT + 300, EPOCH, &end, OURS).unwrap();
        assert_close(Some(rate.ratio), 1.5);
    }

    #[test]
    fn rate_ratio_slides_and_resets_on_epoch_change() {
        let mut window = RewardRateWindow::new(900);
        let t0 = five_accounts();
        let t600 = advanced(&t0, STEP);
        let t1200 = advanced(&t600, STEP);
        window.push(0, SLOT, EPOCH, &t0, OURS);
        window.push(600, SLOT + 3_000, EPOCH, &t600, OURS);
        window.push(1_200, SLOT + 6_000, EPOCH, &t1200, OURS);
        // t=0 is outside the window but stays as the boundary snapshot, so the covered span
        // (1200 s) is at least the configured 900 s.
        assert_eq!(window.len(), 3);
        // At t=1800 both t=0 and t=600 are outside the window; t=600 stays as the boundary.
        // Since t=600 ours earned 500 + 250 = 750 => 0.0075; the others two full steps
        // => median 0.010.
        let t1800 = advanced(
            &t1200,
            &[
                (OURS, 250),
                (BIG, 2_500),
                (SMALL, 100),
                (SLOW, 200),
                (STEADY, 1_200),
            ],
        );
        let rate = window
            .push(1_800, SLOT + 9_000, EPOCH, &t1800, OURS)
            .unwrap();
        assert_eq!(window.len(), 3);
        assert_close(Some(rate.ratio), 0.75);
        assert_eq!(rate.first_slot, SLOT + 3_000);

        // New epoch: history is discarded and no sample is reported.
        assert_eq!(
            window.push(2_400, SLOT + 12_000, EPOCH + 1, &t1800, OURS),
            None
        );
        assert_eq!(window.len(), 1);
    }

    #[test]
    fn minimum_window_emits_with_realistic_poll_spacing() {
        // reward_window_seconds at its minimum of 60 with snapshots every 35 s
        // (30 s sleep plus the loop's own RPC time) must still produce samples.
        let mut window = RewardRateWindow::new(60);
        let mut status = five_accounts();
        let mut emitted = Vec::new();
        for i in 0..6u64 {
            let t = i * 35;
            if i > 0 {
                // Everyone earns 0.00035 per lamport of stake per poll.
                status = advanced(
                    &status,
                    &[(OURS, 35), (BIG, 175), (SMALL, 7), (SLOW, 35), (STEADY, 70)],
                );
            }
            emitted.push(window.push(t, SLOT + t * 5, EPOCH, &status, OURS));
        }
        // First sample at t=70 (span 70 s >= 60), then every push.
        assert_eq!(emitted[0], None);
        assert_eq!(emitted[1], None);
        for rate in &emitted[2..] {
            assert_close(rate.as_ref().map(|rate| rate.ratio), 1.0);
        }
        // Never more than one boundary snapshot plus those inside the window.
        assert_eq!(window.len(), 3);
    }

    #[test]
    fn stalled_validator_reads_zero_not_absent() {
        // Ours stops earning while the cluster keeps going: the alertable case.
        let mut window = RewardRateWindow::new(900);
        let start = five_accounts();
        window.push(0, SLOT, EPOCH, &start, OURS);
        let later = advanced(
            &start,
            &[(BIG, 2_500), (SMALL, 100), (SLOW, 200), (STEADY, 1_200)],
        );
        let rate = window.push(900, SLOT + 4_500, EPOCH, &later, OURS).unwrap();
        assert_eq!(rate.ratio, 0.0);
        assert_eq!(rate.percentile, 0.0);
    }

    #[test]
    fn accounts_without_earnings_or_history_are_excluded_from_the_median() {
        let start = fixture();
        let mut window = RewardRateWindow::new(900);
        window.push(0, SLOT, EPOCH, &start, OURS);
        // Steady appears only in the newest snapshot (missing from the oldest) with a huge
        // credit count; Stale is staked but earned nothing; Zero is unstaked.
        let mut later = advanced(&start, STEP);
        let mut steady = account(&start, SLOW);
        steady.vote_pubkey = STEADY.to_string();
        steady.activated_stake = 1_000;
        steady.epoch_credits = vec![(EPOCH, 1_000_000, 0)];
        later.current.push(steady);
        let rate = window.push(900, SLOT + 4_500, EPOCH, &later, OURS).unwrap();
        // Population: Ours 0.005, Big 0.005, Small 0.005, Slow 0.002 => median 0.005.
        assert_close(Some(rate.ratio), 1.0);
        assert_eq!(rate.percentile, 25.0);
    }

    #[test]
    fn rate_is_omitted_without_stake_or_cluster_earnings() {
        let start = five_accounts();
        let later = advanced(&start, STEP);

        // Unstaked account: no sample, but the cluster snapshot is still recorded.
        let mut window = RewardRateWindow::new(900);
        window.push(
            0,
            SLOT,
            EPOCH,
            &start,
            "Zero1111111111111111111111111111111111111111",
        );
        assert_eq!(
            window.push(
                900,
                SLOT + 4_500,
                EPOCH,
                &later,
                "Zero1111111111111111111111111111111111111111"
            ),
            None
        );
        assert_eq!(window.len(), 2);

        // Account not in the response.
        let mut window = RewardRateWindow::new(900);
        window.push(0, SLOT, EPOCH, &start, "Nobody");
        assert_eq!(
            window.push(900, SLOT + 4_500, EPOCH, &later, "Nobody"),
            None
        );

        // Nobody in the cluster earned anything: no median rather than a division by zero.
        let mut window = RewardRateWindow::new(900);
        window.push(0, SLOT, EPOCH, &start, OURS);
        assert_eq!(window.push(900, SLOT + 4_500, EPOCH, &start, OURS), None);
    }

    #[test]
    fn leader_slots_in_window_counts_slots_inside_the_slot_span() {
        let rate = RewardRate {
            ratio: 1.0,
            percentile: 50.0,
            first_slot: 1_000,
            last_slot: 5_500,
        };
        // Half-open: the oldest snapshot's slot is already accounted for, the newest is not.
        assert_eq!(
            rate.leader_slots_in_window(&[300, 1_000, 1_001, 5_500, 5_501]),
            2
        );
        assert_eq!(rate.leader_slots_in_window(&[]), 0);
    }
}
