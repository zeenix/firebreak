//! Choosing which vouchers pay an amount.
//!
//! A voucher pays out whole, so a payment is an exact subset of the spendable vouchers: no change
//! can be made. [`exact_subset`] finds the subset the delegate should spend, and finds the same one
//! every time for the same vouchers.

/// The most vouchers [`exact_subset`] searches through.
///
/// One funding transaction makes at most this many outputs, so an allowance never holds more
/// vouchers than this.
pub const MAX_VOUCHERS: usize = flamepayments::MAX_OUTPUTS;

/// The vouchers that pay exactly `amount`, chosen from `available` as `(id, quantity)` pairs.
///
/// The fewest vouchers win. Among equally few, the choice favors the larger quantities first and
/// then the smaller ids, so the same vouchers always give the same answer, whatever order
/// `available` lists them in. The chosen ids come back in that same order.
///
/// The search tries every subset size in turn, which is exponential in the number of vouchers.
/// That is cheap up to [`MAX_VOUCHERS`] (65,536 subsets), and more candidates than that are refused
/// instead of searched. The ids must be distinct.
pub fn exact_subset<I>(available: &[(I, u64)], amount: u64) -> Result<Vec<I>, SelectError>
where
    I: Clone + Ord,
{
    if amount == 0 {
        return Err(SelectError::ZeroAmount);
    }
    if available.len() > MAX_VOUCHERS {
        return Err(SelectError::TooManyVouchers {
            count: available.len(),
            max: MAX_VOUCHERS,
        });
    }

    let mut ordered: Vec<&(I, u64)> = available.iter().collect();
    ordered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let quantities: Vec<u64> = ordered.iter().map(|(_, quantity)| *quantity).collect();

    let total: u128 = quantities
        .iter()
        .map(|quantity| u128::from(*quantity))
        .sum();
    if u128::from(amount) > total {
        return Err(SelectError::InsufficientAuthority {
            requested: amount,
            available: u64::try_from(total).unwrap_or(u64::MAX),
        });
    }

    for size in 1..=quantities.len() {
        let mut chosen = Vec::with_capacity(size);
        if fill(&quantities, 0, amount, size, &mut chosen) {
            return Ok(chosen
                .into_iter()
                .map(|position| ordered[position].0.clone())
                .collect());
        }
    }
    Err(SelectError::NoExactCombination {
        requested: amount,
        denominations: quantities,
    })
}

/// Why no vouchers could be chosen.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SelectError {
    /// The amount was zero, which no voucher pays.
    #[error("the amount must be greater than zero")]
    ZeroAmount,

    /// The vouchers together are worth less than the amount.
    #[error("insufficient authority")]
    InsufficientAuthority {
        /// The amount asked for.
        requested: u64,
        /// What all the candidate vouchers are worth together.
        available: u64,
    },

    /// The vouchers together cover the amount, but no subset of them adds up to exactly it.
    #[error(
        "no exact combination of vouchers for {requested} (denominations: {})",
        list(.denominations)
    )]
    NoExactCombination {
        /// The amount asked for.
        requested: u64,
        /// The candidate vouchers' quantities, largest first.
        denominations: Vec<u64>,
    },

    /// More candidates than [`MAX_VOUCHERS`], which an allowance never holds.
    #[error("{count} vouchers are too many to choose from; an allowance holds at most {max}")]
    TooManyVouchers {
        /// How many candidates were offered.
        count: usize,
        /// The most that are searched.
        max: usize,
    },
}

/// Whether `size` of the `quantities` from `start` on, in order, add up to `left`, leaving the
/// positions it picked in `chosen`.
///
/// It tries positions in increasing order, so the first subset it finds is the smallest in that
/// order: the one that favors the quantities listed first.
fn fill(quantities: &[u64], start: usize, left: u64, size: usize, chosen: &mut Vec<usize>) -> bool {
    if size == 0 {
        return left == 0;
    }
    // The last position that still leaves room for the rest of the subset after it.
    let last = quantities.len() - size;
    for position in start..=last {
        let quantity = quantities[position];
        if quantity > left {
            continue;
        }
        chosen.push(position);
        if fill(quantities, position + 1, left - quantity, size - 1, chosen) {
            return true;
        }
        chosen.pop();
    }
    false
}

/// The quantities as a comma-separated list.
fn list(quantities: &[u64]) -> String {
    let words: Vec<String> = quantities.iter().map(u64::to_string).collect();
    words.join(", ")
}

#[cfg(test)]
mod tests {
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    use super::*;

    /// The allowance of the demo: 50, 20, 20 and 10, under ids that sort in that order.
    fn allowance() -> Vec<(u8, u64)> {
        vec![(1, 50), (2, 20), (3, 20), (4, 10)]
    }

    #[test]
    fn the_fewest_vouchers_pay_the_amount() {
        assert_eq!(exact_subset(&allowance(), 50), Ok(vec![1]));
        assert_eq!(exact_subset(&allowance(), 60), Ok(vec![1, 4]));
        assert_eq!(exact_subset(&allowance(), 40), Ok(vec![2, 3]));
        assert_eq!(exact_subset(&allowance(), 80), Ok(vec![1, 2, 4]));
        assert_eq!(exact_subset(&allowance(), 100), Ok(vec![1, 2, 3, 4]));
    }

