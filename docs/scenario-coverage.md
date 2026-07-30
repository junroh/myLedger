# Scenario coverage

Checked against the usage scenarios document. The pending engine is still a memory stub, so
the question for it is only whether the interface carries what the scenario needs.

Each supported scenario has a test named after the behaviour in the evidence column.

| Scenario | State | Evidence |
|---|---|---|
| 0. Single hold, debit | supported | all four kinds move the columns the design specifies; a settle over the remaining hold is refused |
| 4. Single hold, credit | supported | an incoming hold is not spendable until it settles |
| 1. Shared budget group | supported | a group of three resolves as one unit, and every partial or short resolution is refused |
| 2. Linked transfers | supported | a later leg spends what an earlier leg brings in; a chain resolves a hold it created itself; one failing leg rolls back the chain; an unterminated chain is rejected at the batch boundary |
| 3. Delayed settlement and timeout | sequencer side supported | the void path is the same as scenario 0; nothing expires a hold yet |
| A. Auth-less direct settlement | supported | single-phase transfers never touch the pending path |
| 5, 6, 7 (conditional) | not implemented | they depend on open questions that are still open |

## What the scenarios required and the code did not do

**A member of a shared budget group may not move part way.** The scenario document allows
repeated partial settlement for a single hold but requires an atomic whole-group transition
for groups, with no partial leg. The sequencer only refused *individual* resolution; a partial
amount inside a chain went through. It now refuses any resolution of a budget-group hold whose
amount is not the whole remaining (`PartialResolutionNotAllowed`).

**A pending credit is a hold like any other.** Scenario 4 needs no separate mechanism: every
transfer has both directions, so incoming funds are a hold whose credit side is the receiving
account. Held credit stays out of availability, which is the conservative rule, so the money
is visible but unspendable until it settles.

## The shared budget group, end to end

The client names the group and the store owns it:

- A hold **declares** the group it joins in `pending_ref`, which is unused on a hold. The
  convention is to name the group after its first hold. No wire field was added and the transfer
  stays 64 bytes.
- The pending engine **indexes** holds by group and reports the group with every lookup: how many
  members it has and how much they hold together.
- The sequencer **enforces one rule**: a resolution must move the whole group. A group of one can
  therefore be resolved on its own; a group of three needs a chain covering all three, each for
  its whole remainder. Partial amounts (`PartialResolutionNotAllowed`), missing members
  (`SharedBudgetGroupIncomplete`) and lone legs of a larger group
  (`SharedBudgetGroupRequired`) are all refused.

One rule rather than two matters: an earlier version required a chain *and* checked coverage,
which made a single-member group impossible to resolve at all.

The cost is 16 bytes in every replicated effect (the group id is a transaction id), so `Effect`
grew from 96 to 112 bytes. That is recorded in the layout budget.

## Accepted limits

**Expiry belongs to the pending engine.** The sequencer accepts an inbound void, which is its
whole part in scenario 3. How long a hold may live, detecting that it expired and submitting the
voids is the pending engine's work — none of it is built — and keeping a mass-expiry sweep behind
live traffic is the rate limiter's.

## Consistency is the sequencer's, not the pending engine's

Scenario section 0.3 splits consistency checks in two and gives the pending engine "does the hold
exist" and "does the settle amount match the original". That is superseded: the pending engine
provides data and judges nothing, and every judgment — existence, remaining amount, accounts,
ledger, the group rule — is the sequencer's. The scenario document's wording should follow.
