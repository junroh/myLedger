//! Shared budget groups: several holds draw on one budget and must be resolved together. A
//! property of holds, outliving the request that created them, with membership held by the pending
//! engine. Not [`crate::rules::linked`], which is atomicity within one submission.

use ledger_base::ports::HoldView;
use ledger_base::{Amount, BudgetGroup, LedgerError};

/// One rule: a resolution moves the whole group. A group of several members therefore needs a
/// chain, and no member may move part way.
pub struct BudgetRules;

impl BudgetRules {
    /// Checked for every settle and void.
    pub fn allow_resolution(
        hold: &HoldView,
        amount: Amount,
        in_chain: bool,
    ) -> Result<(), LedgerError> {
        if hold.budget.is_absent() {
            return Ok(());
        }
        // One hold cannot cover a group of several.
        if hold.budget_members > 1 && !in_chain {
            return Err(LedgerError::SharedBudgetGroupRequired);
        }
        // Repeated partial settlement is a single-hold affair.
        if amount != hold.remaining {
            return Err(LedgerError::PartialResolutionNotAllowed);
        }
        Ok(())
    }
}

/// What a chain resolves of each budget group, against what the group holds. A chain that leaves a
/// member out, or moves part of one, is refused whole.
pub struct BudgetCoverage {
    tallies: Vec<Tally>,
}

struct Tally {
    budget: BudgetGroup,
    legs: u32,
    amount: Amount,
    members: u32,
    remaining: Amount,
}

impl BudgetCoverage {
    pub fn new(capacity: usize) -> Self {
        Self {
            tallies: Vec::with_capacity(capacity),
        }
    }

    pub fn clear(&mut self) {
        self.tallies.clear();
    }

    /// Records one resolved leg, with the group as the engine reported it.
    pub fn note(&mut self, budget: BudgetGroup, amount: Amount, members: u32, remaining: Amount) {
        if let Some(tally) = self.tallies.iter_mut().find(|tally| tally.budget == budget) {
            tally.legs += 1;
            tally.amount += amount;
            return;
        }
        self.tallies.push(Tally {
            budget,
            legs: 1,
            amount,
            members,
            remaining,
        });
    }

    pub fn misses_a_member(&self) -> bool {
        self.tallies
            .iter()
            .any(|tally| tally.legs != tally.members || tally.amount != tally.remaining)
    }
}

#[cfg(test)]
mod tests {
    use ledger_base::AccountId;

    use super::*;

    fn hold(members: u32, remaining: Amount) -> HoldView {
        HoldView {
            debit_account: AccountId(10),
            credit_account: AccountId(11),
            ledger: 1,
            budget: BudgetGroup(if members == 0 { 0 } else { 7 }),
            budget_members: members,
            budget_remaining: remaining,
            remaining,
            resolved: false,
        }
    }

    /// A hold in a budget group of several can only be resolved inside a chain, and only in full. A
    /// hold with no budget keeps the ordinary freedom to be settled part way.
    #[test]
    fn a_member_of_a_budget_group_may_only_move_whole_and_with_the_others() {
        let alone = hold(3, 100);
        assert_eq!(
            BudgetRules::allow_resolution(&alone, 100, false),
            Err(LedgerError::SharedBudgetGroupRequired)
        );
        assert_eq!(
            BudgetRules::allow_resolution(&alone, 40, true),
            Err(LedgerError::PartialResolutionNotAllowed)
        );
        assert_eq!(BudgetRules::allow_resolution(&alone, 100, true), Ok(()));

        let unbudgeted = hold(0, 100);
        assert_eq!(
            BudgetRules::allow_resolution(&unbudgeted, 40, false),
            Ok(())
        );
    }

    /// Coverage is complete only when every member of the group is resolved for its whole
    /// remainder; a short chain must be caught before it is proposed.
    #[test]
    fn coverage_is_incomplete_until_every_member_is_resolved_in_full() {
        let group = BudgetGroup(7);
        let mut coverage = BudgetCoverage::new(4);
        coverage.note(group, 30, 2, 100);
        assert!(coverage.misses_a_member(), "one leg of two");

        coverage.note(group, 60, 2, 100);
        assert!(coverage.misses_a_member(), "both legs, but 90 of 100");

        coverage.clear();
        coverage.note(group, 30, 2, 100);
        coverage.note(group, 70, 2, 100);
        assert!(!coverage.misses_a_member());
    }
}
