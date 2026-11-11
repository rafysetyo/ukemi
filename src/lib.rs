//! Ukemi: assurance crowdfunding on NEAR.
//!
//! The only rule: the outcome depends on `raised` versus `target`, decided once
//! after the deadline.
//!   - raised >= target: the creator withdraws the raised funds plus the cushion.
//!   - raised <  target: every backer claims principal + pledge / raised * cushion.
//!
//! Storage: every new campaign and every new (campaign, backer) pledge adds state
//! that is never deleted. The contract measures the bytes it just added and keeps
//! that cost out of the attached deposit, so the contract balance can never be
//! drained by spam. As a result:
//!   - cushion = attached deposit - storage fee of the campaign record
//!   - pledge  = attached deposit - storage fee of the pledge record (first pledge
//!               of an account to a campaign only; later top-ups are charged nothing)
//!
//! Transfers: `claim_refund` and `withdraw` mark the entry as paid first, then send
//! the transfer, then a private callback undoes the mark if the transfer failed
//! (e.g. the receiving account no longer exists), so the funds can be claimed again.

use near_sdk::json_types::{U128, U64};
use near_sdk::store::{LookupMap, Vector};
use near_sdk::{
    env, is_promise_success, near, require, AccountId, Gas, NearToken, PanicOnDefault, Promise,
};

const MAX_TITLE: usize = 80;
const MAX_DESCRIPTION: usize = 500;
const MAX_DURATION_MINUTES: u64 = 30 * 24 * 60;
const NS_PER_MINUTE: u64 = 60 * 1_000_000_000;

fn callback_gas() -> Gas {
    Gas::from_tgas(10)
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Active,
    Funded,
    Missed,
}

#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct Campaign {
    pub id: u32,
    pub creator: AccountId,
    pub title: String,
    pub description: String,
    /// All amounts are in yoctoNEAR (1 NEAR = 10^24), as decimal strings in JSON.
    pub target: U128,
    pub cushion: U128,
    pub raised: U128,
    pub backers: u32,
    /// Nanoseconds since the Unix epoch, as a string in JSON.
    pub deadline_ns: U64,
    pub status: Status,
    pub withdrawn: bool,
}

#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct Pledge {
    pub amount: U128,
    pub claimed: bool,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Ukemi {
    campaigns: Vector<Campaign>,
    pledges: LookupMap<String, Pledge>,
}

fn pledge_key(campaign_id: u32, account: &AccountId) -> String {
    format!("{}:{}", campaign_id, account)
}

/// Cost in yoctoNEAR of the storage added since `usage_before` (a value of
/// `env::storage_usage()`). Callers must `flush()` the collection they just wrote
/// to, because near-sdk's `store` collections only write to storage on flush/drop.
fn storage_fee_since(usage_before: u64) -> u128 {
    let added = env::storage_usage().saturating_sub(usage_before) as u128;
    added * env::storage_byte_cost().as_yoctonear()
}

/// floor(a * b / c) without overflowing u128 in the intermediate product.
/// Double-and-add over the bits of `b`; requires c < 2^126 (total NEAR supply is ~2^110).
fn mul_div(a: u128, b: u128, c: u128) -> u128 {
    require!(c > 0 && c < (1u128 << 126), "Invalid divisor");
    let (a_quot, a_rem) = (a / c, a % c);
    let whole = a_quot
        .checked_mul(b)
        .unwrap_or_else(|| env::panic_str("Overflow"));
    let mut q: u128 = 0;
    let mut rem: u128 = 0; // invariant: a_rem * (bits of b seen so far) = q * c + rem, rem < c
    for bit in (0..128).rev() {
        q <<= 1;
        rem <<= 1;
        if rem >= c {
            rem -= c;
            q += 1;
        }
        if (b >> bit) & 1 == 1 {
            rem += a_rem;
            if rem >= c {
                rem -= c;
                q += 1;
            }
        }
    }
    whole
        .checked_add(q)
        .unwrap_or_else(|| env::panic_str("Overflow"))
}

