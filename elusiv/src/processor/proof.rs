use std::collections::HashSet;
use solana_program::pubkey::Pubkey;
use solana_program::sysvar::instructions;
use solana_program::{
    entrypoint::ProgramResult,
    account_info::AccountInfo,
    clock::Clock,
    sysvar::Sysvar,
};
use crate::macros::{guard, BorshSerDeSized, EnumVariantIndex, pda_account};
use crate::processor::ZERO_COMMITMENT_RAW;
use crate::processor::utils::{open_pda_account_with_offset, close_account, open_pda_account, transfer_token, verify_pool, verify_fee_collector, transfer_token_from_pda, transfer_lamports_from_pda_checked, create_associated_token_account, spl_token_account_rent};
use crate::proof::precompute::PrecomputesAccount;
use crate::proof::{prepare_public_inputs_instructions, verify_partial, VerificationAccountData, VerificationState};
use crate::state::MT_COMMITMENT_COUNT;
use crate::state::governor::{FeeCollectorAccount, PoolAccount};
use crate::state::program_account::PDAAccountData;
use crate::state::queue::{CommitmentQueue, CommitmentQueueAccount, Queue, RingQueue};
use crate::state::{
    NullifierAccount,
    StorageAccount,
    program_account::ProgramAccount,
    governor::GovernorAccount,
};
use crate::error::ElusivError::{
    InvalidAmount,
    InvalidAccount,
    InvalidAccountState,
    InvalidMerkleRoot,
    InvalidPublicInputs,
    InvalidInstructionData,
    ComputationIsAlreadyFinished,
    ComputationIsNotYetFinished,
    CouldNotInsertNullifier,
    InvalidFeeVersion,
    FeatureNotAvailable,
};
use crate::proof::{
    VerificationAccount,
    vkey::{SendQuadraVKey, MigrateUnaryVKey},
};
use crate::token::{Token, verify_token_account, TokenPrice, verify_associated_token_account, Lamports, elusiv_token};
use crate::types::{Proof, SendPublicInputs, MigratePublicInputs, PublicInputs, JoinSplitPublicInputs, U256, RawU256};
use crate::bytes::{BorshSerDeSized, BorshSerDeSizedEnum, ElusivOption, usize_as_u32_safe};
use borsh::{BorshSerialize, BorshDeserialize};

use super::CommitmentHashRequest;

#[derive(BorshSerialize, BorshDeserialize, BorshSerDeSized, EnumVariantIndex, PartialEq, Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub enum ProofRequest {
    Send(SendPublicInputs),
    Merge(SendPublicInputs),
    Migrate(MigratePublicInputs),
}

macro_rules! execute_with_vkey {
    ($kind: expr, $vk: ident, $e: expr) => {
        match $kind {
            0 => { type $vk = SendQuadraVKey; $e }
            1 => { type $vk = SendQuadraVKey; $e }
            2 => { type $vk = MigrateUnaryVKey; $e }
            _ => panic!()
        }
    };
}

macro_rules! proof_request {
    ($request: expr, $public_inputs: ident, $e: expr) => {
        match $request {
            ProofRequest::Send($public_inputs) => { $e },
            ProofRequest::Merge($public_inputs) => { $e },
            ProofRequest::Migrate($public_inputs) => { $e },
        }
    };
}

impl ProofRequest {
    pub fn fee_version(&self) -> u32 {
        proof_request!(self, public_inputs, public_inputs.join_split_inputs().fee_version)
    }

    /// The amount used to compute the fee
    pub fn proof_fee_amount(&self) -> u64 {
        match self {
            ProofRequest::Send(request) => request.join_split.amount,
            _ => 0
        }
    }
}

/// We only allow two distinct MTs in a join-split (merge can be used to reduce the amount of MTs)
pub const MAX_MT_COUNT: usize = 2;

macro_rules! nullifier_duplicate_account_pda_seed {
    ($id: ident, $nullifier_hashes: expr) => {
        let nullifier_hashes: Vec<U256> = $nullifier_hashes.iter().map(|n| n.skip_mr()).collect();
        let $id = nullifier_hashes.iter().map(|n| &n[..]).collect::<Vec<&[u8]>>();
    };
}

