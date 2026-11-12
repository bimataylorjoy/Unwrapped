//! BTC price from several independent reporters.
//!
//! Each account in `oracle_ids` posts its own price with `report_price`. The
//! price the protocol uses is computed when it is read:
//!
//! 1. take the latest report of every *currently configured* oracle that is
//!    no older than `max_price_age_sec`;
//! 2. require at least `min_oracle_reports` of them (quorum);
//! 3. use their median;
//! 4. refuse the price altogether if the highest and lowest report are more
//!    than `max_oracle_spread_bps` apart.
//!
//! A single reporter therefore cannot set the price. With 3 reporters and a
//! quorum of 2, one that lies is either outvoted by the median or, if the
//! honest ones are missing, caught by the spread check. When the reporters
//! disagree or go quiet the price becomes unavailable, which pauses
//! borrowing, collateral withdrawals and liquidations, but never repayment.

use crate::*;

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug)]
pub struct Report {
    pub oracle_id: AccountId,
    /// Price of 1 BTC in micro-USDC.
    pub usdc_per_btc: U128,
    pub updated_at_ns: U64,
}

/// The aggregated price, as returned by `get_price`.
#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct Price {
    /// Median of the fresh reports, in micro-USDC per BTC.
    pub usdc_per_btc: U128,
    /// Timestamp of the *oldest* report used: the price is no fresher than that.
    pub updated_at_ns: U64,
    /// Number of reports the median was taken over.
    pub sources: u32,
    /// (highest - lowest) / median of the reports used.
    pub spread_bps: u32,
}

#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct ReportView {
    pub oracle_id: AccountId,
    pub usdc_per_btc: U128,
    pub updated_at_ns: U64,
    /// Whether this report is recent enough to count.
    pub fresh: bool,
}

/// Median of fresh reports with a quorum and a spread limit. Pure, so it can
/// be unit-tested without a blockchain.
pub(crate) fn aggregate(
    reports: &[(u128, u64)],
    now_ns: u64,
    max_age_ns: u64,
    quorum: u32,
    max_spread_bps: u32,
) -> Result<(u128, u64, u32, u32), &'static str> {
    let mut fresh: Vec<(u128, u64)> = reports
        .iter()
        .copied()
        .filter(|(_, at)| *at <= now_ns && now_ns - at <= max_age_ns)
        .collect();
    if fresh.is_empty() || (fresh.len() as u32) < quorum {
        return Err("not enough fresh price reports");
    }
    fresh.sort_unstable_by_key(|(price, _)| *price);
    let n = fresh.len();
    let median = if n % 2 == 1 {
        fresh[n / 2].0
    } else {
        // Midpoint of the two middle reports, without overflow.
        let (a, b) = (fresh[n / 2 - 1].0, fresh[n / 2].0);
        a + (b - a) / 2
    };
    if median == 0 {
        return Err("price is zero");
    }
    let spread = (fresh[n - 1].0 - fresh[0].0) * BPS / median;
    if spread > max_spread_bps as u128 {
        return Err("price reporters disagree");
    }
    let oldest = fresh.iter().map(|(_, at)| *at).min().unwrap();
    Ok((median, oldest, n as u32, spread as u32))
}

#[near]
impl Unwrapped {
    /// Posts the caller's BTC price in micro-USDC per BTC, replacing its
    /// previous report. Only accounts in `oracle_ids`.
    pub fn report_price(&mut self, usdc_per_btc: U128) {
        let caller = env::predecessor_account_id();
        require!(self.cfg.oracle_ids.contains(&caller), "caller is not an oracle");
        require!(usdc_per_btc.0 > 0, "price must be positive");

        // Drop reports of oracles that are no longer configured.
        let oracle_ids = &self.cfg.oracle_ids;
        self.reports.retain(|r| oracle_ids.contains(&r.oracle_id) && r.oracle_id != caller);
        self.reports.push(Report {
            oracle_id: caller.clone(),
            usdc_per_btc,
            updated_at_ns: U64(env::block_timestamp()),
        });
        emit("price_reported", json!({ "oracle_id": caller, "usdc_per_btc": usdc_per_btc }));
    }