#[near]
impl Ukemi {
    #[init]
    pub fn new() -> Self {
        Self {
            campaigns: Vector::new(b"c"),
            pledges: LookupMap::new(b"p"),
        }
    }

    /// Creator commits the Gold Cushion by attaching it as the deposit. The storage
    /// fee of the campaign record is taken out of the deposit; the rest is the cushion.
    /// `duration_minutes` is in minutes so demo campaigns can end within minutes.
    #[payable]
    pub fn create_campaign(
        &mut self,
        title: String,
        description: String,
        target: U128,
        duration_minutes: u64,
    ) -> u32 {
        let deposit = env::attached_deposit().as_yoctonear();
        let title = title.trim().to_string();
        require!(
            !title.is_empty() && title.len() <= MAX_TITLE,
            "Title must be 1 to 80 characters"
        );
        require!(
            description.len() <= MAX_DESCRIPTION,
            "Description is limited to 500 characters"
        );
        require!(target.0 > 0, "Target must be positive");
        require!(deposit > 0, "Attach the Gold Cushion as a deposit");
        require!(
            (1..=MAX_DURATION_MINUTES).contains(&duration_minutes),
            "Duration must be 1 minute to 30 days"
        );

        let id = self.campaigns.len();
        let creator = env::predecessor_account_id();
        let deadline_ns = env::block_timestamp() + duration_minutes * NS_PER_MINUTE;

        // Write the record with the full deposit as a placeholder cushion (same
        // serialized size as the final value), measure it, then set the real cushion.
        let usage_before = env::storage_usage();
        self.campaigns.push(Campaign {
            id,
            creator: creator.clone(),
            title,
            description,
            target,
            cushion: U128(deposit),
            raised: U128(0),
            backers: 0,
            deadline_ns: U64(deadline_ns),
            status: Status::Active,
            withdrawn: false,
        });
        self.campaigns.flush();
        let fee = storage_fee_since(usage_before);

        require!(deposit > fee, "Deposit does not cover the storage fee");
        let cushion = deposit - fee;
        require!(cushion <= target.0, "Cushion cannot exceed the target");
        self.campaigns
            .get_mut(id)
            .unwrap_or_else(|| env::panic_str("Campaign not found"))
            .cushion = U128(cushion);

        env::log_str(&format!(
            "campaign_created id={} creator={} cushion={} storage_fee={}",
            id, creator, cushion, fee
        ));
        id
    }

    /// Backer pledges by attaching the deposit. Allowed until the deadline.
    /// The first pledge of an account to a campaign pays the storage fee of its
    /// pledge record out of the deposit; later pledges are credited in full.
    #[payable]
    pub fn back(&mut self, campaign_id: u32) {
        let deposit = env::attached_deposit().as_yoctonear();
        require!(deposit > 0, "Attach a pledge");
        let backer = env::predecessor_account_id();

        {
            let camp = self
                .campaigns
                .get(campaign_id)
                .unwrap_or_else(|| env::panic_str("Campaign not found"));
            require!(camp.status == Status::Active, "Campaign is not active");
            require!(
                env::block_timestamp() < camp.deadline_ns.0,
                "Deadline has passed"
            );
            require!(backer != camp.creator, "Creator cannot back own campaign");
        }

        let key = pledge_key(campaign_id, &backer);
        let is_new = self.pledges.get(&key).is_none();

        let (credited, fee) = if is_new {
            let usage_before = env::storage_usage();
            self.pledges.insert(
                key.clone(),
                Pledge {
                    amount: U128(deposit),
                    claimed: false,
                },
            );
            self.pledges.flush();
            let fee = storage_fee_since(usage_before);

            require!(deposit > fee, "Deposit does not cover the storage fee");
            let credited = deposit - fee;
            self.pledges
                .get_mut(&key)
                .unwrap_or_else(|| env::panic_str("No pledge found"))
                .amount = U128(credited);
            (credited, fee)
        } else {
            let pledge = self
                .pledges
                .get_mut(&key)
                .unwrap_or_else(|| env::panic_str("No pledge found"));
            pledge.amount = U128(
                pledge
                    .amount
                    .0
                    .checked_add(deposit)
                    .unwrap_or_else(|| env::panic_str("Overflow")),
            );
            (deposit, 0)
        };

        let camp = self
            .campaigns
            .get_mut(campaign_id)
            .unwrap_or_else(|| env::panic_str("Campaign not found"));
        camp.raised = U128(
            camp.raised
                .0
                .checked_add(credited)
                .unwrap_or_else(|| env::panic_str("Overflow")),
        );
        if is_new {
            camp.backers += 1;
        }
        env::log_str(&format!(
            "pledged id={} backer={} credited={} storage_fee={}",
            campaign_id, backer, credited, fee
        ));
    }