#[allow(clippy::too_many_arguments)]
/// Initializes a new proof verification
/// - subsequent calls of `init_verification_transfer_fee` and `init_verification_proof` required to start the computation
/// - both need to be called by the same signer (-> the fee structure "enforces" `init_verification_transfer_fee` to be called in the same transaction)
pub fn init_verification<'a, 'b, 'c, 'd>(
    fee_payer: &AccountInfo<'a>,
    verification_account: &AccountInfo<'a>,
    nullifier_duplicate_account: &AccountInfo<'a>,
    recipient: &AccountInfo,
    storage_account: &StorageAccount,
    nullifier_account0: &NullifierAccount<'b, 'c, 'd>,
    nullifier_account1: &NullifierAccount<'b, 'c, 'd>,

    verification_account_index: u32,
    tree_indices: [u32; MAX_MT_COUNT],
    request: ProofRequest,
    skip_nullifier_pda: bool,
) -> ProgramResult {
    let vkey = request.variant_index();
    let raw_public_inputs = proof_request!(
        &request,
        public_inputs,
        public_inputs.public_signals()
    );
    let instructions = execute_with_vkey!(
        vkey,
        VKey,
        prepare_public_inputs_instructions::<VKey>(
            &proof_request!(
                &request,
                public_inputs,
                public_inputs.public_signals_skip_mr()
            )
        )
    );

    // Verify public inputs
    let join_split = match &request {
        ProofRequest::Send(public_inputs) => {
            guard!(public_inputs.verify_additional_constraints(), InvalidPublicInputs);
            guard!(recipient.key.to_bytes() == public_inputs.recipient.address, InvalidAccount);

            if public_inputs.recipient.is_non_associated_token_account {
                guard!(verify_token_account(recipient, public_inputs.join_split.token_id)?, InvalidAccount);
            }

            if !cfg!(test) {
                let clock = Clock::get()?;
                let current_timestamp: u64 = clock.unix_timestamp.try_into().unwrap();
                guard!(is_timestamp_valid(public_inputs.current_time, current_timestamp), InvalidInstructionData);
            }

            &public_inputs.join_split
        }
        ProofRequest::Merge(public_inputs) => {
            guard!(public_inputs.join_split.amount == 0, InvalidAmount);
            guard!(public_inputs.verify_additional_constraints(), InvalidPublicInputs);
            &public_inputs.join_split
        }
        ProofRequest::Migrate(_) => {
            // Migrate from archived MTs not implemented yet
            return Err(FeatureNotAvailable.into())
        }
    };

    check_join_split_public_inputs(
        join_split,
        storage_account,
        [nullifier_account0, nullifier_account1],
        &tree_indices,
    )?;

    // Open `nullifier_duplicate_account`
    // - this account is used to prevent two proof verifications (of the same nullifier-hashes) at the same time
    // - using `skip_nullifier_pda` a second verification can be initialized, for more details see OS-ELV-ADV-05
    nullifier_duplicate_account_pda_seed!(nullifier_hashes, join_split.nullifier_hashes);
    if skip_nullifier_pda {
        if nullifier_duplicate_account.lamports() == 0 {
            return Err(InvalidInstructionData.into())
        }
    } else {
        open_pda_account(
            &crate::id(),
            fee_payer,
            nullifier_duplicate_account,
            PDAAccountData::SIZE,
            &nullifier_hashes,
        )?;
    }

    // Open `VerificationAccount`
    open_pda_account_with_offset::<VerificationAccount>(
        &crate::id(),
        fee_payer,
        verification_account,
        verification_account_index
    )?;

    pda_account!(mut verification_account, VerificationAccount, verification_account);
    verification_account.setup(
        RawU256::new(fee_payer.key.to_bytes()),
        skip_nullifier_pda,
        &raw_public_inputs,
        &instructions,
        vkey,
        request,
        tree_indices,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn init_verification_transfer_fee<'a>(
    fee_payer: &AccountInfo<'a>,
    fee_payer_token_account: &AccountInfo<'a>,

    pool: &AccountInfo<'a>,
    pool_account: &AccountInfo<'a>,

    fee_collector: &AccountInfo<'a>,
    fee_collector_account: &AccountInfo<'a>,

    sol_usd_price_account: &AccountInfo,
    token_usd_price_account: &AccountInfo,

    governor: &GovernorAccount,
    verification_account: &mut VerificationAccount,
    token_program: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,

    _verification_account_index: u32,
) -> ProgramResult {
    guard!(matches!(verification_account.get_state(), VerificationState::None), InvalidAccountState);

    let other_data = verification_account.get_other_data();
    guard!(other_data.fee_payer.skip_mr() == fee_payer.key.to_bytes(), InvalidAccount);

    let request = verification_account.get_request();
    let join_split = proof_request!(&request, public_inputs, public_inputs.join_split_inputs());

    guard!(request.fee_version() == governor.get_fee_version(), InvalidFeeVersion);
    let token_id = join_split.token_id;
    let price = TokenPrice::new(sol_usd_price_account, token_usd_price_account, token_id)?;
    let min_batching_rate = governor.get_commitment_batching_rate();
    let fee = governor.get_program_fee();
    let subvention = fee.proof_subvention.into_token(&price, token_id)?;
    let input_preparation_tx_count = verification_account.get_prepare_inputs_instructions_count() as usize;
    let proof_verification_fee = fee.proof_verification_computation_fee(input_preparation_tx_count).into_token(&price, token_id)?;
    let commitment_hash_fee = fee.commitment_hash_computation_fee(min_batching_rate);
    let commitment_hash_fee_token = commitment_hash_fee.into_token(&price, token_id)?;
    let network_fee = Token::new_checked(token_id, fee.proof_network_fee.calc(join_split.amount))?;

    let fee = (((commitment_hash_fee_token + proof_verification_fee)? + network_fee)? - subvention)?;
    guard!(fee.amount() == join_split.fee, InvalidPublicInputs);

    verify_pool(pool, pool_account, token_id)?;
    verify_fee_collector(fee_collector, fee_collector_account, token_id)?;

    let mut associated_token_account_rent = Lamports(0);
    let mut associated_token_account_rent_token = 0;

    if let ProofRequest::Send(public_inputs) = request {
        // If the sender wants to send to an associated token account, enough Lamports (and the correct amount of tokens) need to be reserved for renting it
        // - because of this guard here, `init_verification` and `init_verification_transfer_fee` should be part of a single tx, otherwise the transfer could get stuck
        if !public_inputs.recipient.is_non_associated_token_account {
            associated_token_account_rent = spl_token_account_rent()?;
            associated_token_account_rent_token = associated_token_account_rent.into_token(&price, token_id)?.amount();
            guard!(public_inputs.join_split.amount >= associated_token_account_rent_token, InvalidPublicInputs);
        }
    }

    // `fee_payer` transfers `commitment_hash_fee` (+ `associated_token_account_rent`)? to `pool` (lamports)
    transfer_token(
        fee_payer,
        fee_payer,
        pool,
        system_program,
        (commitment_hash_fee + associated_token_account_rent)?.into_token_strict(),
    )?;

    // `fee_collector` transfers `subvention` to `pool` (token)
    transfer_token_from_pda::<FeeCollectorAccount>(
        fee_collector,
        fee_collector_account,
        pool_account,
        token_program,
        subvention,
        None,
    )?;

    guard!(verify_token_account(fee_payer_token_account, token_id)?, InvalidAccount);

    verification_account.set_other_data(
        &VerificationAccountData {
            fee_payer: RawU256::new(fee_payer.key.to_bytes()),
            fee_payer_account: RawU256::new(fee_payer_token_account.key.to_bytes()),
            skip_nullifier_pda: other_data.skip_nullifier_pda,
            min_batching_rate,
            token_id,
            subvention: subvention.amount(),
            network_fee: network_fee.amount(),
            commitment_hash_fee,
            commitment_hash_fee_token: commitment_hash_fee_token.amount(),
            proof_verification_fee: proof_verification_fee.amount(),
            associated_token_account_rent: associated_token_account_rent_token,
        }
    );

    verification_account.set_state(&VerificationState::FeeTransferred);

    Ok(())
}

/// Called once after `init_verification` to initialize the proof's public inputs
/// - Note: has to be called by the original `fee_payer`, that called `init_verification`
/// - depending on the MT-count this has to be called in a different tx than the init-tx (-> require fee_payer signature)
/// - this is required, due to tx-byte size limits
pub fn init_verification_proof(
    fee_payer: &AccountInfo,
    verification_account: &mut VerificationAccount,

    _verification_account_index: u32,
    proof: Proof,
) -> ProgramResult {
    guard!(matches!(verification_account.get_state(), VerificationState::FeeTransferred), InvalidAccountState);
    guard!(verification_account.get_is_verified().option().is_none(), ComputationIsAlreadyFinished);
    guard!(verification_account.get_other_data().fee_payer.skip_mr() == fee_payer.key.to_bytes(), InvalidAccount);

    verification_account.a.set(&proof.a);
    verification_account.b.set(&proof.b);
    verification_account.c.set(&proof.c);

    verification_account.set_state(&VerificationState::ProofSetup);

    Ok(())
}

/// Partial proof verification computation
pub fn compute_verification(
    verification_account: &mut VerificationAccount,
    precomputes_account: &PrecomputesAccount,
    instructions_account: &AccountInfo,

    _verification_account_index: u32,
) -> ProgramResult {
    guard!(precomputes_account.get_is_setup(), InvalidAccountState);
    guard!(
        matches!(verification_account.get_state(), VerificationState::None) ||
        matches!(verification_account.get_state(), VerificationState::ProofSetup),
        InvalidAccountState
    );
    guard!(verification_account.get_is_verified().option().is_none(), ComputationIsAlreadyFinished);

    // instruction_index is used to allow a uniform number of ixs per tx
    let instruction_index = if cfg!(test) {
        0
    } else {
        instructions::load_current_index_checked(instructions_account)?
    };

    match execute_with_vkey!(
        verification_account.get_kind(),
        VKey,
        verify_partial::<_, VKey>(verification_account, precomputes_account, instruction_index)
    ) {
        Ok(result) => {
            if let Some(final_result) = result { // After last round we receive the verification result
                verification_account.set_is_verified(&ElusivOption::Some(final_result));
            }
        }
        Err(e) => {
            match e {
                InvalidAccountState => return Err(e.into()),
                _ => { // An error (!= InvalidAccountState) can only happen with flawed inputs -> cancel verification
                    verification_account.set_is_verified(&ElusivOption::Some(false));
                    return Ok(())
                }
            }
        }
    }

    Ok(())
}

#[derive(BorshDeserialize, BorshSerialize, BorshSerDeSized, Clone, Copy)]
pub struct FinalizeSendData {
    pub timestamp: u64,
    pub total_amount: u64,
    pub token_id: u16,

    /// Estimated index of the MT in which the next-commitment will be inserted
    pub mt_index: u32,

    /// Estimated index of the next-commitment in the MT
    pub commitment_index: u32,
}

/// First finalize instruction
/// - for valid proof finalization: `finalize_verification_send, `finalize_verification_send_nullifiers`, `finalize_verification_transfer`
/// - for invalid proof: `finalize_verification_send`, `finalize_verification_transfer`
pub fn finalize_verification_send(
    identifier_account: &AccountInfo,
    salt_account: &AccountInfo,
    commitment_hash_queue: &mut CommitmentQueueAccount,
    verification_account: &mut VerificationAccount,
    storage_account: &StorageAccount,

    data: FinalizeSendData,
    _verification_account_index: u32,
) -> ProgramResult {
    guard!(matches!(verification_account.get_state(), VerificationState::ProofSetup), InvalidAccountState);

    match verification_account.get_is_verified() {
        ElusivOption::None => return Err(ComputationIsNotYetFinished.into()),
        ElusivOption::Some(false) => {
            verification_account.set_state(&VerificationState::Finalized);
            return Ok(())
        }
        _ => {}
    }

    let request = verification_account.get_request();
    let public_inputs = match request {
        ProofRequest::Send(public_inputs) => public_inputs,
        ProofRequest::Merge(public_inputs) => public_inputs,
        _ => return Err(FeatureNotAvailable.into())
    };

    guard!(identifier_account.key.to_bytes() == public_inputs.identifier.skip_mr(), InvalidAccount);
    guard!(salt_account.key.to_bytes() == public_inputs.salt.skip_mr(), InvalidAccount);

    let (commitment_index, mt_index) = minimum_commitment_mt_index(
        storage_account.get_trees_count(),
        storage_account.get_next_commitment_ptr(),
        CommitmentQueue::new(commitment_hash_queue).len()
    );
    guard!(data.timestamp == public_inputs.current_time, InvalidInstructionData);
    guard!(data.total_amount == public_inputs.join_split.total_amount(), InvalidInstructionData);
    guard!(data.token_id == public_inputs.join_split.token_id, InvalidInstructionData);
    guard!(data.commitment_index == commitment_index, InvalidInstructionData);
    guard!(data.mt_index == mt_index, InvalidInstructionData);

    verification_account.set_state(&VerificationState::InsertNullifiers);

    Ok(())
}

pub fn finalize_verification_send_nullifiers<'a, 'b, 'c>(
    verification_account: &mut VerificationAccount,
    nullifier_account0: &mut NullifierAccount<'a, 'b, 'c>,
    nullifier_account1: &mut NullifierAccount<'a, 'b, 'c>,

    _verification_account_index: u32,
) -> ProgramResult {
    // TODO: Handle the case in which a duplicate verification has failed (funds flow to fee-collector)
    guard!(matches!(verification_account.get_state(), VerificationState::InsertNullifiers), InvalidAccountState);

    let request = verification_account.get_request();
    let public_inputs = match request {
        ProofRequest::Send(public_inputs) => public_inputs,
        ProofRequest::Merge(public_inputs) => public_inputs,
        _ => return Err(FeatureNotAvailable.into())
    };

    let nullifier_accounts: [&mut NullifierAccount<'a, 'b, 'c>; MAX_MT_COUNT] = [nullifier_account0, nullifier_account1];
    let mut tree_index = 0;
    for (i, root) in public_inputs.join_split.roots.iter().enumerate() {
        let nullifier_hash = public_inputs.join_split.nullifier_hashes[i].reduce();
        let index = match root {
            Some(_) => {
                let t = tree_index;
                tree_index += 1;
                t
            }
            None => 0,
        };
        nullifier_accounts[index].try_insert_nullifier_hash(nullifier_hash)?;
    }

    verification_account.set_state(&VerificationState::Finalized);

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn finalize_verification_transfer_lamports<'a>(
    recipient: &AccountInfo<'a>, // can be any account for merge/migrate
    original_fee_payer: &AccountInfo<'a>,
    pool: &AccountInfo<'a>,
    fee_collector: &AccountInfo<'a>,

    commitment_hash_queue: &mut CommitmentQueueAccount,
    verification_account_info: &AccountInfo<'a>,
    nullifier_duplicate_account: &AccountInfo<'a>,

    _verification_account_index: u32,
) -> ProgramResult {
    pda_account!(mut verification_account, VerificationAccount, verification_account_info);
    let data = verification_account.get_other_data();
    let request = verification_account.get_request();
    let join_split = proof_request!(&request, public_inputs, public_inputs.join_split_inputs());

    guard!(join_split.token_id == 0, InvalidAccountState);

    guard!(matches!(verification_account.get_state(), VerificationState::Finalized), InvalidAccountState);
    nullifier_duplicate_account_pda_seed!(nullifier_hashes, join_split.nullifier_hashes);
    guard!(*nullifier_duplicate_account.key == Pubkey::find_program_address(&nullifier_hashes, &crate::id()).0, InvalidAccount);
    guard!(original_fee_payer.key.to_bytes() == data.fee_payer.skip_mr(), InvalidAccount);

    if let ElusivOption::Some(false) = verification_account.get_is_verified() {
        // `rent` and `commitment_hash_fee` flow to `fee_collector`
        close_account(fee_collector, verification_account_info)?;
        if !data.skip_nullifier_pda {
            close_account(fee_collector, nullifier_duplicate_account)?;
        }

        verification_account.set_state(&VerificationState::Closed);

        // `pool` transfers `subvention` to `fee_collector` (lamports)
        transfer_lamports_from_pda_checked(
            pool,
            fee_collector,
            data.subvention,
        )?;

        // `pool` transfers `commitment_hash_fee` to `fee_collector` (lamports)
        transfer_lamports_from_pda_checked(
            pool,
            fee_collector,
            data.commitment_hash_fee.0,
        )?;

        return Ok(())
    }

    if let ProofRequest::Send(public_inputs) = &request {
        guard!(recipient.key.to_bytes() == public_inputs.recipient.address, InvalidAccount);

        // `pool` transfers `amount` to `recipient` (lamports)
        transfer_lamports_from_pda_checked(
            pool,
            recipient,
            public_inputs.join_split.amount
        )?;
    }

    // `pool` transfers `commitment_hash_fee_token (incl. subvention) + proof_verification_fee` to `fee_payer` (lamports)
    transfer_lamports_from_pda_checked(
        pool,
        original_fee_payer,
        (
            Lamports(data.commitment_hash_fee_token) +
            Lamports(data.proof_verification_fee)
        )?.0
    )?;

    // `pool` transfers `network_fee` to `fee_collector` (lamports)
    transfer_lamports_from_pda_checked(
        pool,
        fee_collector,
        data.network_fee
    )?;

    // Close `verification_account` and `nullifier_duplicate_account`
    close_verification_pdas(
        original_fee_payer,
        verification_account_info,
        nullifier_duplicate_account,
        data.skip_nullifier_pda,
    )?;

    let mut commitment_queue = CommitmentQueue::new(commitment_hash_queue);
    commitment_queue.enqueue(
        CommitmentHashRequest {
            commitment: join_split.commitment.reduce(),
            fee_version: join_split.fee_version,
            min_batching_rate: data.min_batching_rate,
        }
    )?;

    verification_account.set_state(&VerificationState::Closed);

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn finalize_verification_transfer_token<'a>(
    signer: &AccountInfo<'a>,
    recipient: &AccountInfo<'a>, // can be any account for merge/migrate
    recipient_wallet: &AccountInfo<'a>,
    original_fee_payer: &AccountInfo<'a>,
    original_fee_payer_account: &AccountInfo<'a>,
    pool: &AccountInfo<'a>,
    pool_account: &AccountInfo<'a>,
    fee_collector: &AccountInfo<'a>,
    fee_collector_account: &AccountInfo<'a>,

    commitment_hash_queue: &mut CommitmentQueueAccount,
    verification_account_info: &AccountInfo<'a>,
    nullifier_duplicate_account: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
    mint_account: &AccountInfo<'a>,

    _verification_account_index: u32,
) -> ProgramResult {
    pda_account!(mut verification_account, VerificationAccount, verification_account_info);
    let data = verification_account.get_other_data();
    let request = verification_account.get_request();
    let join_split = proof_request!(&request, public_inputs, public_inputs.join_split_inputs());

    let token_id = join_split.token_id;
    guard!(token_id > 0, InvalidAccountState);

    guard!(matches!(verification_account.get_state(), VerificationState::Finalized), InvalidAccountState);
    nullifier_duplicate_account_pda_seed!(nullifier_hashes, join_split.nullifier_hashes);
    guard!(*nullifier_duplicate_account.key == Pubkey::find_program_address(&nullifier_hashes, &crate::id()).0, InvalidAccount);
    guard!(original_fee_payer.key.to_bytes() == data.fee_payer.skip_mr(), InvalidAccount);
    guard!(original_fee_payer_account.key.to_bytes() == data.fee_payer_account.skip_mr(), InvalidAccount);

    verify_pool(pool, pool_account, token_id)?;
    verify_fee_collector(fee_collector, fee_collector_account, token_id)?;

    if let ElusivOption::Some(false) = verification_account.get_is_verified() {
        // rent flows to `fee_collector`
        close_verification_pdas(
            fee_collector,
            verification_account_info,
            nullifier_duplicate_account,
            data.skip_nullifier_pda,
        )?;

        verification_account.set_state(&VerificationState::Closed);

        // `pool` transfers `subvention` to `fee_collector` (token)
        transfer_token_from_pda::<PoolAccount>(
            pool,
            pool_account,
            fee_collector_account,
            token_program,
            Token::new(token_id, data.subvention),
            None,
        )?;

        // `pool` transfers `commitment_hash_fee` and `associated_token_account_rent` to `fee_collector` (lamports)
        transfer_lamports_from_pda_checked(
            pool,
            fee_collector,
            (data.commitment_hash_fee + spl_token_account_rent()?)?.0,
        )?;

        return Ok(())
    }

    let mut associated_token_account_rent_token = None;
    if let ProofRequest::Send(public_inputs) = &request {
        let mut actual_recipient = recipient;

        if public_inputs.recipient.is_non_associated_token_account {
            guard!(recipient.key.to_bytes() == public_inputs.recipient.address, InvalidAccount);

            // Invalid recipient token account -> funds flow to `fee_collector` instead
            if !matches!(verify_token_account(recipient, token_id), Ok(true)) {
                actual_recipient = fee_collector_account;
            }
        } else {
            guard!(recipient_wallet.key.to_bytes() == public_inputs.recipient.address, InvalidAccount);

            guard!(
                verify_associated_token_account(recipient_wallet.key, recipient.key, token_id)?,
                InvalidAccount
            );

            if recipient.lamports() == 0 {  // Check if associated token accounts exists
                guard!(*mint_account.key == elusiv_token(token_id)?.mint, InvalidAccount);

                // We use signer (since it's an available system account) to sign the creation of the associated token account (refunded at the end)
                create_associated_token_account(
                    signer,
                    recipient_wallet,
                    recipient,
                    mint_account,
                    token_id,
                )?;

                // `pool` transfers `associated_token_account_rent` to `fee_payer` (token)
                associated_token_account_rent_token = Some(data.associated_token_account_rent);
            } else {
                associated_token_account_rent_token = Some(0) ;
            }
        }

        // `pool` transfers `amount` to `recipient` (token)
        transfer_token_from_pda::<PoolAccount>(
            pool,
            pool_account,
            actual_recipient,
            token_program,
            Token::new(
                token_id,
                public_inputs.join_split.amount - associated_token_account_rent_token.unwrap_or(0)
            ),
            None,
        )?;
    }

    // `pool` transfers `commitment_hash_fee_token (incl. subvention) + proof_verification_fee + associated_token_account_rent_token?` to `fee_payer` (token)
    transfer_token_from_pda::<PoolAccount>(
        pool,
        pool_account,
        original_fee_payer_account,
        token_program,
        (
            (
                Token::new(token_id, data.commitment_hash_fee_token) +
                Token::new(token_id, data.proof_verification_fee)
            )? +
            Token::new(
                token_id,
                associated_token_account_rent_token.unwrap_or(0)
            )
        )?,
        None,
    )?;

    // `pool` transfers `network_fee` to `fee_collector` (token)
    transfer_token_from_pda::<PoolAccount>(
        pool,
        pool_account,
        fee_collector_account,
        token_program,
        Token::new(token_id, data.network_fee),
        None,
    )?;

    // Close `verification_account` and `nullifier_duplicate_account`
    close_verification_pdas(
        original_fee_payer,
        verification_account_info,
        nullifier_duplicate_account,
        data.skip_nullifier_pda,
    )?;

    if let Some(associated_token_account_rent_token) = associated_token_account_rent_token {
        let rented = associated_token_account_rent_token != 0;
        transfer_lamports_from_pda_checked(
            pool,
            if rented { signer } else { original_fee_payer },
            spl_token_account_rent()?.0,
        )?;
    }

    let mut commitment_queue = CommitmentQueue::new(commitment_hash_queue);
    commitment_queue.enqueue(
        CommitmentHashRequest {
            commitment: join_split.commitment.reduce(),
            fee_version: join_split.fee_version,
            min_batching_rate: data.min_batching_rate,
        }
    )?;

    verification_account.set_state(&VerificationState::Closed);

    Ok(())
}

fn close_verification_pdas<'a>(
    beneficiary: &AccountInfo<'a>,
    verification_account: &AccountInfo<'a>,
    nullifier_duplicate_account: &AccountInfo<'a>,
    skipped_nullifier_pda: bool,
) -> ProgramResult {
    close_account(beneficiary, verification_account)?;
    if !skipped_nullifier_pda {
        close_account(beneficiary, nullifier_duplicate_account)?;
    }

    Ok(())
}

