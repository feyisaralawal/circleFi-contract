# CircleFi - the circle contract

**A rotating savings circle that a stranger can safely join.**

A Soroban contract for the savings mechanism most of the unbanked world already
uses - *adashe* in Hausa, *esusu* and *ajo* in Yoruba and Igbo, *susu* in Ghana
and the Caribbean, *tanda* in Mexico, *chit fund* in India, *hui* in China.

An app for it: https://circle-fi.github.io/circleFi-app/ - browse a circle, or open one from a wallet, with no CLI. The [SDK](https://github.com/circle-Fi/circleFi-sdk) wraps these contracts for everything else.

## Live on testnet

```
factory      CCJRXTYIEFE6Z7DGTAKRGBLOGYBZNOONHI7FWZUDXEKZ7LGEGZNXKG3M
circle wasm  8c67956897b34dd87029a92620f89ea6a8d2936963fbc161661c531d3a4e5943
network      Test SDF Network ; September 2015
token        native (XLM) Stellar Asset Contract
```

The factory holds the circle's code hash and deploys a fresh circle per call,
so anyone with a wallet can open one - no CLI, no upload step. Circles it has
opened, newest first:

```sh
stellar contract invoke --id CCJRXTYIEFE6Z7DGTAKRGBLOGYBZNOONHI7FWZUDXEKZ7LGEGZNXKG3M \
  --network testnet --source <your-key> -- list --offset 0 --limit 10
```

The first circle opened through it, and one deployed by hand before the
factory existed:

```
circle #1    CBV6IG53JF7WIINUBJS27YUCPCWUJM5EUSOWQKH7FKQLENKUR6Y2MA6Y
by hand      CDIDPDIPE7BWHLGDY32I6JUIJLATMHUYL4ZHK7BFLFR3EXEM7QRAYRLI
terms        3 members, 1 XLM per round, 5-minute rounds
```

Read one without a wallet, an account or a fee:

```sh
stellar contract invoke --id CBV6IG53JF7WIINUBJS27YUCPCWUJM5EUSOWQKH7FKQLENKUR6Y2MA6Y \
  --network testnet --source <your-key> -- get_state
```

## The problem

A fixed group agrees on an amount and a period. Every period each member pays
in, and one member - a different one each time - takes the whole pot. After N
periods everyone has paid N times and received once. Nobody earns interest.
What it buys is a **lump sum today instead of in N months**, with no bank, no
credit check, and no paperwork. Hundreds of millions of people save this way.

It fails in exactly one way, and it fails this way constantly: a member takes
their payout and stops contributing. Everyone who has not yet received is now
short, and the only enforcement available is social. That single failure is why
these groups stay small, stay informal, and stay restricted to people who
already know each other. It caps the mechanism at the size of your address book.

## What this contract changes

Joining requires locking a **security deposit** of one contribution.

When a member misses a round, the shortfall is drawn from their own deposit
automatically. The recipient is paid **in full and on time** regardless of who
defaulted. The defaulter must restore their deposit before the circle will take
them as current again, and the miss is recorded permanently in a public counter.

Absconding stops being everybody else's problem and becomes only the
absconder's. That is the whole idea: a circle you can join with people you have
never met, because the contract holds the collateral that trust used to.

If a member misses a round *and* their deposit is already spent, the contract
does not invent money. The pot is genuinely short, the recipient receives less,
and the member is flagged delinquent. Reporting that honestly matters more than
appearing to guarantee something the contract cannot.

## How a circle runs

```
create --> Forming --(fills)--> Active --(N rounds)--> Complete
              |                    |                       |
           join()              contribute()        withdraw_deposit()
      locks a deposit         settle() pays
                             round N's member
```

1. **`join`** - locks one contribution as a deposit. Returns your position,
   which is also the round you get paid. Payout order is join order, published
   before anyone commits money, so you know your turn before you pay anything.
2. **`contribute`** - pay into the open round.
3. **`settle`** - closes the round: covers any misses from the defaulters'
   deposits, pays that round's recipient, opens the next round. **Permissionless
   by design** - the recipient, another member, or an unrelated keeper can call
   it, because the outcome is fixed by the contract and does not depend on who
   asks. No payout can be held hostage by an absent administrator.
4. **`top_up`** - restore a deposit that covered a miss.
5. **`withdraw_deposit`** - reclaim your deposit once the circle completes.

`settle` may be called early the moment everyone has paid, so an on-time circle
never waits for a clock.

## Contract API

| Function | Auth | Returns |
|---|---|---|
| `join(member)` | member | position in the circle |
| `contribute(member)` | member | - |
| `top_up(member)` | member | amount restored |
| `settle()` | none | the address paid |
| `withdraw_deposit(member)` | member | amount returned |
| `get_config()` / `get_state()` / `get_member(addr)` | none | view |
| `has_contributed(round, addr)` / `recipient_of(round)` | none | view |

Money is any SEP-41 token - a Stellar Asset Contract for USDC, or a local
currency issued by an anchor, which is what makes this usable where the savings
practice actually lives.

## Parallel execution under CAP-0063

Storage keys are chosen so that contributions do not serialise against each
other. `contribute` writes only `Paid(round, member)` and `Member(member)` -
both parameterised by the caller - so every member of a circle can pay in the
same ledger without their transactions clustering together.

The contract deliberately does **not** keep a `paid_this_round` counter. A tally
like that is the obvious way to write it, and it would make every single
contribution write the same ledger entry, serialising the entire group behind
one number. `settle` reconstructs the count by reading each member's flag
instead. `settle` does write shared state, but it runs once per round, where
serialising is correct anyway.

This was verified with [Braid](https://github.com/soroban-toolworks/braid), a
static analyser for exactly this class of bug.

## Deploy

```sh
./scripts/deploy.sh 3 10000000 300      # capacity, contribution, round seconds
```

Prints the circle contract id, which is what the app and the SDK take.

## Build and test

```sh
cargo test                                        # 25 tests
cargo clippy --all-targets -- -D warnings
cargo build --target wasm32v1-none --release
```

Soroban rejects the stock `wasm32-unknown-unknown` target on Rust 1.82+, which
enables reference-types and multivalue; `wasm32v1-none` is the target to build
for.

The suite covers the full lifecycle, both default paths, the authorisation
boundary, and an accounting invariant asserting that after a complete cycle
every member is square to the stroop and the contract holds nothing.

### Property-based testing

In addition to the hand-written scenarios, `contracts/circle/src/proptest_harness.rs`
runs a property-based harness using [proptest](https://docs.rs/proptest). It
generates random circles — varying member counts (3–24), contribution sizes
(1–1 000 000 stroops), round durations (1 s – 30 days) — and executes full
lifecycles with random per-member, per-round actions:

| Action | Meaning |
|--------|---------|
| `Pay` | Member contributes on time |
| `Default` | Member skips; deposit covers the gap (or is exhausted) |
| `DefaultThenTopUp` | Member skips then calls `top_up` to restore the deposit |

After every run the harness asserts four invariants:

| ID | Invariant |
|----|-----------|
| I-1 | `status == Complete` after the last round |
| I-2 | Every member's `received` flag is `true` |
| I-3 | Contract token balance is `0` after all withdrawals |
| I-4 | Token conservation: `Σ balance_after == Σ balance_before_join` |

Counterexamples are shrunk automatically by proptest to the smallest failing
input. If the harness finds a bug, the output names the violated invariant and
the exact `CircleParams` and action matrix that triggered it.

```sh
cargo test -p circlefi-circle          # runs scenario tests + property tests
PROPTEST_CASES=1000 cargo test         # run more cases for a deeper search
```

## Licence

Apache-2.0.
