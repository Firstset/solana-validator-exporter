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
use std::collections::VecDeque;

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

    let vote_distance_slots = ours.map(|info| current_slot as i64 - info.last_vote as i64);
    let root_distance_slots = ours.map(|info| current_slot as i64 - info.root_slot as i64);
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
    validator_credits.sort_by(|a, b| b.1.cmp(&a.1));

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

#[derive(Debug, Clone, PartialEq)]
struct RateSnapshot {
    timestamp_secs: u64,
    epoch: u64,
    our_credits: u64,
    cluster_median_credits_per_stake: f64,
}

/// Sliding window of `(timestamp, our_credits, cluster_median_credits_per_stake)` snapshots
/// used for `solana_epoch_credits_rate_ratio`: our credit increase per stake over the window,
/// divided by the increase of the cluster median over the same window.
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

    /// Record a snapshot taken at `timestamp_secs` and return the rate ratio, or `None` while
    /// the window holds fewer than two snapshots, spans less than
    /// [`MIN_RATE_WINDOW_SECONDS`], or the cluster median did not increase.
    pub fn push(
        &mut self,
        timestamp_secs: u64,
        epoch: u64,
        analysis: &VoteAccountAnalysis,
    ) -> Option<f64> {
        // Without stake or a cluster median there is nothing to compare against.
        let median = analysis.cluster_median_credits_per_stake?;
        if analysis.activated_stake == 0 || analysis.voting_status == VOTING_STATUS_NOT_FOUND {
            return None;
        }

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

        self.snapshots.push_back(RateSnapshot {
            timestamp_secs,
            epoch,
            our_credits: analysis.epoch_credits,
            cluster_median_credits_per_stake: median,
        });
        while self
            .snapshots
            .front()
            .is_some_and(|first| timestamp_secs - first.timestamp_secs > self.window_seconds)
        {
            self.snapshots.pop_front();
        }

        let (first, last) = (self.snapshots.front()?, self.snapshots.back()?);
        if self.snapshots.len() < 2
            || last.timestamp_secs - first.timestamp_secs < MIN_RATE_WINDOW_SECONDS
        {
            return None;
        }
        let our_delta = last.our_credits.saturating_sub(first.our_credits) as f64
            / analysis.activated_stake as f64;
        let median_delta =
            last.cluster_median_credits_per_stake - first.cluster_median_credits_per_stake;
        if median_delta <= 0.0 {
            return None;
        }
        Some(our_delta / median_delta)
    }

    /// Number of snapshots currently held.
    pub fn len(&self) -> usize {
        self.snapshots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
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

    fn staked_analysis(epoch_credits: u64, median: f64) -> VoteAccountAnalysis {
        VoteAccountAnalysis {
            voting_status: VOTING_STATUS_CURRENT,
            vote_distance_slots: Some(2),
            root_distance_slots: Some(34),
            epoch_credits,
            vote_account_epoch: Some(EPOCH),
            activated_stake: 100_000,
            credits_per_stake: Some(epoch_credits as f64 / 100_000.0),
            cluster_median_credits_per_stake: Some(median),
            credits_per_stake_ratio: None,
            credits_percentile: None,
            vote_credit_rank: 0,
        }
    }

    #[test]
    fn rate_ratio_needs_two_snapshots_15_minutes_apart() {
        let mut window = RewardRateWindow::new(900);
        // Single snapshot: no sample.
        assert_eq!(
            window.push(1_000, EPOCH, &staked_analysis(1_000, 0.0100)),
            None
        );
        assert_eq!(window.len(), 1);
        // 15 min later: we earned 500 more credits on 100_000 stake => 0.005 per stake;
        // the cluster median rose from 0.0100 to 0.0140 => 0.004 per stake. Ratio = 1.25.
        let ratio = window.push(1_900, EPOCH, &staked_analysis(1_500, 0.0140));
        assert_close(ratio, 1.25);
        assert_eq!(window.len(), 2);
    }

    #[test]
    fn rate_ratio_waits_for_the_minimum_span() {
        let mut window = RewardRateWindow::new(900);
        assert_eq!(
            window.push(1_000, EPOCH, &staked_analysis(1_000, 0.0100)),
            None
        );
        // 30 s later: two snapshots but under the 60 s minimum.
        assert_eq!(
            window.push(1_030, EPOCH, &staked_analysis(1_100, 0.0105)),
            None
        );
        // 60 s after the first snapshot: reported, over the whole window.
        let ratio = window.push(1_060, EPOCH, &staked_analysis(1_200, 0.0110));
        // Ours: 200 / 100_000 = 0.002; median: 0.001 => 2.0
        assert_close(ratio, 2.0);
    }

    #[test]
    fn rate_ratio_slides_and_resets_on_epoch_change() {
        let mut window = RewardRateWindow::new(900);
        window.push(0, EPOCH, &staked_analysis(0, 0.0));
        window.push(600, EPOCH, &staked_analysis(600, 0.006));
        window.push(1_200, EPOCH, &staked_analysis(1_200, 0.012));
        // The snapshot at t=0 is older than 900 s and has been dropped.
        assert_eq!(window.len(), 2);
        // At t=1800 the t=600 snapshot slides out as well; the window is [1200, 1800].
        let ratio = window.push(1_800, EPOCH, &staked_analysis(1_500, 0.018));
        assert_eq!(window.len(), 2);
        // Since t=1200: ours 300 / 100_000 = 0.003; median 0.006 => 0.5
        assert_close(ratio, 0.5);

        // New epoch: history is discarded and no sample is reported.
        assert_eq!(
            window.push(2_400, EPOCH + 1, &staked_analysis(10, 0.0001)),
            None
        );
        assert_eq!(window.len(), 1);
    }

    #[test]
    fn rate_ratio_is_omitted_without_stake_or_median() {
        let mut window = RewardRateWindow::new(900);
        let mut unstaked = staked_analysis(1_000, 0.01);
        unstaked.activated_stake = 0;
        assert_eq!(window.push(0, EPOCH, &unstaked), None);
        assert!(window.is_empty());

        let mut no_median = staked_analysis(1_000, 0.01);
        no_median.cluster_median_credits_per_stake = None;
        assert_eq!(window.push(0, EPOCH, &no_median), None);
        assert!(window.is_empty());

        // Flat cluster median: no ratio rather than a division by zero.
        window.push(0, EPOCH, &staked_analysis(0, 0.01));
        assert_eq!(window.push(900, EPOCH, &staked_analysis(100, 0.01)), None);
    }
}