const TIMESTAMP_BITS_PRUNING: usize = 5;
fn is_timestamp_valid(asserted_time: u64, timestamp: u64) -> bool {
    (asserted_time >> TIMESTAMP_BITS_PRUNING) <= (timestamp >> TIMESTAMP_BITS_PRUNING)
}

fn is_vec_duplicate_free<T: std::cmp::Eq + std::hash::Hash + std::clone::Clone>(v: &Vec<T>) -> bool {
    (*v).clone().drain(..).collect::<HashSet<T>>().len() == v.len()
}

/// Computes the minimum index of a commitment and it's corresponding MT-index
fn minimum_commitment_mt_index(
    mt_index: u32,
    commitment_count: u32,
    commitment_queue_len: u32,
) -> (u32, u32) {
    let count = usize_as_u32_safe(MT_COMMITMENT_COUNT);
    let index = (commitment_count + commitment_queue_len) % count;
    let mt_offset = (commitment_count + commitment_queue_len) / count;
    (index, mt_index + mt_offset)
}

fn check_join_split_public_inputs(
    public_inputs: &JoinSplitPublicInputs,
    storage_account: &StorageAccount,
    nullifier_accounts: [&NullifierAccount; MAX_MT_COUNT],
    tree_indices: &[u32; MAX_MT_COUNT],
) -> ProgramResult {
    // Check that the resulting commitment is not the zero-commitment
    guard!(public_inputs.commitment.skip_mr() != ZERO_COMMITMENT_RAW, InvalidPublicInputs);

    let active_tree_index = storage_account.get_trees_count();

    guard!(public_inputs.roots[0].is_some(), InvalidPublicInputs);
    guard!(public_inputs.nullifier_hashes.len() == public_inputs.commitment_count as usize, InvalidPublicInputs);
    guard!(public_inputs.roots.len() == public_inputs.commitment_count as usize, InvalidPublicInputs);

    let mut roots = Vec::new();
    let mut tree_index = vec![0; public_inputs.commitment_count as usize];
    let mut nullifier_hashes = Vec::new();
    for (i, root) in public_inputs.roots.iter().enumerate() {
        match root {
            Some(root) => {
                let index = roots.len();
                tree_index[i] = index;
                roots.push(root);
                nullifier_hashes.push(vec![public_inputs.nullifier_hashes[i]]);

                // Verify that root is valid
                // - Note: roots are stored in mr-form
                if tree_indices[index] == active_tree_index { // Active tree
                    guard!(storage_account.is_root_valid(root.reduce()), InvalidMerkleRoot);
                } else { // Closed tree
                    guard!(root.reduce() == nullifier_accounts[index].get_root(), InvalidMerkleRoot);
                }
            }
            None => {
                nullifier_hashes[0].push(public_inputs.nullifier_hashes[i]);
            }
        }
    }
    guard!(!roots.is_empty() && roots.len() <= MAX_MT_COUNT, InvalidPublicInputs);
    guard!(public_inputs.roots[0].is_some(), InvalidPublicInputs);
    guard!(tree_indices.len() >= roots.len(), InvalidPublicInputs);

    // All supplied MTs (storage/nullifier-accounts) are pairwise different
    if roots.len() > 1 {
        guard!(is_vec_duplicate_free(&tree_indices.to_vec()), InvalidInstructionData);
    }

    for (i, &nullifier_hash) in public_inputs.nullifier_hashes.iter().enumerate() {
        // No duplicate nullifier-hashes for the same MT
        for j in 0..public_inputs.nullifier_hashes.len() {
            if i == j { continue }
            if nullifier_hash == public_inputs.nullifier_hashes[j] {
                guard!(tree_index[i] != tree_index[j], InvalidPublicInputs);
            }
        }

        // Check that `nullifier_hash` is new
        // - Note: nullifier-hashes are stored in mr-form
        guard!(
            nullifier_accounts[tree_index[i]].can_insert_nullifier_hash(public_inputs.nullifier_hashes[i].reduce())?,
            CouldNotInsertNullifier
        );
    }

    Ok(())
}

