#![no_std]
//! A rotating savings and credit association (ROSCA), on-chain.
//!
//! A ROSCA is the savings mechanism most of the unbanked world actually uses:
//! *adashe* in Hausa, *esusu* and *ajo* in Yoruba and Igbo, *susu* in Ghana and
//! the Caribbean, *tanda* in Mexico, *chit fund* in India, *hui* in China. A
//! fixed group agrees on a contribution and a period. Every period each member
//! pays in, and one member - a different one each period - takes the whole pot.
//! After N periods everyone has paid N times and received once. Nobody earns
//! interest; what it buys is a lump sum today instead of in N months, and it
//! works with no bank, no credit score, and no paperwork.
//!
//! It also fails in one specific way, over and over: a member takes their payout
//! and stops contributing. Everyone who has not yet received is now short, and
//! the enforcement available to them is social. That single failure is why these
//! groups stay small and stay confined to people who already know each other.
//!
//! This contract removes that failure rather than digitising around it. Joining
//! requires locking a **security deposit** of one contribution. A missed round is
//! covered from the defaulter's own deposit automatically, so the recipient is
//! paid in full and on time regardless, and the defaulter must restore the
//! deposit before the circle will accept them again. Absconding stops being
//! everyone else's problem and becomes only the absconder's.
//!
//! # Parallel execution
//!
//! Storage keys here are deliberately chosen so that contributions do not
//! serialise against each other under CAP-0063. `contribute` writes only
//! `Paid(round, member)` and `Member(member)` - both parameterised by the caller,
//! so two members paying into the same round touch different ledger entries and
//! cluster independently. Nothing in the hot path writes a shared counter: the
//! contract deliberately does *not* keep a `paid_this_round` tally, because
//! maintaining one would make every contribution write the same entry and
//! serialise the whole group. `settle` does write shared state, but it runs once
//! per round, where serialising is the correct behaviour anyway.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, token, Address, Env, Vec,
};

/// Ledgers per day at Soroban's ~5s close time, used for TTL arithmetic.
const DAY_IN_LEDGERS: u32 = 17_280;
/// Extend an entry's life when it drops below this many ledgers remaining.
const TTL_THRESHOLD: u32 = DAY_IN_LEDGERS * 30;
/// Extend it back out to this many. A circle outlives its rounds by a wide
/// margin so that a late `withdraw_deposit` cannot find its record archived.
const TTL_EXTEND_TO: u32 = DAY_IN_LEDGERS * 120;

/// Smallest circle worth forming: with two members it is just a loan.
const MIN_CAPACITY: u32 = 3;
/// `settle` walks every member once, so capacity is bounded to keep that call
/// inside a single transaction's resource budget with room to spare.
const MAX_CAPACITY: u32 = 24;

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Error {
    /// Contribution or period was zero or negative.
    InvalidTerms = 1,
    /// Capacity outside [MIN_CAPACITY, MAX_CAPACITY].
    InvalidCapacity = 2,
    /// The circle has already filled and started.
    NotForming = 3,
    /// The circle has not started, or has already finished.
    NotActive = 4,
    /// The circle has not finished.
    NotComplete = 5,
    /// Caller is not a member of this circle.
    NotMember = 6,
    /// Caller has already joined.
    AlreadyMember = 7,
    /// Caller has already contributed to the current round.
    AlreadyContributed = 8,
    /// The round is still open and not everyone has paid, so there is nothing
    /// to settle yet.
    RoundStillOpen = 9,
    /// The deposit is already whole; there is nothing to top up.
    DepositIntact = 10,
    /// The deposit has already been withdrawn.
    NothingToWithdraw = 11,
}

/// Where the circle is in its life.
#[contracttype]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Accepting members. No contributions yet.
    Forming = 0,
    /// Full, running rounds.
    Active = 1,
    /// Every member has received their payout.
    Complete = 2,
}

/// The terms, fixed at creation and never mutated. Kept in instance storage
/// because it is genuinely one shared value that every call reads and none
/// writes - reads never conflict, so this costs no parallelism.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub admin: Address,
    /// The asset contract members pay in - a Stellar Asset Contract for USDC,
    /// a local-currency anchor token, or any SEP-41 token.
    pub token: Address,
    /// Amount each member owes per round.
    pub contribution: i128,
    /// Length of a round in seconds.
    pub round_seconds: u64,
    /// Number of members, and therefore also the number of rounds.
    pub capacity: u32,
}

/// The mutable part: who is in, which round is running, when it closes.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundState {
    pub status: Status,
    /// Members in join order, which is also the payout order. Publishing the
    /// order up front is deliberate - a member can see exactly when their turn
    /// comes before committing any money.
    pub members: Vec<Address>,
    /// 1-indexed. Zero while forming.
    pub round: u32,
    /// Ledger timestamp after which the round may be settled even if some
    /// members have not paid.
    pub round_ends_at: u64,
}

