use crate::token::{TokenAuthorityAccount, SPL_TOKEN_COUNT};
use crate::{macros::elusiv_account, types::U256};
use crate::bytes::ElusivOption;
use super::{program_account::PDAAccountData, fee::ProgramFee};

#[elusiv_account(eager_type: true)]
pub struct GovernorAccount {
    pda_data: PDAAccountData,

    /// The current fee-version (new requests are forced to use this version)
    pub fee_version: u32,

    /// The `ProgramFee` for the `FeeAccount` with the offset `fee_version`
    pub program_fee: ProgramFee,

    /// The number of commitments in a MT-root hashing batch
    pub commitment_batching_rate: u32,

    program_version: u32,
}

macro_rules! impl_token_authority {
    ($ty: ident) => {
        impl<'a> TokenAuthorityAccount for $ty<'a> {
            unsafe fn get_token_account_unchecked(&self, token_id: u16) -> Option<U256> {
                if token_id == 0 {
                    return None
                }

                self.get_accounts(token_id as usize - 1).option()
            }

            unsafe fn set_token_account_unchecked(&mut self, token_id: u16, key: &solana_program::pubkey::Pubkey) {
                if token_id == 0 {
                    return
                }

                self.set_accounts(token_id as usize - 1, &ElusivOption::Some(key.to_bytes()));
            }
        }
    };
}

impl_token_authority!(PoolAccount);
impl_token_authority!(FeeCollectorAccount);

#[elusiv_account(eager_type: true)]
pub struct PoolAccount {
    pda_data: PDAAccountData,
    pub accounts: [ElusivOption<U256>; SPL_TOKEN_COUNT],
}

#[elusiv_account(eager_type: true)]
pub struct FeeCollectorAccount {
    pda_data: PDAAccountData,
    pub accounts: [ElusivOption<U256>; SPL_TOKEN_COUNT],
}