#[cfg(test)]
fn mutate<T: Clone, F>(v: &T, f: F) -> T where F: Fn(&mut T) {
    let mut i = v.clone();
    f(&mut i);
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use assert_matches::assert_matches;
    use elusiv_computation::PartialComputation;
    use pyth_sdk_solana::Price;
    use solana_program::native_token::LAMPORTS_PER_SOL;
    use solana_program::pubkey::Pubkey;
    use solana_program::system_program;
    use crate::fields::{u256_from_str, u256_from_str_skip_mr};
    use crate::processor::ZERO_COMMITMENT_RAW;
    use crate::proof::precompute::{VirtualPrecomputes, precompute_account_size};
    use crate::proof::{COMBINED_MILLER_LOOP_IXS, FINAL_EXPONENTIATION_IXS, proof_from_str, CombinedMillerLoop, FinalExponentiation};
    use crate::state::fee::{ProgramFee, BasisPointFee};
    use crate::state::governor::PoolAccount;
    use crate::state::empty_root_raw;
    use crate::state::program_account::{SizedAccount, PDAAccount, MultiAccountProgramAccount, MultiAccountAccount};
    use crate::macros::{two_pow, zero_account, account, test_account_info, storage_account, nullifier_account, pyth_price_account_info, token_pda_account};
    use crate::token::{Lamports, USDC_TOKEN_ID, LAMPORTS_TOKEN_ID, spl_token_account_data, USDT_TOKEN_ID};
    use crate::types::{RawU256, Proof, compute_fee_rec, compute_fee_rec_lamports, JOIN_SPLIT_MAX_N_ARITY, RecipientAccount};

    fn fee() -> ProgramFee {
        ProgramFee {
            lamports_per_tx: Lamports(5000),
            base_commitment_network_fee: BasisPointFee(11),
            proof_network_fee: BasisPointFee(100),
            base_commitment_subvention: Lamports(33),
            proof_subvention: Lamports(44),
            warden_hash_tx_reward: Lamports(300),
            warden_proof_reward: Lamports(555),
            proof_base_tx_count: (CombinedMillerLoop::TX_COUNT + FinalExponentiation::TX_COUNT + 2) as u64,
        }
    }

    #[test]
    fn test_init_verification() {
        use ProofRequest::*;

        storage_account!(s);
        nullifier_account!(mut n);
        test_account_info!(fee_payer, 0);
        test_account_info!(recipient, 0);
        account!(v_acc, VerificationAccount::find(Some(0)).0, vec![0; VerificationAccount::SIZE]);

        let mut inputs = SendPublicInputs {
            join_split: JoinSplitPublicInputs {
                commitment_count: 1,
                roots: vec![Some(empty_root_raw())],
                nullifier_hashes: vec![RawU256::new(u256_from_str_skip_mr("1"))],
                commitment: RawU256::new(u256_from_str_skip_mr("1")),
                fee_version: 0,
                amount: LAMPORTS_PER_SOL,
                fee: 0,
                token_id: 0,
            },
            recipient: RecipientAccount::new(recipient.key.to_bytes(), true),
            current_time: 0,
            identifier: RawU256::new(u256_from_str_skip_mr("1")),
            salt: RawU256::new(u256_from_str_skip_mr("1")),
        };
        compute_fee_rec_lamports::<SendQuadraVKey, _>(&mut inputs, &fee());

        account!(n_duplicate_acc, inputs.join_split.nullifier_duplicate_pda().0, vec![1]);

        // TODO: test skip nullifier pda

        // Commitment-count too low
        assert_matches!(
            init_verification(&fee_payer, &v_acc, &n_duplicate_acc, &recipient, &s, &n, &n, 0, [0, 1], Send(mutate(&inputs, |v| {
                v.join_split.commitment_count = 0;
            })), false),
            Err(_)
        );

        // Commitment-count too high
        assert_matches!(
            init_verification(&fee_payer, &v_acc, &n_duplicate_acc, &recipient, &s, &n, &n, 0, [0, 1], Send(mutate(&inputs, |v| {
                v.join_split.commitment_count = JOIN_SPLIT_MAX_N_ARITY + 1;
            })), false),
            Err(_)
        );

        // Invalid root
        assert_matches!(
            init_verification(&fee_payer, &v_acc, &n_duplicate_acc, &recipient, &s, &n, &n, 0, [0, 1], Send(mutate(&inputs, |v| {
                v.join_split.roots[0] = Some(RawU256::new(u256_from_str_skip_mr("1")));
            })), false),
            Err(_)
        );

        // First root is None
        assert_matches!(
            init_verification(&fee_payer, &v_acc, &n_duplicate_acc, &recipient, &s, &n, &n, 0, [0, 1], Send(mutate(&inputs, |v| {
                v.join_split.roots[0] = None;
            })), false),
            Err(_)
        );

        // Mismatched tree indices
        assert_matches!(
            init_verification(&fee_payer, &v_acc, &n_duplicate_acc, &recipient, &s, &n, &n, 0, [1, 0], Send(inputs.clone()), false),
            Err(_)
        );

        // Zero commitment
        assert_matches!(
            init_verification(&fee_payer, &v_acc, &n_duplicate_acc, &recipient, &s, &n, &n, 0, [0, 1], Send(mutate(&inputs, |v| {
                v.join_split.commitment = RawU256::new(ZERO_COMMITMENT_RAW);
            })), false),
            Err(_)
        );

        // Nullifier already exists
        n.try_insert_nullifier_hash(inputs.join_split.nullifier_hashes[0].reduce()).unwrap();
        assert_matches!(
            init_verification(&fee_payer, &v_acc, &n_duplicate_acc, &recipient, &s, &n, &n, 0, [0, 1], Send(inputs.clone()), false),
            Err(_)
        );
        
        // Invalid nullifier_duplicate_account
        nullifier_account!(n);
        account!(invalid_n_duplicate_acc, VerificationAccount::find(Some(0)).0, vec![1]);
        assert_matches!(
            init_verification(&fee_payer, &v_acc, &invalid_n_duplicate_acc, &recipient, &s, &n, &n, 0, [0, 1], Send(inputs.clone()), false),
            Err(_)
        );

        // Migrate always fails 
        assert_matches!(
            init_verification(&fee_payer, &v_acc, &n_duplicate_acc, &recipient, &s, &n, &n, 0, [0, 1], Migrate(
                MigratePublicInputs {
                    join_split: inputs.join_split.clone(),
                    current_nsmt_root: RawU256::new([0; 32]),
                    next_nsmt_root: RawU256::new([0; 32]),
                }
            ), false),
            Err(_)
        );

        assert_matches!(
            init_verification(&fee_payer, &v_acc, &n_duplicate_acc, &recipient, &s, &n, &n, 0, [0, 1], Send(inputs), false),
            Ok(())
        );
    }

    #[test]
    fn test_init_verification_transfer_fee_lamports() {
        test_account_info!(f, 0);   // fee_payer
        test_account_info!(pool, 0);
        test_account_info!(fee_c, 0);   // fee_collector
        test_account_info!(any, 0);
        account!(sys, system_program::id(), vec![]);
        account!(spl, spl_token::id(), vec![]);
        zero_account!(mut g, GovernorAccount);
        g.set_program_fee(&fee());
        let mut data = vec![0; VerificationAccount::SIZE];
        let mut verification_acc= VerificationAccount::new(&mut data).unwrap();
    
        let mut inputs = SendPublicInputs {
            join_split: JoinSplitPublicInputs {
                commitment_count: 1,
                roots: vec![Some(empty_root_raw())],
                nullifier_hashes: vec![RawU256::new(u256_from_str_skip_mr("1"))],
                commitment: RawU256::new(u256_from_str_skip_mr("1")),
                fee_version: 0,
                amount: LAMPORTS_PER_SOL,
                fee: 0,
                token_id: 0,
            },
            recipient: RecipientAccount::new([0; 32], true),
            current_time: 0,
            identifier: RawU256::new(u256_from_str_skip_mr("1")),
            salt: RawU256::new(u256_from_str_skip_mr("1")),
        };
        compute_fee_rec_lamports::<SendQuadraVKey, _>(&mut inputs, &fee());
        let instructions = prepare_public_inputs_instructions::<SendQuadraVKey>(&inputs.public_signals_skip_mr());

        verification_acc.set_request(&ProofRequest::Send(inputs.clone()));
        verification_acc.set_prepare_inputs_instructions_count(&(instructions.len() as u32));
        verification_acc.set_other_data(&VerificationAccountData { fee_payer: RawU256::new(f.key.to_bytes()), ..Default::default() });

        // Invalid fee_payer
        test_account_info!(f2, 0); 
        assert_matches!(
            init_verification_transfer_fee(&f2, &f, &pool, &pool, &fee_c, &fee_c, &any, &any, &g, &mut verification_acc, &sys, &sys, 0),
            Err(_)
        );

        // Invalid verification account state
        verification_acc.set_state(&VerificationState::FeeTransferred);
        assert_matches!(
            init_verification_transfer_fee(&f, &f, &pool, &pool, &fee_c, &fee_c, &any, &any, &g, &mut verification_acc, &sys, &sys, 0),
            Err(_)
        );

        // Invalid fee_version
        verification_acc.set_state(&VerificationState::None);
        g.set_fee_version(&1);
        assert_matches!(
            init_verification_transfer_fee(&f, &f, &pool, &pool, &fee_c, &fee_c, &any, &any, &g, &mut verification_acc, &sys, &sys, 0),
            Err(_)
        );

        // Invalid fee
        g.set_fee_version(&0);
        inputs.join_split.fee += 1;
        verification_acc.set_request(&ProofRequest::Send(inputs.clone()));
        assert_matches!(
            init_verification_transfer_fee(&f, &f, &pool, &pool, &fee_c, &fee_c, &any, &any, &g, &mut verification_acc, &sys, &sys, 0),
            Err(_)
        );

        // Invalid system_program
        inputs.join_split.fee = 0;
        compute_fee_rec_lamports::<SendQuadraVKey, _>(&mut inputs, &fee());
        verification_acc.set_request(&ProofRequest::Send(inputs));
        assert_matches!(
            init_verification_transfer_fee(&f, &f, &pool, &pool, &fee_c, &fee_c, &any, &any, &g, &mut verification_acc, &sys, &spl, 0),
            Err(_)
        );

        // Invalid pool_account
        assert_matches!(
            init_verification_transfer_fee(&f, &f, &pool, &any, &fee_c, &fee_c, &any, &any, &g, &mut verification_acc, &sys, &sys, 0),
            Err(_)
        );

        // Invalid fee_collector_account
        assert_matches!(
            init_verification_transfer_fee(&f, &f, &pool, &pool, &fee_c, &any, &any, &any, &g, &mut verification_acc, &sys, &sys, 0),
            Err(_)
        );

        assert_matches!(
            init_verification_transfer_fee(&f, &f, &pool, &pool, &fee_c, &fee_c, &any, &any, &g, &mut verification_acc, &sys, &sys, 0),
            Ok(())
        );

        assert_matches!(verification_acc.get_state(), VerificationState::FeeTransferred);
    }

    #[test]
    fn test_init_verification_transfer_fee_token() {
        test_account_info!(f, 0);   // fee_payer
        account!(sys, system_program::id(), vec![]);
        account!(spl, spl_token::id(), vec![]);
        zero_account!(mut g, GovernorAccount);
        g.set_program_fee(&fee());
        let mut data = vec![0; VerificationAccount::SIZE];
        let mut verification_acc = VerificationAccount::new(&mut data).unwrap();

        account!(token_acc, Pubkey::new_unique(), spl_token_account_data(USDC_TOKEN_ID), spl_token::id());
        account!(wrong_token_acc, Pubkey::new_unique(), spl_token_account_data(USDT_TOKEN_ID), spl_token::id());

        token_pda_account!(pool, pool_token, PoolAccount, USDC_TOKEN_ID);
        token_pda_account!(fee_c, fee_c_token, FeeCollectorAccount, USDC_TOKEN_ID);

        let sol_usd = Price { price: 39, conf: 1, expo: 0 };
        let usdc_usd = Price { price: 1, conf: 1, expo: 0 };
        let price = TokenPrice::new_from_sol_price(sol_usd, usdc_usd, USDC_TOKEN_ID).unwrap();
        pyth_price_account_info!(sol, LAMPORTS_TOKEN_ID, sol_usd);
        pyth_price_account_info!(usdc, USDC_TOKEN_ID, usdc_usd);
    
        let mut inputs = SendPublicInputs {
            join_split: JoinSplitPublicInputs {
                commitment_count: 1,
                roots: vec![Some(empty_root_raw())],
                nullifier_hashes: vec![RawU256::new(u256_from_str_skip_mr("1"))],
                commitment: RawU256::new(u256_from_str_skip_mr("1")),
                fee_version: 0,
                amount: 1_000_000,
                fee: 0,
                token_id: USDC_TOKEN_ID,
            },
            recipient: RecipientAccount::new([0; 32], true),
            current_time: 0,
            identifier: RawU256::new(u256_from_str_skip_mr("1")),
            salt: RawU256::new(u256_from_str_skip_mr("1")),
        };
        compute_fee_rec::<SendQuadraVKey, _>(&mut inputs, &fee(), &price);
        let instructions = prepare_public_inputs_instructions::<SendQuadraVKey>(&inputs.public_signals_skip_mr());

        verification_acc.set_request(&ProofRequest::Send(inputs.clone()));
        verification_acc.set_prepare_inputs_instructions_count(&(instructions.len() as u32));
        verification_acc.set_other_data(&VerificationAccountData { fee_payer: RawU256::new(f.key.to_bytes()), ..Default::default() });

        // Invalid fee
        inputs.join_split.fee += 1;
        verification_acc.set_request(&ProofRequest::Send(inputs.clone()));
        assert_matches!(
            init_verification_transfer_fee(&f, &token_acc, &pool, &pool_token, &fee_c, &fee_c_token, &sol, &usdc, &g, &mut verification_acc, &spl, &sys, 0),
            Err(_)
        );

        inputs.join_split.fee = 0;
        compute_fee_rec::<SendQuadraVKey, _>(&mut inputs, &fee(), &price);
        verification_acc.set_request(&ProofRequest::Send(inputs.clone()));

        // Invalid system_program
        assert_matches!(
            init_verification_transfer_fee(&f, &token_acc, &pool, &pool_token, &fee_c, &fee_c_token, &sol, &usdc, &g, &mut verification_acc, &spl, &spl, 0),
            Err(_)
        );

        // Invalid token_program
        assert_matches!(
            init_verification_transfer_fee(&f, &token_acc, &pool, &pool_token, &fee_c, &fee_c_token, &sol, &usdc, &g, &mut verification_acc, &sys, &sys, 0),
            Err(_)
        );

        // Invalid fee_payer_account
        assert_matches!(
            init_verification_transfer_fee(&f, &wrong_token_acc, &pool, &pool_token, &fee_c, &fee_c_token, &sol, &usdc, &g, &mut verification_acc, &spl, &sys, 0),
            Err(_)
        );

        // Invalid pool_account
        assert_matches!(
            init_verification_transfer_fee(&f, &token_acc, &pool, &fee_c_token, &fee_c, &fee_c_token, &sol, &usdc, &g, &mut verification_acc, &spl, &sys, 0),
            Err(_)
        );

        // Invalid fee_collector_account
        assert_matches!(
            init_verification_transfer_fee(&f, &token_acc, &pool, &pool_token, &fee_c, &pool_token, &sol, &usdc, &g, &mut verification_acc, &spl, &sys, 0),
            Err(_)
        );

        // Invalid sol_usd_price_account
        assert_matches!(
            init_verification_transfer_fee(&f, &token_acc, &pool, &pool_token, &fee_c, &fee_c_token, &usdc, &usdc, &g, &mut verification_acc, &spl, &sys, 0),
            Err(_)
        );

        // Invalid token_usd_price_account
        assert_matches!(
            init_verification_transfer_fee(&f, &token_acc, &pool, &pool_token, &fee_c, &fee_c_token, &sol, &sol, &g, &mut verification_acc, &spl, &sys, 0),
            Err(_)
        );

        assert_matches!(
            init_verification_transfer_fee(&f, &token_acc, &pool, &pool_token, &fee_c, &fee_c_token, &sol, &usdc, &g, &mut verification_acc, &spl, &sys, 0),
            Ok(())
        );

        assert_matches!(verification_acc.get_state(), VerificationState::FeeTransferred);
    }
    
    #[test]
    fn test_init_verification_proof() {
        let mut data = vec![0; VerificationAccount::SIZE];
        let mut verification_account = VerificationAccount::new(&mut data).unwrap();

        let proof = test_proof();
        let valid_pk = Pubkey::new(&[0; 32]);
        account!(fee_payer, valid_pk, vec![0; 0]);

        // Account setup
        verification_account.set_state(&VerificationState::ProofSetup);
        assert_matches!(init_verification_proof(&fee_payer, &mut verification_account, 0, proof), Err(_));
        verification_account.set_state(&VerificationState::FeeTransferred);

        // Computation already finished
        verification_account.set_is_verified(&ElusivOption::Some(true));
        assert_matches!(init_verification_proof(&fee_payer, &mut verification_account, 0, proof), Err(_));
        verification_account.set_is_verified(&ElusivOption::Some(false));
        assert_matches!(init_verification_proof(&fee_payer, &mut verification_account, 0, proof), Err(_));
        verification_account.set_is_verified(&ElusivOption::None);

        // Invalid fee_payer
        let invalid_pk = Pubkey::new_unique();
        account!(invalid_fee_payer, invalid_pk, vec![0; 0]);
        assert_matches!(init_verification_proof(&invalid_fee_payer, &mut verification_account, 0, proof), Err(_));

        // Success
        assert_matches!(init_verification_proof(&fee_payer, &mut verification_account, 0, proof), Ok(()));
        assert_matches!(verification_account.get_state(), VerificationState::ProofSetup);
        assert_eq!(verification_account.a.get(), proof.a);
        assert_eq!(verification_account.b.get(), proof.b);
        assert_eq!(verification_account.c.get(), proof.c);

        // Already setup proof
        assert_matches!(init_verification_proof(&fee_payer, &mut verification_account, 0, proof), Err(_));
    }

    macro_rules! precomputes_sub_account {
        ($id: ident, $vkey: ident) => {
            let mut data = vec![0; precompute_account_size::<$vkey>()];
            let precomputes = VirtualPrecomputes::<$vkey>::new(&mut data);

            let mut d = vec![1];
            d.extend(precomputes.data.to_vec());

            let pk = solana_program::pubkey::Pubkey::new_unique();
            account!($id, pk, d);
        };
    }

    macro_rules! precomputes_account {
        ($id: ident) => {
            let mut data = vec![0; PrecomputesAccount::SIZE];
            let mut map = HashMap::new();

            precomputes_sub_account!(acc0, SendQuadraVKey);
            map.insert(0, &acc0);

            let mut $id = PrecomputesAccount::new(&mut data, map).unwrap();
            $id.set_is_setup(&true);
        };
    }

    #[test]
    fn test_compute_verification() {
        let mut data = vec![0; VerificationAccount::SIZE];
        let mut verification_account = VerificationAccount::new(&mut data).unwrap();
        precomputes_account!(precomputes_account);
        test_account_info!(any, 0);

        // Setup
        let public_inputs = test_public_inputs();
        for (i, &public_input) in public_inputs.iter().enumerate() {
            verification_account.set_public_input(i, &RawU256::new(public_input));
        }
        let instructions = prepare_public_inputs_instructions::<SendQuadraVKey>(&public_inputs);
        verification_account.set_prepare_inputs_instructions_count(&(instructions.len() as u32));
        for (i, &ix) in instructions.iter().enumerate() {
            verification_account.set_prepare_inputs_instructions(i, &(ix as u16));
        }

        // Computation is already finished (is_verified is Some)
        verification_account.set_is_verified(&ElusivOption::Some(true));
        assert_matches!(compute_verification(&mut verification_account, &precomputes_account, &any, 0), Err(_));
        verification_account.set_is_verified(&ElusivOption::None);

        // Success for public input preparation
        for _ in 0..instructions.len() {
            assert_matches!(compute_verification(&mut verification_account, &precomputes_account, &any, 0), Ok(()));
        }

        // Failure for miller loop (proof not setup)
        assert_matches!(compute_verification(&mut verification_account, &precomputes_account, &any, 0), Err(_));

        let proof = test_proof();
        verification_account.a.set(&proof.a);
        verification_account.b.set(&proof.b);
        verification_account.c.set(&proof.c);
        verification_account.set_state(&VerificationState::ProofSetup);

        // Success
        for _ in 0..COMBINED_MILLER_LOOP_IXS + FINAL_EXPONENTIATION_IXS {
            assert_matches!(compute_verification(&mut verification_account, &precomputes_account, &any, 0), Ok(()));
        }

        // Computation is finished
        assert_matches!(compute_verification(&mut verification_account, &precomputes_account, &any, 0), Err(_));
        assert_matches!(verification_account.get_is_verified().option(), Some(false));
    }

    macro_rules! finalize_send_test {
        ($token_id: expr, $public_inputs: ident, $v_data: ident, $nullifier_duplicate_pda: ident, $finalize_data: ident) => {
            let $public_inputs = SendPublicInputs {
                join_split: JoinSplitPublicInputs {
                    commitment_count: 1,
                    roots: vec![Some(empty_root_raw())],
                    nullifier_hashes: vec![RawU256::new(u256_from_str_skip_mr("1"))],
                    commitment: RawU256::new(u256_from_str_skip_mr("987654321")),
                    fee_version: 0,
                    amount: LAMPORTS_PER_SOL,
                    fee: 10000,
                    token_id: $token_id,
                },
                recipient: RecipientAccount::new(u256_from_str_skip_mr("123"), true),
                current_time: 112233,
                identifier: RawU256::new(u256_from_str_skip_mr("12345")),
                salt: RawU256::new(u256_from_str_skip_mr("6789")),
            };
    
            let mut $v_data = vec![0; VerificationAccount::SIZE];
            let mut v_account = VerificationAccount::new(&mut $v_data).unwrap();
            let fee_payer = RawU256::new(Pubkey::new_unique().to_bytes());
            v_account.setup(fee_payer, false, &[], &vec![0], 0, ProofRequest::Send($public_inputs.clone()), [0, 1]).unwrap();
            v_account.set_state(&VerificationState::ProofSetup);
            v_account.set_is_verified(&ElusivOption::Some(true));
            v_account.set_other_data(&VerificationAccountData {
                fee_payer,
                fee_payer_account: fee_payer,
                ..Default::default()
            });

            nullifier_duplicate_account_pda_seed!(seeds, $public_inputs.join_split.nullifier_hashes);
            let $nullifier_duplicate_pda = Pubkey::find_program_address(&seeds[..], &crate::id()).0;

            let $finalize_data = FinalizeSendData {
                timestamp: $public_inputs.current_time,
                total_amount: $public_inputs.join_split.total_amount(),
                token_id: $token_id,
                mt_index: 0,
                commitment_index: 0,
            };
        };
    }

    macro_rules! storage_account {
        ($id: ident) => {
            let mut data = vec![0; StorageAccount::SIZE];
            let $id = StorageAccount::new(&mut data, HashMap::new()).unwrap();
        };
    }

    #[test]
    fn test_finalize_verification_send_valid() {
        finalize_send_test!(USDC_TOKEN_ID, public_inputs, verification_acc_data, _nullifier_duplicate_pda, finalize_data);
        let mut verification_acc = VerificationAccount::new(&mut verification_acc_data).unwrap();
        let mut data = vec![0; CommitmentQueueAccount::SIZE];
        let mut queue = CommitmentQueueAccount::new(&mut data).unwrap();
        let identifier_pk = Pubkey::new(&public_inputs.identifier.skip_mr());
        let salt_pk = Pubkey::new(&public_inputs.salt.skip_mr());
        account!(identifier, identifier_pk, vec![]);
        account!(salt, salt_pk, vec![]);
        storage_account!(storage);

        // Verification is not finished
        verification_acc.set_is_verified(&ElusivOption::None);
        assert_matches!(
            finalize_verification_send(&identifier, &salt, &mut queue, &mut verification_acc, &storage, finalize_data, 0),
            Err(_)
        );

        verification_acc.set_is_verified(&ElusivOption::Some(true));

        // Invalid identifier
        {
            account!(identifier, salt_pk, vec![]); 
            assert_matches!(
                finalize_verification_send(&identifier, &salt, &mut queue, &mut verification_acc, &storage, finalize_data, 0),
                Err(_)
            );
        }

        // Invalid salt
        {
            account!(salt, identifier_pk, vec![]); 
            assert_matches!(
                finalize_verification_send(&identifier, &salt, &mut queue, &mut verification_acc, &storage, finalize_data, 0),
                Err(_)
            );
        }

        // Invalid finalize_data
        for invalid_data in [
            mutate(&finalize_data, |d| { d.timestamp = 0 }),
            mutate(&finalize_data, |d| { d.total_amount = public_inputs.join_split.amount }),
            mutate(&finalize_data, |d| { d.token_id = 0 }),
            mutate(&finalize_data, |d| { d.commitment_index = 1 }),
            mutate(&finalize_data, |d| { d.mt_index = 1 }),
        ] {
            assert_matches!(
                finalize_verification_send(&identifier, &salt, &mut queue, &mut verification_acc, &storage, invalid_data, 0),
                Err(_)
            );
        }

        // Success
        assert_matches!(
            finalize_verification_send(&identifier, &salt, &mut queue, &mut verification_acc, &storage, finalize_data, 0),
            Ok(())
        );

        assert_matches!(verification_acc.get_state(), VerificationState::InsertNullifiers);

        // Called twice
        assert_matches!(
            finalize_verification_send(&identifier, &salt, &mut queue, &mut verification_acc, &storage, finalize_data, 0),
            Err(_)
        );
    }

    #[test]
    fn test_finalize_verification_send_invalid() {
        finalize_send_test!(USDC_TOKEN_ID, public_inputs, verification_acc_data, _nullifier_duplicate_pda, finalize_data);
        let mut verification_acc = VerificationAccount::new(&mut verification_acc_data).unwrap();
        let mut data = vec![0; CommitmentQueueAccount::SIZE];
        let mut queue = CommitmentQueueAccount::new(&mut data).unwrap();
        let identifier_pk = Pubkey::new(&public_inputs.identifier.skip_mr());
        let salt_pk = Pubkey::new(&public_inputs.salt.skip_mr());
        account!(identifier, identifier_pk, vec![]);
        account!(salt, salt_pk, vec![]);
        verification_acc.set_is_verified(&ElusivOption::Some(false));
        storage_account!(storage);

        assert_matches!(
            finalize_verification_send(&identifier, &salt, &mut queue, &mut verification_acc, &storage, finalize_data, 0),
            Ok(())
        );        
        assert_matches!(verification_acc.get_state(), VerificationState::Finalized);
    }

    #[test]
    fn test_finalize_verification_migrate() {
        let migrate_public_inputs = MigratePublicInputs {
            join_split: JoinSplitPublicInputs {
                commitment_count: 1,
                roots: vec![Some(empty_root_raw())],
                nullifier_hashes: vec![RawU256::new(u256_from_str_skip_mr("1"))],
                commitment: RawU256::new(u256_from_str_skip_mr("1")),
                fee_version: 0,
                amount: LAMPORTS_PER_SOL,
                fee: 10000,
                token_id: 0,
            },
            current_nsmt_root: RawU256::new([0; 32]),
            next_nsmt_root: RawU256::new([0; 32]),
        };

        let pk = Pubkey::new_unique();
        account!(acc, pk, vec![]);

        let mut data = vec![0; VerificationAccount::SIZE];
        let mut v_account = VerificationAccount::new(&mut data).unwrap();
        v_account.set_request(&ProofRequest::Migrate(migrate_public_inputs));
        v_account.set_state(&VerificationState::ProofSetup);
        v_account.set_is_verified(&ElusivOption::Some(true));

        let mut data = vec![0; CommitmentQueueAccount::SIZE];
        let mut queue = CommitmentQueueAccount::new(&mut data).unwrap();

        let finalize_data = FinalizeSendData { timestamp: 0, total_amount: 0, token_id: 0, mt_index: 0, commitment_index: 0 };
        storage_account!(storage);

        assert_matches!(
            finalize_verification_send(&acc, &acc, &mut queue, &mut v_account, &storage, finalize_data, 0),
            Err(_)
        );
    }

    #[test]
    fn test_finalize_verification_send_nullifiers() {
        finalize_send_test!(USDC_TOKEN_ID, public_inputs, verification_acc_data, _nullifier_duplicate_pda, _finalize_data);
        let mut verification_acc = VerificationAccount::new(&mut verification_acc_data).unwrap();
        nullifier_account!(mut n_acc_0);
        nullifier_account!(mut n_acc_1);

        // finalize_verification_send not called
        verification_acc.set_state(&VerificationState::InsertNullifiers);

        // Nullifier duplicate
        n_acc_0.try_insert_nullifier_hash(public_inputs.join_split.nullifier_hashes[0].reduce()).unwrap();
        assert_matches!(
            finalize_verification_send_nullifiers(&mut verification_acc, &mut n_acc_0, &mut n_acc_1, 0),
            Err(_)
        );

        nullifier_account!(mut n_acc_0);

        // Success
        assert_matches!(
            finalize_verification_send_nullifiers(&mut verification_acc, &mut n_acc_0, &mut n_acc_1, 0),
            Ok(())
        );

        assert!(!n_acc_0.can_insert_nullifier_hash(public_inputs.join_split.nullifier_hashes[0].reduce()).unwrap());
        assert_matches!(verification_acc.get_state(), VerificationState::Finalized);

        // Called twice
        assert_matches!(
            finalize_verification_send_nullifiers(&mut verification_acc, &mut n_acc_0, &mut n_acc_1, 0),
            Err(_)
        );
    }

    #[test]
    fn test_finalize_verification_transfer_lamports() -> ProgramResult {
        finalize_send_test!(LAMPORTS_TOKEN_ID, public_inputs, verification_acc_data, nullifier_duplicate_pda, _finalize_data);
        account!(recipient, public_inputs.recipient.pubkey(), vec![]);
        let fee_payer = Pubkey::new(&VerificationAccount::new(&mut verification_acc_data).unwrap().get_other_data().fee_payer.skip_mr());
        account!(f, fee_payer, vec![]);  // fee_payer
        test_account_info!(pool, 0);
        test_account_info!(fee_c, 0);
        test_account_info!(any, 0);
        account!(n_pda, nullifier_duplicate_pda, vec![]);
        account!(v_acc, Pubkey::new_unique(), verification_acc_data);
        let mut data = vec![0; CommitmentQueueAccount::SIZE];
        let mut queue = CommitmentQueueAccount::new(&mut data).unwrap();

        {
            pda_account!(mut v_acc, VerificationAccount, v_acc);
            v_acc.set_state(&VerificationState::None);
            v_acc.set_is_verified(&ElusivOption::Some(true));
        }

        // Invalid state
        assert_matches!(
            finalize_verification_transfer_lamports(&recipient, &f, &pool, &fee_c, &mut queue, &v_acc, &n_pda, 0),
            Err(_)
        );

        {
            pda_account!(mut v_acc, VerificationAccount, v_acc);
            v_acc.set_state(&VerificationState::Finalized);
        }

        // Invalid nullifier_duplicate_account
        account!(invalid_n_pda, VerificationAccount::find(Some(0)).0, vec![1]);
        assert_matches!(
            finalize_verification_transfer_lamports(&recipient, &f, &pool, &fee_c, &mut queue, &v_acc, &invalid_n_pda, 0),
            Err(_)
        );

        // Invalid original_fee_payer
        assert_matches!(
            finalize_verification_transfer_lamports(&recipient, &any, &pool, &fee_c, &mut queue, &v_acc, &n_pda, 0),
            Err(_)
        );

        // Invalid recipient
        assert_matches!(
            finalize_verification_transfer_lamports(&any, &f, &pool, &fee_c, &mut queue, &v_acc, &n_pda, 0),
            Err(_)
        );

        // Commitment queue is full
        {
            let mut queue = CommitmentQueue::new(&mut queue);
            for _ in 0..CommitmentQueue::CAPACITY {
                queue.enqueue(CommitmentHashRequest { commitment: [0; 32], fee_version: 0, min_batching_rate: 0 }).unwrap();
            }
        }
        assert_matches!(
            finalize_verification_transfer_lamports(&recipient, &f, &pool, &fee_c, &mut queue, &v_acc, &n_pda, 0),
            Err(_)
        );

        let mut data = vec![0; CommitmentQueueAccount::SIZE];
        let mut queue = CommitmentQueueAccount::new(&mut data).unwrap();

        assert_matches!(
            finalize_verification_transfer_lamports(&recipient, &f, &pool, &fee_c, &mut queue, &v_acc, &n_pda, 0),
            Ok(())
        );

        assert_eq!(n_pda.lamports(), 0);
        assert_eq!(v_acc.lamports(), 0);
        pda_account!(v_acc, VerificationAccount, v_acc);
        assert_matches!(v_acc.get_state(), VerificationState::Closed);

        Ok(())
    }

    #[test]
    fn test_finalize_verification_transfer_token() -> ProgramResult {
        finalize_send_test!(USDC_TOKEN_ID, public_inputs, verification_acc_data, nullifier_duplicate_pda, _finalize_data);
        account!(r, &public_inputs.recipient.pubkey(), vec![], spl_token::id());
        let fee_payer = Pubkey::new(&VerificationAccount::new(&mut verification_acc_data).unwrap().get_other_data().fee_payer.skip_mr());
        account!(f, fee_payer, vec![]);  // fee_payer
        account!(f_token, fee_payer, vec![], spl_token::id());  // fee_payer
        token_pda_account!(pool, pool_token, PoolAccount, USDC_TOKEN_ID);
        token_pda_account!(fee_c, fee_c_token, FeeCollectorAccount, USDC_TOKEN_ID);
        test_account_info!(any, 0);
        account!(spl, spl_token::id(), vec![]);
        account!(n_pda, nullifier_duplicate_pda, vec![]);
        account!(v_acc, Pubkey::new_unique(), verification_acc_data);
        let mut data = vec![0; CommitmentQueueAccount::SIZE];
        let mut queue = CommitmentQueueAccount::new(&mut data).unwrap();

        {
            pda_account!(mut v_acc, VerificationAccount, v_acc);
            v_acc.set_state(&VerificationState::Finalized);
            v_acc.set_is_verified(&ElusivOption::Some(true));
        }

        // Invalid pool_account
        assert_matches!(
            finalize_verification_transfer_token(&r, &r, &r, &f, &f_token, &pool, &fee_c_token, &fee_c, &fee_c_token, &mut queue, &v_acc, &n_pda, &spl, &any, 0),
            Err(_)
        );

        // Invalid fee_collector_account
        assert_matches!(
            finalize_verification_transfer_token(&r, &r, &r, &f, &f_token, &pool, &pool_token, &fee_c, &pool_token, &mut queue, &v_acc, &n_pda, &spl, &any, 0),
            Err(_)
        );

        // Invalid token_program
        assert_matches!(
            finalize_verification_transfer_token(&r, &r, &r, &f, &f_token, &pool, &pool_token, &fee_c, &fee_c_token, &mut queue, &v_acc, &n_pda, &any, &any, 0),
            Err(_)
        );

        // Invalid original_fee_payer
        assert_matches!(
            finalize_verification_transfer_token(&r, &r, &r, &any, &f_token, &pool, &pool_token, &fee_c, &fee_c_token, &mut queue, &v_acc, &n_pda, &spl, &any, 0),
            Err(_)
        );

        // Invalid recipient
        assert_matches!(
            finalize_verification_transfer_token(&r, &any, &r, &f, &f_token, &pool, &pool_token, &fee_c, &fee_c_token, &mut queue, &v_acc, &n_pda, &spl, &any, 0),
            Err(_)
        );

        assert_matches!(
            finalize_verification_transfer_token(&r, &r, &r, &f, &f_token, &pool, &pool_token, &fee_c, &fee_c_token, &mut queue, &v_acc, &n_pda, &spl, &any, 0),
            Ok(())
        );

        assert_eq!(n_pda.lamports(), 0);
        assert_eq!(v_acc.lamports(), 0);
        pda_account!(v_acc, VerificationAccount, v_acc);
        assert_matches!(v_acc.get_state(), VerificationState::Closed);

        Ok(())
    }

    #[test]
    fn test_is_timestamp_valid() {
        assert!(is_timestamp_valid(0, 1));
        assert!(is_timestamp_valid(two_pow!(5) as u64 - 1, 0));

        assert!(!is_timestamp_valid(two_pow!(5) as u64, 0));
    }

    #[test]
    fn test_minimum_commitment_mt_index() {
        assert_eq!(minimum_commitment_mt_index(0, 0, 0), (0, 0));
        assert_eq!(minimum_commitment_mt_index(0, 1, 0), (1, 0));
        assert_eq!(minimum_commitment_mt_index(0, 1, 1), (2, 0));

        assert_eq!(minimum_commitment_mt_index(0, MT_COMMITMENT_COUNT as u32, 0), (0, 1));
        assert_eq!(
            minimum_commitment_mt_index(0, MT_COMMITMENT_COUNT as u32, MT_COMMITMENT_COUNT as u32 + 1),
            (1, 2)
        );
    }

    #[test]
    fn test_is_vec_duplicate_free() {
        assert!(is_vec_duplicate_free(&<Vec<u8>>::new()));
        assert!(is_vec_duplicate_free(&vec![0]));
        assert!(is_vec_duplicate_free(&vec![0, 1, 2]));

        assert!(!is_vec_duplicate_free(&vec![0, 1, 2, 0]));
        assert!(!is_vec_duplicate_free(&vec![0, 1, 0, 2]));
        assert!(!is_vec_duplicate_free(&vec![0, 0]));
    }

    #[test]
    fn test_check_join_split_public_inputs() {
        storage_account!(storage);
        nullifier_account!(n_account);

        let valid_inputs = JoinSplitPublicInputs {
            commitment_count: 1,
            roots: vec![
                Some(empty_root_raw()),
            ],
            nullifier_hashes: vec![
                RawU256::new(u256_from_str_skip_mr("1")),
            ],
            commitment: RawU256::new(u256_from_str_skip_mr("1")),
            fee_version: 0,
            amount: 0,
            fee: 123,
            token_id: 0,
        };

        let invalid_public_inputs = [
            // Zero-commitment
            mutate(&valid_inputs, |inputs| {
                inputs.commitment = RawU256::new(ZERO_COMMITMENT_RAW);
            }),

            // Invalid root for active MT
            mutate(&valid_inputs, |inputs| {
                inputs.roots[0] = Some(RawU256::new([0; 32]));
            }),

            // First root is None
            mutate(&valid_inputs, |inputs| {
                inputs.roots[0] = None;
            }),

            // Mismatched nullifier_hashes amount
            mutate(&valid_inputs, |inputs| {
                inputs.commitment_count = 2;
            }),

            // Same nullifier_hash supplied twice for same MT
            mutate(&valid_inputs, |inputs| {
                inputs.commitment_count = 2;
                inputs.nullifier_hashes = vec![
                    RawU256::new(u256_from_str_skip_mr("0")),
                    RawU256::new(u256_from_str_skip_mr("0")),
                ];
                inputs.roots.push(None);
            }),

            // Invalid root in closed MT
            mutate(&valid_inputs, |inputs| {
                inputs.commitment_count = 2;
                inputs.nullifier_hashes = vec![
                    RawU256::new(u256_from_str_skip_mr("0")),
                    RawU256::new(u256_from_str_skip_mr("0")),
                ];
                inputs.roots.push(Some(empty_root_raw()));
            }),
        ];

        for public_inputs in invalid_public_inputs {
            assert_matches!(
                check_join_split_public_inputs(&public_inputs, &storage, [&n_account, &n_account], &[0, 1]),
                Err(_)
            );
        }

        // Same MT supplied twice
        assert_matches!(
            check_join_split_public_inputs(
                &mutate(&valid_inputs, |inputs| {
                    inputs.commitment_count = 2;
                    inputs.nullifier_hashes = vec![
                        RawU256::new(u256_from_str_skip_mr("0")),
                        RawU256::new(u256_from_str_skip_mr("0")),
                    ];
                    inputs.roots.push(Some(RawU256::new(u256_from_str_skip_mr("0"))));
                }),
                &storage, [&n_account, &n_account], &[0, 0]
            ),
            Err(_)
        );
        
        // Success
        assert_matches!(
            check_join_split_public_inputs(&valid_inputs, &storage, [&n_account, &n_account], &[0, 1]),
            Ok(())
        );

        let valid_public_inputs = [
            // Same nullifier_hash supplied twice for different MT
            mutate(&valid_inputs, |inputs| {
                inputs.commitment_count = 2;
                inputs.nullifier_hashes = vec![
                    RawU256::new(u256_from_str_skip_mr("0")),
                    RawU256::new(u256_from_str_skip_mr("0")),
                ];
                inputs.roots.push(Some(RawU256::new(u256_from_str_skip_mr("0"))));
            }),
        ];

        for public_inputs in valid_public_inputs {
            assert_matches!(
                check_join_split_public_inputs(&public_inputs, &storage, [&n_account, &n_account], &[0, 1]),
                Ok(())
            );
        }

        // Duplicate nullifier_hash already exists
        let data = vec![0; NullifierAccount::ACCOUNT_SIZE];
        let pk = Pubkey::new_unique();
        account!(sub_account, pk, data);

        let mut map = HashMap::new();
        map.insert(0, &sub_account);
        let mut data = vec![0; NullifierAccount::SIZE];
        let mut n_account = NullifierAccount::new(&mut data, map).unwrap();

        n_account.try_insert_nullifier_hash(u256_from_str("1")).unwrap();

        assert_matches!(
            check_join_split_public_inputs(
                &mutate(&valid_inputs, |inputs| {
                    inputs.nullifier_hashes = vec![
                        RawU256::new(u256_from_str_skip_mr("1")),
                    ];
                }),
                &storage, [&n_account, &n_account], &[0, 1]
            ),
            Err(_)
        );
    }

    fn test_proof() -> Proof {
        proof_from_str(
            (
                "10026859857882131638516328056627849627085232677511724829502598764489185541935",
                "19685960310506634721912121951341598678325833230508240750559904196809564625591",
                false,
            ),
            (
                (
                    "857882131638516328056627849627085232677511724829502598764489185541935",
                    "685960310506634721912121951341598678325833230508240750559904196809564625591",
                ),
                (
                    "837064132573119120838379738103457054645361649757131991036638108422638197362",
                    "86803555845400161937398579081414146527572885637089779856221229551142844794",
                ),
                false,
            ),
            (
                "21186803555845400161937398579081414146527572885637089779856221229551142844794",
                "85960310506634721912121951341598678325833230508240750559904196809564625591",
                false,
            ),
        )
    }

    fn test_public_inputs() -> Vec<U256> {
        vec![
            "7889586699914970744657798935358222218486353295005298675075639741334684257960",
            "9606705614694883961284553030253534686862979817135488577431113592919470999200",
            "7548080684044753634901903467536594261850721059805517798311616241293112471457",
            "7548080684044753634901903467536594261850721059805517798311616241293112471457",
            "7548080684044753634901903467536594261850721059805517798311616241293112471457",
            "17718047633435172913528840327177336488970255844461341542131787100983543190394",
            "17718047633435172913528840327177336488970255844461341542131787100983543190394",
            "0",
            "0",
            "340282366920938463463374607431768211455",
            "340282366920938463463374607431768211455",
            "120000",
            "1657140479",
            "1",
            "2",
            "2827970856290632118729271546490749634442294169342908710567180510922374163316",
            "0",
        ].iter().map(|s| u256_from_str_skip_mr(*s)).collect()
    }
}