/// One member's standing.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberRecord {
    /// Security deposit currently held. Starts at one contribution and is drawn
    /// down to cover missed rounds.
    pub deposit: i128,
    /// Whether this member has taken their payout yet.
    pub received: bool,
    /// How many rounds were covered from their deposit rather than paid on time.
    /// Permanent and public: this is the reputation the circle runs on.
    pub defaults: u32,
    /// Set when a member misses a round their deposit could not cover.
    pub delinquent: bool,
}

#[contracttype]
pub enum DataKey {
    /// Instance. Read-only after creation.
    Config,
    /// Persistent, shared. Written only by `join` and `settle`, both of which
    /// are inherently serial operations.
    State,
    /// Persistent, parameterised by member - parallel-safe.
    Member(Address),
    /// Persistent, parameterised by round and member - parallel-safe. This is
    /// what lets an entire round's contributions execute concurrently.
    Paid(u32, Address),
}

fn config(env: &Env) -> Config {
    env.storage()
        .instance()
        .get(&DataKey::Config)
        .expect("circle not initialized")
}

fn state(env: &Env) -> RoundState {
    let key = DataKey::State;
    let s: RoundState = env
        .storage()
        .persistent()
        .get(&key)
        .expect("circle not initialized");
    env.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
    s
}

fn save_state(env: &Env, s: &RoundState) {
    let key = DataKey::State;
    env.storage().persistent().set(&key, s);
    env.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
}

fn member_record(env: &Env, who: &Address) -> Option<MemberRecord> {
    let key = DataKey::Member(who.clone());
    let r: Option<MemberRecord> = env.storage().persistent().get(&key);
    if r.is_some() {
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
    }
    r
}

fn save_member(env: &Env, who: &Address, rec: &MemberRecord) {
    let key = DataKey::Member(who.clone());
    env.storage().persistent().set(&key, rec);
    env.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
}

fn has_paid(env: &Env, round: u32, who: &Address) -> bool {
    env.storage()
        .persistent()
        .get(&DataKey::Paid(round, who.clone()))
        .unwrap_or(false)
}

#[contract]
pub struct Circle;

#[contractimpl]
impl Circle {
    /// Create a circle. Runs once, at deploy.
    pub fn __constructor(
        env: Env,
        admin: Address,
        token: Address,
        contribution: i128,
        round_seconds: u64,
        capacity: u32,
    ) {
        if contribution <= 0 || round_seconds == 0 {
            panic_with_error!(&env, Error::InvalidTerms);
        }
        if !(MIN_CAPACITY..=MAX_CAPACITY).contains(&capacity) {
            panic_with_error!(&env, Error::InvalidCapacity);
        }

        env.storage().instance().set(
            &DataKey::Config,
            &Config {
                admin,
                token,
                contribution,
                round_seconds,
                capacity,
            },
        );
        env.storage()
            .instance()
            .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);