    /// Anyone can finalize after the deadline. The outcome is decided only by raised vs target.
    pub fn finalize(&mut self, campaign_id: u32) -> Status {
        let camp = self
            .campaigns
            .get_mut(campaign_id)
            .unwrap_or_else(|| env::panic_str("Campaign not found"));
        require!(camp.status == Status::Active, "Already finalized");
        require!(
            env::block_timestamp() >= camp.deadline_ns.0,
            "Deadline not reached"
        );
        camp.status = if camp.raised.0 >= camp.target.0 {
            Status::Funded
        } else {
            Status::Missed
        };
        env::log_str(&format!("finalized id={} status={:?}", campaign_id, camp.status));
        camp.status
    }

    /// Missed campaign: backer claims principal plus pledge / raised * cushion.
    /// If the transfer fails, `on_refund_resolved` clears the `claimed` flag again.
    pub fn claim_refund(&mut self, campaign_id: u32) -> Promise {
        let camp = self
            .campaigns
            .get(campaign_id)
            .unwrap_or_else(|| env::panic_str("Campaign not found"));
        require!(
            camp.status == Status::Missed,
            "Campaign did not miss its target"
        );
        let claimer = env::predecessor_account_id();
        let key = pledge_key(campaign_id, &claimer);
        let pledge = self
            .pledges
            .get_mut(&key)
            .unwrap_or_else(|| env::panic_str("No pledge found"));
        require!(!pledge.claimed, "Already claimed");

        let bonus = mul_div(pledge.amount.0, camp.cushion.0, camp.raised.0);
        let payout = pledge.amount.0 + bonus;
        pledge.claimed = true; // state first, transfer second, rollback in the callback
        env::log_str(&format!(
            "refund_claimed id={} backer={} payout={}",
            campaign_id, claimer, payout
        ));
        Promise::new(claimer.clone())
            .transfer(NearToken::from_yoctonear(payout))
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(callback_gas())
                    .on_refund_resolved(campaign_id, claimer),
            )
    }

    /// Creator: funded campaign pays raised + cushion. A missed campaign with
    /// zero backers returns the cushion (nobody else is owed it).
    /// If the transfer fails, `on_withdraw_resolved` clears the `withdrawn` flag again.
    pub fn withdraw(&mut self, campaign_id: u32) -> Promise {
        let camp = self
            .campaigns
            .get_mut(campaign_id)
            .unwrap_or_else(|| env::panic_str("Campaign not found"));
        let caller = env::predecessor_account_id();
        require!(caller == camp.creator, "Only the creator can withdraw");
        require!(!camp.withdrawn, "Already withdrawn");

        let payout = match camp.status {
            Status::Funded => camp.raised.0 + camp.cushion.0,
            Status::Missed if camp.raised.0 == 0 => camp.cushion.0,
            _ => env::panic_str("Nothing to withdraw"),
        };
        camp.withdrawn = true;
        env::log_str(&format!("withdrawn id={} payout={}", campaign_id, payout));
        Promise::new(caller)
            .transfer(NearToken::from_yoctonear(payout))
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(callback_gas())
                    .on_withdraw_resolved(campaign_id),
            )
    }

    // ---- Callbacks (only the contract account itself can call these) ----

    #[private]
    pub fn on_refund_resolved(&mut self, campaign_id: u32, account_id: AccountId) {
        if !is_promise_success() {
            self.undo_claim(campaign_id, &account_id);
        }
    }

    #[private]
    pub fn on_withdraw_resolved(&mut self, campaign_id: u32) {
        if !is_promise_success() {
            self.undo_withdraw(campaign_id);
        }
    }

    // ---- Views ----

    pub fn get_campaign_count(&self) -> u32 {
        self.campaigns.len()
    }

    pub fn get_campaign(&self, campaign_id: u32) -> Option<Campaign> {
        self.campaigns.get(campaign_id).cloned()
    }

    pub fn get_campaigns(&self, from_index: u32, limit: u32) -> Vec<Campaign> {
        self.campaigns
            .iter()
            .skip(from_index as usize)
            .take(limit as usize)
            .cloned()
            .collect()
    }

    pub fn get_pledge(&self, campaign_id: u32, account_id: AccountId) -> Option<Pledge> {
        self.pledges
            .get(&pledge_key(campaign_id, &account_id))
            .cloned()
    }

    /// Principal + bonus the account can claim now (0 unless the campaign missed and it is unclaimed).
    pub fn get_claimable(&self, campaign_id: u32, account_id: AccountId) -> U128 {
        let Some(camp) = self.campaigns.get(campaign_id) else {
            return U128(0);
        };
        if camp.status != Status::Missed {
            return U128(0);
        }
        match self.pledges.get(&pledge_key(campaign_id, &account_id)) {
            Some(p) if !p.claimed => {
                U128(p.amount.0 + mul_div(p.amount.0, camp.cushion.0, camp.raised.0))
            }
            _ => U128(0),
        }
    }
}

