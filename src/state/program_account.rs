//! Traits used to represent types of accounts, owned by the program

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::account_info::AccountInfo;
use solana_program::program_error::ProgramError;
use solana_program::pubkey::Pubkey;
use crate::macros::BorshSerDeSized;
use crate::bytes::BorshSerDeSized;
use crate::types::U256;

pub trait SizedAccount {
    const SIZE: usize;
}

pub trait ProgramAccount<'a>: SizedAccount {
    type T: SizedAccount;

    fn new(d: &'a mut [u8]) -> Result<Self::T, ProgramError>;
}

pub trait MultiAccountProgramAccount<'a, 'b, 't>: SizedAccount {
    type T: SizedAccount;

    fn new(
        d: &'a mut [u8],
        accounts: std::collections::HashMap<usize, &'b AccountInfo<'t>>,
    ) -> Result<Self::T, ProgramError>;
}

/// This trait is used by the `elusiv_instruction` and `elusiv_accounts` macros
/// - a PDAAccount is simply a PDA with:
///     1. the leading fields specified by `PDAAccountFields`
///     2. a PDA that is derived using the following seed: `&[ &SEED, offset?, bump ]`
/// - so there are two kinds of PDAAccounts:
///     - single instance: the pda_offset is `None` -> `&[ &SEED, bump ]`
///     - multi instance: the pda_offset is `Some(offset)` -> `&[ &SEED, offset, bump ]`
pub trait PDAAccount {
    const SEED: &'static [u8];

    fn find(offset: Option<u64>) -> (Pubkey, u8) {
        let seed = Self::offset_seed(offset);
        let seed: Vec<&[u8]> = seed.iter().map(|x| &x[..]).collect();

        Pubkey::find_program_address(&seed, &crate::id())
    }

    fn pubkey(offset: Option<u64>, bump: u8) -> Result<Pubkey, ProgramError> {
        let mut seed = Self::offset_seed(offset);
        seed.push(vec![bump]);
        let seed: Vec<&[u8]> = seed.iter().map(|x| &x[..]).collect();

        match Pubkey::create_program_address(&seed, &crate::id()) {
            Ok(v) => Ok(v),
            Err(_) => Err(ProgramError::InvalidSeeds)
        }
    }

    fn offset_seed(offset: Option<u64>) -> Vec<Vec<u8>> {
        match offset {
            Some(offset) => vec![Self::SEED.to_vec(), offset.to_le_bytes().to_vec()],
            None => vec![Self::SEED.to_vec()]
        }
    }

    fn is_valid_pubkey(account: &AccountInfo, offset: Option<u64>, pubkey: &Pubkey) -> Result<bool, ProgramError> {
        match PDAAccountData::new(&account.data.borrow()) {
            Ok(a) => Ok(Self::pubkey(offset, a.bump_seed)? == *pubkey),
            Err(_) => Err(ProgramError::InvalidAccountData)
        }
    }
} 

#[derive(BorshDeserialize, BorshSerialize, BorshSerDeSized)]
pub struct PDAAccountData {
    pub bump_seed: u8,

    /// Used for future account migrations
    pub version: u8,

    /// In general useless, only if an account-type uses it
    pub initialized: bool,
}

impl PDAAccountData {
    pub fn new(data: &[u8]) -> Result<Self, std::io::Error> {
        PDAAccountData::try_from_slice(&data[..Self::SIZE])
    }
}

#[derive(BorshDeserialize, BorshSerialize, BorshSerDeSized)]
pub struct MultiAccountAccountData<const COUNT: usize> {
    // ... PDAAccountData always before MultiAccountAccountData, since it's a PDA
     
    pub pubkeys: [Option<U256>; COUNT],
}

impl<const COUNT: usize> MultiAccountAccountData<COUNT> {
    pub fn new(data: &[u8]) -> Result<Self, std::io::Error> {
        MultiAccountAccountData::try_from_slice(&data[PDAAccountData::SIZE..Self::SIZE])
    }
}

/// Certain accounts, like the `VerificationAccount` can be instantiated multiple times.
/// - this allows for parallel computations/usage
/// - so we can compare this index with `MAX_INSTANCES` to check validity
pub trait MultiInstancePDAAccount: PDAAccount {
    const MAX_INSTANCES: u64;

    fn is_valid(&self, index: u64) -> bool {
        index < Self::MAX_INSTANCES
    }
}

/// 1 MiB sub-account size
/// - we don't use 10 MiB (`solana_program::system_instruction::MAX_PERMITTED_DATA_LENGTH`) for increased fetching/rent efficiency
pub const MIN_ACCOUNT_SIZE: usize = 1048576;

/// Allows for storing data across multiple accounts (needed for data sized >10 MiB)
/// - these accounts can be PDAs, but will most likely be data accounts (size > 10 KiB)
/// - by default all these accounts are assumed to have the same size = `INTERMEDIARY_ACCOUNT_SIZE`
pub trait MultiAccountAccount<'t>: PDAAccount {
    type T: BorshSerDeSized;

    /// The count of subsidiary accounts
    const COUNT: usize;

    /// The size of subsidiary accounts
    const ACCOUNT_SIZE: usize;

    fn get_account(&self, account_index: usize) -> Result<&AccountInfo<'t>, ProgramError>;

    /// Can be used to track modifications (just important for test functions)
    fn modify(&mut self, index: usize, value: Self::T);
}