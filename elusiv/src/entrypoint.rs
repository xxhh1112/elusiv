use solana_program::{
    pubkey::Pubkey,
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    program_error::ProgramError,
};
use crate::instruction;
use crate::bytes::BorshSerDeSizedEnum;

crate::macros::program_id!(elusiv);
solana_program::entrypoint!(process_instruction);

#[cfg(not(tarpaulin_include))]
pub fn process_instruction(program_id: &Pubkey, accounts: &[AccountInfo], instruction_data: &[u8]) -> ProgramResult {
    if instruction_data.is_empty() { return Err(ProgramError::InvalidInstructionData) }

    match instruction::ElusivInstruction::deserialize_enum(&mut &instruction_data[..]) {
        Ok(instruction) => instruction::ElusivInstruction::process(program_id, accounts, instruction),
        Err(_) => Err(ProgramError::InvalidInstructionData)
    }
}