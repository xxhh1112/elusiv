use crate::macros::{ ElusivAccount, remove_original_implementation };
use solana_program::entrypoint::ProgramResult;
use solana_program::program_error::ProgramError;
use crate::types::U256;
use crate::bytes::{ not_contains, contains };
use crate::error::ElusivError::{
    CommitmentAlreadyExists,
    NoRoomForCommitment,
    InvalidMerkleTreeAccess,
    InvalidMerkleRoot,
    NullifierAccountDoesNotExist,
};
use super::NullifierAccount;
use crate::macros::guard;

pub const TREE_HEIGHT: usize = 16;
pub const TREE_SIZE: usize = 1 << (TREE_HEIGHT + 1);
pub const TREE_COMMITMENT_START: usize = (1 << TREE_HEIGHT) - 1;
pub const TREE_COMMITMENT_COUNT: usize = 1 << TREE_HEIGHT;
const HISTORY_ARRAY_COUNT: usize = 10;

#[derive(ElusivAccount)]
#[remove_original_implementation]
struct StorageAccount {
    merkle_tree: [U256; TREE_SIZE],
    next_commitment: u64,
    root_history: [U256; HISTORY_ARRAY_COUNT],
    nullifier_account: Option<U256>,
}

impl<'a> StorageAccount<'a> {
    crate::macros::pubkey!("CYFkyPAmHjayCwhRS6LpQjY2E7atNeLS3b8FE1HTYQY4");

    pub fn reset(&mut self) {
        for i in 0..self.merkle_tree.len() {
            self.merkle_tree[i] = 0;
        }

        self.set_next_commitment(0);

        for i in 0..self.root_history.len() {
            self.root_history[i] = 0;
        }

        self.set_nullifier_account(None);
    }
}

// Commitments
impl<'a> StorageAccount<'a> {

    pub fn can_insert_commitment(&self, commitment: U256) -> ProgramResult {
        // Room for commitment
        guard!(
            self.get_next_commitment() < TREE_LEAF_COUNT as u64,
            NoRoomForCommitment
        );

        // Check for duplicate
        let tree_leaves = &self.merkle_tree[TREE_LEAF_START..(TREE_LEAF_START + self.get_next_commitment() as usize)];
        guard!(
            not_contains(commitment, tree_leaves),
            CommitmentAlreadyExists
        );

        Ok(())
    }

    /// Inserts commitment and the above hashes
    pub fn insert_commitment(&mut self, values: [U256; TREE_HEIGHT + 1]) -> ProgramResult {
        let leaf_index = self.get_next_commitment() as usize;

        // Additional commitment security check
        let commitment = values[0];
        self.can_insert_commitment(commitment)?;

        // Save last root
        let root = self.get_tree_node(0, 0)?;
        self.set_root_history(leaf_index % HISTORY_ARRAY_COUNT, &root);

        // Insert values into the tree
        for (i, &value) in values.iter().enumerate() {
            let layer = TREE_HEIGHT - i;
            let index = leaf_index >> i;
            self.set_tree_node(layer, index, value)?;
        }

        // Increment pointer
        self.set_next_commitment(leaf_index as u64 + 1);

        Ok(())
    }
}

// Root
impl<'a> StorageAccount<'a> {
    pub fn is_root_valid(
        &self,
        nullifier_account: &NullifierAccount,
        root: U256
    ) -> ProgramResult {
        // TODO: Check that two different roots are from different trees/nullifier accounts
        
        // If nullifier account is active, just check storage account roots
        match self.get_nullifier_account() {
            Some(active_nullifier_account) => {
                if nullifier_account.get_key() == active_nullifier_account {
                    // Checks for root equality with tree root
                    if contains(root, &self.merkle_tree[..32]) {
                        return Ok(());
                    }

                    // Checks for root in root history
                    if contains(root, self.root_history) {
                        return Ok(());
                    }
                }
            },
            None => {}
        }

        // Archived nullifier account
        guard!(
            nullifier_account.get_root() == root,
            InvalidMerkleRoot
        );

        Ok(())
    }
}

macro_rules! assert_valid_tree_access {
    ($layer: expr, $index: expr) => {
        guard!(
            $layer <= TREE_HEIGHT && $index <= size_of_tree_layer($layer),
            InvalidMerkleTreeAccess
        );
    };
}

// Merkle tree
// - `layer` 0 is the root
impl<'a> StorageAccount<'a> {
    #[allow(unused_comparisons)]
    pub fn get_tree_opening(&self, index: usize) -> Result<[U256; TREE_HEIGHT], ProgramError> {
        assert_valid_tree_access!(0, index);

        let mut opening = [[0; 32]; TREE_HEIGHT];
        let mut index = index;

        for i in 0..TREE_HEIGHT {
            let layer = TREE_HEIGHT - i;
            let n_index = if index % 2 == 0 { index + 1 } else { index - 1};
            opening[i] = self.get_tree_node(layer, n_index)?;
            index = index >> 1;
        }

        Ok(opening)
    }

    pub fn get_tree_node(&self, layer: usize, index: usize) -> Result<U256, ProgramError> {
        assert_valid_tree_access!(layer, index);

        Ok(self.get_merkle_tree(tree_array_index(layer, index)))
    }

    pub fn set_tree_node(&mut self, layer: usize, index: usize, value: U256) -> Result<(), ProgramError> {
        assert_valid_tree_access!(layer, index);

        self.set_merkle_tree(tree_array_index(layer, index), &value);

        Ok(())
    }
}

// Nullifiers
impl<'a> StorageAccount<'a> {
    pub fn try_get_nullifier_account(&self) -> Result<U256, ProgramError> {
        match self.get_nullifier_account() {
            Some(nullifier_account) => Ok(nullifier_account),
            None => Err(NullifierAccountDoesNotExist.into())
        }
    }
}

pub fn tree_array_index(layer: usize, index: usize) -> usize {
    (1 << layer) - 1 + index
}

fn size_of_tree_layer(layer: usize) -> usize {
    1 << layer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_correct_size() {
        let mut data = vec![0; StorageAccount::TOTAL_SIZE];
        StorageAccount::from_data(&mut data).unwrap();
    }

    #[test]
    #[should_panic]
    fn test_invalid_size() {
        let mut data = vec![0; StorageAccount::TOTAL_SIZE - 1];
        StorageAccount::from_data(&mut data).unwrap();
    }
}