// Not exported as contract methods (plain impl block, no #[near]).
impl Ukemi {
    fn undo_claim(&mut self, campaign_id: u32, account_id: &AccountId) {
        if let Some(p) = self.pledges.get_mut(&pledge_key(campaign_id, account_id)) {
            p.claimed = false;
        }
        env::log_str(&format!(
            "refund_failed id={} backer={} (claim reopened)",
            campaign_id, account_id
        ));
    }

    fn undo_withdraw(&mut self, campaign_id: u32) {
        if let Some(c) = self.campaigns.get_mut(campaign_id) {
            c.withdrawn = false;
        }
        env::log_str(&format!(
            "withdraw_failed id={} (withdraw reopened)",
            campaign_id
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::{accounts, VMContextBuilder};
    use near_sdk::testing_env;

    const NEAR: u128 = 1_000_000_000_000_000_000_000_000;
    const MIN: u64 = 60_000_000_000;
    /// Storage fees for one record are far below 0.1 NEAR.
    const MAX_FEE: u128 = NEAR / 10;

    fn set(predecessor: AccountId, deposit: u128, ts: u64) {
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(predecessor)
            .attached_deposit(NearToken::from_yoctonear(deposit))
            .block_timestamp(ts)
            .account_balance(NearToken::from_near(100_000))
            .build());
    }

    /// Creator (accounts(0)) attaches 100 NEAR (cushion = 100 NEAR minus storage fee)
    /// on a 1000 NEAR target, 10 minute deadline.
    fn setup() -> Ukemi {
        let mut c = Ukemi::new();
        set(accounts(0), 100 * NEAR, 0);
        let id = c.create_campaign("Demo".into(), "Test".into(), U128(1000 * NEAR), 10);
        assert_eq!(id, 0);
        c
    }

    #[test]
    fn mul_div_matches_exact_math() {
        assert_eq!(mul_div(100 * NEAR, 100 * NEAR, 400 * NEAR), 25 * NEAR);
        assert_eq!(mul_div(0, 5, 7), 0);
        assert_eq!(mul_div(7, 3, 2), 10); // floor(21 / 2)
        assert_eq!(mul_div(1, 1, 3), 0);
    }

    #[test]
    fn create_campaign_takes_storage_fee_out_of_cushion() {
        let c = setup();
        let cushion = c.get_campaign(0).unwrap().cushion.0;
        assert!(cushion < 100 * NEAR, "storage fee must be charged");
        assert!(100 * NEAR - cushion < MAX_FEE, "storage fee must stay small");
    }

    #[test]
    #[should_panic(expected = "Deposit does not cover the storage fee")]
    fn create_campaign_rejects_deposit_below_storage_fee() {
        let mut c = Ukemi::new();
        set(accounts(0), 1, 0);
        c.create_campaign("Demo".into(), "Test".into(), U128(1000 * NEAR), 10);
    }

    #[test]
    fn first_pledge_pays_storage_fee_top_up_does_not() {
        let mut c = setup();
        set(accounts(1), 10 * NEAR, MIN);
        c.back(0);
        let first = c.get_pledge(0, accounts(1)).unwrap().amount.0;
        assert!(first < 10 * NEAR, "first pledge must pay the storage fee");
        assert!(10 * NEAR - first < MAX_FEE);

        set(accounts(1), 10 * NEAR, 2 * MIN);
        c.back(0);
        let second = c.get_pledge(0, accounts(1)).unwrap().amount.0;
        assert_eq!(second, first + 10 * NEAR, "top-up is credited in full");

        let camp = c.get_campaign(0).unwrap();
        assert_eq!(camp.backers, 1);
        assert_eq!(camp.raised.0, second);
    }

    #[test]
    #[should_panic(expected = "Deposit does not cover the storage fee")]
    fn first_pledge_rejects_deposit_below_storage_fee() {
        let mut c = setup();
        set(accounts(1), 1, MIN);
        c.back(0);
    }

    #[test]
    fn missed_campaign_splits_cushion_by_pledge() {
        let mut c = setup();
        set(accounts(1), 100 * NEAR, MIN);
        c.back(0);
        set(accounts(2), 300 * NEAR, 2 * MIN);
        c.back(0);

        set(accounts(3), 0, 10 * MIN);
        assert_eq!(c.finalize(0), Status::Missed); // raised ~400 < target 1000

        let camp = c.get_campaign(0).unwrap();
        let p1 = c.get_pledge(0, accounts(1)).unwrap().amount.0;
        let p2 = c.get_pledge(0, accounts(2)).unwrap().amount.0;
        assert_eq!(camp.raised.0, p1 + p2);

        let c1 = c.get_claimable(0, accounts(1)).0;
        let c2 = c.get_claimable(0, accounts(2)).0;
        assert_eq!(c1, p1 + mul_div(p1, camp.cushion.0, camp.raised.0));
        assert_eq!(c2, p2 + mul_div(p2, camp.cushion.0, camp.raised.0));
        // Everything raised + cushion is paid out, minus at most a couple of yocto of rounding.
        let total = camp.raised.0 + camp.cushion.0;
        assert!(c1 + c2 <= total);
        assert!(total - (c1 + c2) <= 2);

        set(accounts(1), 0, 10 * MIN);
        let _ = c.claim_refund(0);
        assert_eq!(c.get_claimable(0, accounts(1)).0, 0);
    }

    #[test]
    #[should_panic(expected = "Already claimed")]
    fn cannot_claim_twice() {
        let mut c = setup();
        set(accounts(1), 100 * NEAR, MIN);
        c.back(0);
        set(accounts(3), 0, 10 * MIN);
        c.finalize(0);
        set(accounts(1), 0, 10 * MIN);
        let _ = c.claim_refund(0);
        let _ = c.claim_refund(0);
    }

    #[test]
    fn failed_refund_transfer_can_be_retried() {
        let mut c = setup();
        set(accounts(1), 100 * NEAR, MIN);
        c.back(0);
        set(accounts(3), 0, 10 * MIN);
        c.finalize(0);

        set(accounts(1), 0, 10 * MIN);
        let _ = c.claim_refund(0);
        assert_eq!(c.get_claimable(0, accounts(1)).0, 0);

        c.undo_claim(0, &accounts(1)); // what the callback does when the transfer failed
        assert!(c.get_claimable(0, accounts(1)).0 > 0);
        let _ = c.claim_refund(0); // retry succeeds
        assert_eq!(c.get_claimable(0, accounts(1)).0, 0);
    }

    #[test]
    fn funded_campaign_pays_creator_once() {
        let mut c = setup();
        set(accounts(1), 600 * NEAR, MIN);
        c.back(0);
        set(accounts(2), 500 * NEAR, 2 * MIN);
        c.back(0);

        set(accounts(3), 0, 10 * MIN);
        assert_eq!(c.finalize(0), Status::Funded); // raised ~1100 >= target 1000

        set(accounts(0), 0, 10 * MIN);
        let _ = c.withdraw(0);
        assert!(c.get_campaign(0).unwrap().withdrawn);
    }

    #[test]
    fn failed_withdraw_transfer_can_be_retried() {
        let mut c = setup();
        set(accounts(1), 1100 * NEAR, MIN);
        c.back(0);
        set(accounts(3), 0, 10 * MIN);
        c.finalize(0);

        set(accounts(0), 0, 10 * MIN);
        let _ = c.withdraw(0);
        assert!(c.get_campaign(0).unwrap().withdrawn);

        c.undo_withdraw(0); // what the callback does when the transfer failed
        assert!(!c.get_campaign(0).unwrap().withdrawn);
        let _ = c.withdraw(0); // retry succeeds
        assert!(c.get_campaign(0).unwrap().withdrawn);
    }

    #[test]
    #[should_panic(expected = "Already withdrawn")]
    fn cannot_withdraw_twice() {
        let mut c = setup();
        set(accounts(1), 1100 * NEAR, MIN);
        c.back(0);
        set(accounts(3), 0, 10 * MIN);
        c.finalize(0);
        set(accounts(0), 0, 10 * MIN);
        let _ = c.withdraw(0);
        let _ = c.withdraw(0);
    }

    #[test]
    #[should_panic(expected = "Campaign did not miss its target")]
    fn funded_backers_cannot_claim_refund() {
        let mut c = setup();
        set(accounts(1), 1100 * NEAR, MIN);
        c.back(0);
        set(accounts(3), 0, 10 * MIN);
        c.finalize(0);
        set(accounts(1), 0, 10 * MIN);
        let _ = c.claim_refund(0);
    }

    #[test]
    fn missed_with_no_backers_returns_cushion_to_creator() {
        let mut c = setup();
        set(accounts(3), 0, 10 * MIN);
        assert_eq!(c.finalize(0), Status::Missed);
        set(accounts(0), 0, 10 * MIN);
        let _ = c.withdraw(0);
    }

    #[test]
    #[should_panic(expected = "Deadline not reached")]
    fn cannot_finalize_early() {
        let mut c = setup();
        set(accounts(3), 0, 5 * MIN);
        c.finalize(0);
    }

    #[test]
    #[should_panic(expected = "Deadline has passed")]
    fn cannot_back_after_deadline() {
        let mut c = setup();
        set(accounts(1), 10 * NEAR, 10 * MIN);
        c.back(0);
    }

    #[test]
    #[should_panic(expected = "Creator cannot back own campaign")]
    fn creator_cannot_back_own_campaign() {
        let mut c = setup();
        set(accounts(0), 10 * NEAR, MIN);
        c.back(0);
    }

    #[test]
    #[should_panic(expected = "Only the creator can withdraw")]
    fn only_creator_can_withdraw() {
        let mut c = setup();
        set(accounts(1), 1100 * NEAR, MIN);
        c.back(0);
        set(accounts(3), 0, 10 * MIN);
        c.finalize(0);
        set(accounts(1), 0, 10 * MIN);
        let _ = c.withdraw(0);
    }
}