    /// The aggregated price, or `None` while there is no quorum of fresh,
    /// agreeing reports (see `price_error`).
    pub fn get_price(&self) -> Option<Price> {
        self.aggregate_price().ok()
    }

    /// Why `get_price` is `None`, or `None` if it is available.
    pub fn price_error(&self) -> Option<String> {
        self.aggregate_price().err().map(str::to_string)
    }

    /// Whether a usable price exists right now.
    pub fn is_price_fresh(&self) -> bool {
        self.aggregate_price().is_ok()
    }

    /// The latest report of every configured oracle.
    pub fn get_price_reports(&self) -> Vec<ReportView> {
        let now = env::block_timestamp();
        let max_age = self.max_price_age_ns();
        self.reports
            .iter()
            .filter(|r| self.cfg.oracle_ids.contains(&r.oracle_id))
            .map(|r| ReportView {
                oracle_id: r.oracle_id.clone(),
                usdc_per_btc: r.usdc_per_btc,
                updated_at_ns: r.updated_at_ns,
                fresh: r.updated_at_ns.0 <= now && now - r.updated_at_ns.0 <= max_age,
            })
            .collect()
    }
}

impl Unwrapped {
    fn max_price_age_ns(&self) -> u64 {
        self.cfg.max_price_age_sec.saturating_mul(1_000_000_000)
    }