    #[test]
    fn larger_vouchers_come_first_among_equally_few() {
        // 30 + 10 and 20 + 20 both make 40 with two vouchers; the larger first voucher wins.
        let available = [(1, 30), (2, 20), (3, 20), (4, 10)];
        assert_eq!(exact_subset(&available, 40), Ok(vec![1, 4]));
    }

    #[test]
    fn equal_quantities_are_chosen_by_id() {
        let available = [(9, 20), (3, 20), (7, 20)];
        assert_eq!(exact_subset(&available, 20), Ok(vec![3]));
        assert_eq!(exact_subset(&available, 40), Ok(vec![3, 7]));
    }

    #[test]
    fn the_choice_does_not_depend_on_the_order_vouchers_are_listed() {
        let mut available = allowance();
        let expected = exact_subset(&available, 60);
        available.reverse();
        assert_eq!(exact_subset(&available, 60), expected);
        available.swap(0, 2);
        assert_eq!(exact_subset(&available, 60), expected);
    }

    #[test]
    fn ids_can_be_any_ordered_type() {
        let first = [1u8; 32];
        let second = [2u8; 32];
        let available = [(second, 10), (first, 10)];
        assert_eq!(exact_subset(&available, 10), Ok(vec![first]));
    }

    #[test]
    fn more_than_all_the_vouchers_are_worth_is_insufficient_authority() {
        assert_eq!(
            exact_subset(&allowance(), 101),
            Err(SelectError::InsufficientAuthority {
                requested: 101,
                available: 100
            })
        );
        assert_eq!(
            exact_subset::<u8>(&[], 1),
            Err(SelectError::InsufficientAuthority {
                requested: 1,
                available: 0
            })
        );
    }

    #[test]
    fn an_amount_no_subset_makes_lists_the_denominations() {
        let error = exact_subset(&allowance(), 65).expect_err("no subset makes 65");
        assert_eq!(
            error,
            SelectError::NoExactCombination {
                requested: 65,
                denominations: vec![50, 20, 20, 10]
            }
        );
        assert_eq!(
            error.to_string(),
            "no exact combination of vouchers for 65 (denominations: 50, 20, 20, 10)"
        );
    }

    #[test]
    fn insufficient_authority_reads_as_the_agent_reports_it() {
        let error = exact_subset(&allowance(), 500).expect_err("more than the allowance");
        assert_eq!(error.to_string(), "insufficient authority");
    }

    #[test]
    fn a_zero_amount_is_refused() {
        assert_eq!(exact_subset(&allowance(), 0), Err(SelectError::ZeroAmount));
        assert_eq!(exact_subset::<u8>(&[], 0), Err(SelectError::ZeroAmount));
    }

    #[test]
    fn sums_beyond_the_largest_integer_do_not_overflow() {
        let available = [(1u8, u64::MAX), (2, u64::MAX)];
        assert_eq!(exact_subset(&available, u64::MAX), Ok(vec![1]));
        // 1 and u64::MAX make 1, u64::MAX and u64::MAX + 1, so u64::MAX - 1 is out of reach.
        let error = exact_subset(&[(1u8, 1), (2, u64::MAX)], u64::MAX - 1).expect_err("no subset");
        assert!(matches!(error, SelectError::NoExactCombination { .. }));
    }

    #[test]
    fn more_than_the_maximum_vouchers_are_refused() {
        let at_most: Vec<(usize, u64)> = (0..MAX_VOUCHERS).map(|id| (id, 1)).collect();
        assert_eq!(exact_subset(&at_most, 3), Ok(vec![0, 1, 2]));

        let too_many: Vec<(usize, u64)> = (0..=MAX_VOUCHERS).map(|id| (id, 1)).collect();
        assert_eq!(
            exact_subset(&too_many, 3),
            Err(SelectError::TooManyVouchers {
                count: MAX_VOUCHERS + 1,
                max: MAX_VOUCHERS
            })
        );
    }

    #[test]
    fn the_search_agrees_with_trying_every_subset() {
        let mut rng = StdRng::seed_from_u64(11);
        for _ in 0..200 {
            let count = rng.gen_range(1..=8usize);
            let available: Vec<(usize, u64)> =
                (0..count).map(|id| (id, rng.gen_range(1..=9u64))).collect();
            let amount = rng.gen_range(1..=40u64);

            // Every subset that pays the amount, by size: the answer must be one of the smallest.
            let mut smallest: Option<usize> = None;
            for mask in 1u32..(1 << count) {
                let picked: Vec<u64> = (0..count)
                    .filter(|&position| mask & (1u32 << position) != 0)
                    .map(|position| available[position].1)
                    .collect();
                if picked.iter().sum::<u64>() == amount {
                    smallest = Some(smallest.map_or(picked.len(), |size| size.min(picked.len())));
                }
            }

            match (exact_subset(&available, amount), smallest) {
                (Ok(chosen), Some(size)) => {
                    assert_eq!(chosen.len(), size, "{available:?} for {amount}");
                    let paid: u64 = chosen.iter().map(|id| available[*id].1).sum();
                    assert_eq!(paid, amount, "{available:?} for {amount}");
                }
                (Err(_), None) => {}
                (answer, smallest) => {
                    panic!("{available:?} for {amount}: {answer:?}, smallest {smallest:?}")
                }
            }
        }
    }
}