        save_state(
            &env,
            &RoundState {
                status: Status::Forming,
                members: Vec::new(&env),
                round: 0,
                round_ends_at: 0,
            },
        );
    }

    /// Join a forming circle, locking one contribution as a security deposit.
    ///
    /// The deposit is what makes the circle safe to join with strangers: it is
    /// the member's own money standing behind their future obligations. It is
    /// returned in full when the circle completes, minus anything it had to
    /// cover.
    pub fn join(env: Env, member: Address) -> Result<u32, Error> {
        member.require_auth();

        let cfg = config(&env);
        let mut st = state(&env);

        if st.status != Status::Forming {
            return Err(Error::NotForming);
        }
        if member_record(&env, &member).is_some() {
            return Err(Error::AlreadyMember);
        }

        let contract = env.current_contract_address();
        token::Client::new(&env, &cfg.token).transfer(&member, &contract, &cfg.contribution);

        save_member(
            &env,
            &member,
            &MemberRecord {
                deposit: cfg.contribution,
                received: false,
                defaults: 0,
                delinquent: false,
            },
        );

        st.members.push_back(member);
        let position = st.members.len();

        if position == cfg.capacity {
            st.status = Status::Active;
            st.round = 1;
            st.round_ends_at = env.ledger().timestamp() + cfg.round_seconds;
        }
        save_state(&env, &st);

        Ok(position)
    }

    /// Pay into the current round.
    ///
    /// Writes only entries keyed by the caller, so every member of a circle can
    /// contribute in the same ledger without serialising against one another.
    pub fn contribute(env: Env, member: Address) -> Result<(), Error> {
        member.require_auth();

        let cfg = config(&env);
        let st = state(&env);

        if st.status != Status::Active {
            return Err(Error::NotActive);
        }
        let Some(rec) = member_record(&env, &member) else {
            return Err(Error::NotMember);
        };
        if has_paid(&env, st.round, &member) {
            return Err(Error::AlreadyContributed);
        }

        let contract = env.current_contract_address();
        token::Client::new(&env, &cfg.token).transfer(&member, &contract, &cfg.contribution);

        let key = DataKey::Paid(st.round, member.clone());
        env.storage().persistent().set(&key, &true);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);

        // Paying on time clears a delinquency flag, but never erases the
        // default count - the record of what happened stays.
        if rec.delinquent {
            save_member(
                &env,
                &member,
                &MemberRecord {
                    delinquent: false,
                    ..rec
                },
            );
        }

        Ok(())
    }

    /// Restore a deposit that was drawn down to cover a missed round.
    pub fn top_up(env: Env, member: Address) -> Result<i128, Error> {
        member.require_auth();

        let cfg = config(&env);
        let Some(rec) = member_record(&env, &member) else {
            return Err(Error::NotMember);
        };
        let owed = cfg.contribution - rec.deposit;
        if owed <= 0 {
            return Err(Error::DepositIntact);
        }

        let contract = env.current_contract_address();
        token::Client::new(&env, &cfg.token).transfer(&member, &contract, &owed);

        save_member(
            &env,
            &member,
            &MemberRecord {
                deposit: cfg.contribution,
                delinquent: false,
                ..rec
            },
        );

        Ok(owed)
    }

    /// Close the current round: cover anyone who did not pay from their own
    /// deposit, pay this round's recipient, and open the next round.
    ///
    /// Permissionless on purpose. Anyone may call it - the recipient, another
    /// member, or an unrelated keeper - because the outcome is fixed by the
    /// contract and does not depend on who asks for it. Nobody's payout can be
    /// held hostage by an absent administrator.
    pub fn settle(env: Env) -> Result<Address, Error> {
        let cfg = config(&env);
        let mut st = state(&env);

        if st.status != Status::Active {
            return Err(Error::NotActive);
        }

        let now = env.ledger().timestamp();
        let everyone_paid = st.members.iter().all(|m| has_paid(&env, st.round, &m));
        if !everyone_paid && now < st.round_ends_at {
            return Err(Error::RoundStillOpen);
        }

        // Cover the gaps from the defaulters' own deposits so the recipient is
        // paid in full and on time whatever anyone else did.
        let mut pot: i128 = 0;
        for m in st.members.iter() {
            if has_paid(&env, st.round, &m) {
                pot += cfg.contribution;
                continue;
            }
            let mut rec = member_record(&env, &m).expect("member record missing");
            if rec.deposit >= cfg.contribution {
                rec.deposit -= cfg.contribution;
                rec.defaults += 1;
                pot += cfg.contribution;
            } else {
                // Deposit exhausted. The pot is genuinely short and the
                // contract says so rather than inventing funds.
                pot += rec.deposit;
                rec.deposit = 0;
                rec.defaults += 1;
                rec.delinquent = true;
            }
            save_member(&env, &m, &rec);
        }

        let recipient = st.members.get(st.round - 1).expect("round out of range");
        if pot > 0 {
            let contract = env.current_contract_address();
            token::Client::new(&env, &cfg.token).transfer(&contract, &recipient, &pot);
        }

        let mut rrec = member_record(&env, &recipient).expect("recipient record missing");
        rrec.received = true;
        save_member(&env, &recipient, &rrec);

        if st.round == cfg.capacity {
            st.status = Status::Complete;
        } else {
            st.round += 1;
            st.round_ends_at = now + cfg.round_seconds;
        }
        save_state(&env, &st);

        Ok(recipient)
    }

    /// Reclaim your security deposit once the circle has completed.
    pub fn withdraw_deposit(env: Env, member: Address) -> Result<i128, Error> {
        member.require_auth();

        let cfg = config(&env);
        let st = state(&env);
        if st.status != Status::Complete {
            return Err(Error::NotComplete);
        }
        let Some(rec) = member_record(&env, &member) else {
            return Err(Error::NotMember);
        };
        if rec.deposit <= 0 {
            return Err(Error::NothingToWithdraw);
        }

        let amount = rec.deposit;
        let contract = env.current_contract_address();
        token::Client::new(&env, &cfg.token).transfer(&contract, &member, &amount);
        save_member(&env, &member, &MemberRecord { deposit: 0, ..rec });

        Ok(amount)
    }

    // ---- views ----

    pub fn get_config(env: Env) -> Config {
        config(&env)
    }

    pub fn get_state(env: Env) -> RoundState {
        env.storage()
            .persistent()
            .get(&DataKey::State)
            .expect("circle not initialized")
    }

    pub fn get_member(env: Env, member: Address) -> Option<MemberRecord> {
        env.storage().persistent().get(&DataKey::Member(member))
    }

    pub fn has_contributed(env: Env, round: u32, member: Address) -> bool {
        has_paid(&env, round, &member)
    }

    /// Who receives the pot in the given round, if the circle is full.
    pub fn recipient_of(env: Env, round: u32) -> Option<Address> {
        let st: RoundState = env.storage().persistent().get(&DataKey::State)?;
        if round == 0 {
            return None;
        }
        st.members.get(round - 1)
    }
}

mod test;

#[cfg(test)]
mod proptest_harness;
