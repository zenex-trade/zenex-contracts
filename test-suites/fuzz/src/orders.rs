//! Order-id bookkeeping for the fill and cancel commands.
//!
//! Trade and vault orders draw ids from one per-user counter, and a filled or
//! cancelled order keeps its id. A pick over the whole counter lands mostly on
//! a consumed order or one of the other kind, so the call spends its step on
//! `OrderNotFound` or `VaultOrderNotFound`. Each target records the ids of the
//! kind it fills, and [`Orders::pick`] aims at a resting one. One pick in
//! [`ANY_ID_ONE_IN`] still takes any allocated id, which keeps the not-found
//! path and the single-consumption check covered.

use soroban_sdk::Address;
use std::collections::HashSet;
use std::vec::Vec;

use crate::fixture::{Fixture, USER_COUNT};

/// One pick in this many takes any allocated id.
pub const ANY_ID_ONE_IN: u8 = 8;

/// One kind of order: the ids a target created per user, and the ids a
/// landed fill or cancel consumed.
pub struct Orders {
    created: Vec<Vec<u32>>,
    consumed: HashSet<(usize, u32)>,
}

impl Default for Orders {
    fn default() -> Self {
        Orders {
            created: std::vec![Vec::new(); USER_COUNT],
            consumed: HashSet::new(),
        }
    }
}

impl Orders {
    /// Records an id a create call returned for user `idx`. Id 0 is a
    /// retired-market instant redeem, which never rests.
    pub fn created(&mut self, idx: usize, id: u32) {
        if id != 0 {
            self.created[idx % USER_COUNT].push(id);
        }
    }

    /// Maps `byte` onto the `(user, id)` a fill or cancel targets.
    ///
    /// It takes one of user `idx`'s orders that `rests` reports open, or
    /// another user's when `idx` has none, since a keeper fills any user's
    /// order. With no order resting, or when the byte picks that path, it
    /// takes any id user `idx` was allocated.
    pub fn pick(
        &self,
        f: &Fixture,
        idx: usize,
        byte: u8,
        rests: impl Fn(usize, u32) -> bool,
    ) -> (usize, u32) {
        let idx = idx % USER_COUNT;
        if byte % ANY_ID_ONE_IN != 0 {
            for offset in 0..USER_COUNT {
                let owner = (idx + offset) % USER_COUNT;
                let resting: Vec<u32> = self.created[owner]
                    .iter()
                    .copied()
                    .filter(|&id| rests(owner, id))
                    .collect();
                if !resting.is_empty() {
                    return (
                        owner,
                        resting[(byte / ANY_ID_ONE_IN) as usize % resting.len()],
                    );
                }
            }
        }
        (idx, any_allocated(f, &f.user(idx), byte))
    }

    /// Records a fill or cancel on user `idx`'s order `id`.
    ///
    /// A landed call removes the order and pays out its escrow, so the id is
    /// spent, and a second landing would release the same escrow twice. The
    /// caller has already checked the error code. This asserts only that a
    /// spent id never lands again.
    pub fn record(&mut self, idx: usize, id: u32, landed: bool, context: &str) {
        if landed {
            assert!(
                self.consumed.insert((idx % USER_COUNT, id)),
                "[{context}] order {id} for user {idx} was consumed twice"
            );
        }
    }
}

/// Maps `byte` onto an id `user` has been allocated, of either kind and
/// possibly consumed. Ids run `1..counter`. An empty counter yields `1`, the
/// not-found path.
pub fn any_allocated(f: &Fixture, user: &Address, byte: u8) -> u32 {
    let counter = f.market.get_order_counter(user);
    if counter <= 1 {
        return 1;
    }
    1 + (byte as u32) % (counter - 1)
}