    fn aggregate_price(&self) -> Result<Price, &'static str> {
        let reports: Vec<(u128, u64)> = self
            .reports
            .iter()
            .filter(|r| self.cfg.oracle_ids.contains(&r.oracle_id))
            .map(|r| (r.usdc_per_btc.0, r.updated_at_ns.0))
            .collect();
        let (median, oldest, sources, spread_bps) = aggregate(
            &reports,
            env::block_timestamp(),
            self.max_price_age_ns(),
            self.cfg.min_oracle_reports,
            self.cfg.max_oracle_spread_bps,
        )?;
        Ok(Price { usdc_per_btc: U128(median), updated_at_ns: U64(oldest), sources, spread_bps })
    }

    pub(crate) fn try_fresh_price(&self) -> Option<u128> {
        self.aggregate_price().ok().map(|p| p.usdc_per_btc.0)
    }

    pub(crate) fn fresh_price(&self) -> u128 {
        match self.aggregate_price() {
            Ok(p) => p.usdc_per_btc.0,
            Err(e) => env::panic_str(&format!("BTC price is unavailable: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Migration from the single-oracle version (deployed 2026-11-12).

#[near(serializers = [borsh])]
struct LegacyConfig {
    mpc_signer: AccountId,
    mpc_domain_id: u64,
    btc_light_client: AccountId,
    btc_hrp: String,
    min_confirmations: u64,
    usdc: AccountId,
    oracle_ids: Vec<AccountId>,
    max_price_age_sec: u64,
    max_ltv_bps: u32,
    liq_threshold_bps: u32,
    liq_bonus_bps: u32,
    base_rate_bps: u32,
    slope_rate_bps: u32,
    mint_fee: NearToken,
    ledger_fee: NearToken,
}

#[near(serializers = [borsh])]
struct LegacyPrice {
    usdc_per_btc: U128,
    updated_at_ns: U64,
}

#[near(serializers = [borsh])]
struct LegacyState {
    owner_id: AccountId,
    cfg: LegacyConfig,
    tokens: NonFungibleToken,
    vaults: LookupMap<u64, Vault>,
    next_vault_id: u64,
    collateral: LookupMap<[u8; 36], CollateralEntry>,
    signature_ledger: LookupSet<(u64, [u8; 36])>,
    pool: Pool,
    shares: LookupMap<AccountId, u128>,
    claimable: LookupMap<AccountId, u128>,
    price: Option<LegacyPrice>,
}

#[near]
impl Unwrapped {
    /// Upgrades state written by the single-oracle version. Vaults, the
    /// signature ledger, the pool and all balances are carried over; the old
    /// single price is discarded, so the new reporters must report before
    /// anyone can borrow.
    #[private]
    #[init(ignore_state)]
    pub fn migrate_to_multi_oracle(
        oracle_ids: Vec<AccountId>,
        min_oracle_reports: u32,
        max_oracle_spread_bps: u32,
        max_price_age_sec: u64,
    ) -> Self {
        let old: LegacyState = env::state_read().unwrap_or_else(|| env::panic_str("no state to migrate"));
        let _ = old.price;
        let c = old.cfg;
        let cfg = Config {
            mpc_signer: c.mpc_signer,
            mpc_domain_id: c.mpc_domain_id,
            btc_light_client: c.btc_light_client,
            btc_hrp: c.btc_hrp,
            min_confirmations: c.min_confirmations,
            usdc: c.usdc,
            oracle_ids,
            min_oracle_reports,
            max_oracle_spread_bps,
            max_price_age_sec,
            max_ltv_bps: c.max_ltv_bps,
            liq_threshold_bps: c.liq_threshold_bps,
            liq_bonus_bps: c.liq_bonus_bps,
            base_rate_bps: c.base_rate_bps,
            slope_rate_bps: c.slope_rate_bps,
            mint_fee: c.mint_fee,
            ledger_fee: c.ledger_fee,
        };
        cfg.assert_valid();
        emit("migrated", json!({ "to": "multi_oracle" }));
        Self {
            owner_id: old.owner_id,
            cfg,
            tokens: old.tokens,
            vaults: old.vaults,
            next_vault_id: old.next_vault_id,
            collateral: old.collateral,
            signature_ledger: old.signature_ledger,
            pool: old.pool,
            shares: old.shares,
            claimable: old.claimable,
            reports: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::aggregate;

    const S: u64 = 1_000_000_000;
    const NOW: u64 = 10_000 * S;
    const AGE: u64 = 600 * S;

    fn agg(r: &[(u128, u64)], quorum: u32, spread: u32) -> Result<(u128, u64, u32, u32), &'static str> {
        aggregate(r, NOW, AGE, quorum, spread)
    }

    #[test]
    fn median_of_odd_and_even_sets() {
        let r = [(100, NOW), (102, NOW - S), (101, NOW - 2 * S)];
        assert_eq!(agg(&r, 2, 500), Ok((101, NOW - 2 * S, 3, 198)));
        let r = [(100, NOW), (103, NOW)];
        assert_eq!(agg(&r, 2, 500).unwrap().0, 101);
    }

    #[test]
    fn one_liar_out_of_three_is_outvoted_or_rejected() {
        // The median ignores a liar...
        let r = [(100_000, NOW), (100_100, NOW), (1, NOW)];
        // ...but the spread check refuses a price the reporters disagree on.
        assert_eq!(agg(&r, 2, 200), Err("price reporters disagree"));
        // Even with the spread check switched off, the liar cannot move the median.
        assert_eq!(agg(&r, 2, u32::MAX).unwrap().0, 100_000);
    }

    #[test]
    fn a_lone_reporter_cannot_set_the_price() {
        assert_eq!(agg(&[(100, NOW)], 2, 200), Err("not enough fresh price reports"));
        assert_eq!(agg(&[], 1, 200), Err("not enough fresh price reports"));
    }

    #[test]
    fn stale_and_future_reports_do_not_count() {
        let r = [(100, NOW), (100, NOW - AGE - 1), (100, NOW + 1)];
        assert_eq!(agg(&r, 2, 200), Err("not enough fresh price reports"));
        let r = [(100, NOW), (100, NOW - AGE)];
        assert_eq!(agg(&r, 2, 200).unwrap().1, NOW - AGE);
    }

    #[test]
    fn a_minority_cannot_drag_the_price_beyond_the_spread_limit() {
        // Two honest reporters at 100,000; a third pushes 1.9% higher, just
        // inside a 2% limit. The median does not move.
        let r = [(100_000, NOW), (100_000, NOW), (101_900, NOW)];
        assert_eq!(agg(&r, 2, 200).unwrap().0, 100_000);
        // If one honest reporter is missing the liar can move the midpoint by
        // at most half the spread limit.
        let r = [(100_000, NOW), (101_900, NOW)];
        assert_eq!(agg(&r, 2, 200).unwrap().0, 100_950);
        let r = [(100_000, NOW), (102_100, NOW)];
        assert_eq!(agg(&r, 2, 200), Err("price reporters disagree"));
    }
}
