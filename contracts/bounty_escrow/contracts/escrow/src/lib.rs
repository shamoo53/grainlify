#![no_std]
mod events;
mod test_bounty_escrow;

use events::{
    emit_bounty_initialized, emit_funds_locked, emit_funds_refunded, emit_funds_released,
    BountyEscrowInitialized, FundsLocked, FundsRefunded, FundsReleased,
};
use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env, Vec};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    BountyExists = 3,
    BountyNotFound = 4,
    FundsNotLocked = 5,
    DeadlineNotPassed = 6,
    Unauthorized = 7,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EscrowStatus {
    Locked,
    Released,
    Refunded,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Escrow {
    pub depositor: Address,
    pub amount: i128,
    pub status: EscrowStatus,
    pub deadline: u64,
}

#[contracttype]
pub enum DataKey {
    Admin,
    Token,
    Escrow(u64),             // bounty_id
    EscrowIndex,             // Vec<u64> of all bounty_ids
    DepositorIndex(Address), // Vec<u64> of bounty_ids by depositor
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EscrowWithId {
    pub bounty_id: u64,
    pub escrow: Escrow,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AggregateStats {
    pub total_locked: i128,
    pub total_released: i128,
    pub total_refunded: i128,
    pub count_locked: u32,
    pub count_released: u32,
    pub count_refunded: u32,
}

#[contract]
pub struct BountyEscrowContract;

#[contractimpl]
impl BountyEscrowContract {
    /// Initialize the contract with the admin address and the token address (XLM).
    pub fn init(env: Env, admin: Address, token: Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);

        emit_bounty_initialized(
            &env,
            BountyEscrowInitialized {
                admin,
                token,
                timestamp: env.ledger().timestamp(),
            },
        );

        Ok(())
    }

    /// Lock funds for a specific bounty.
    pub fn lock_funds(
        env: Env,
        depositor: Address,
        bounty_id: u64,
        amount: i128,
        deadline: u64,
    ) -> Result<(), Error> {
        depositor.require_auth();

        if !env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::NotInitialized);
        }

        if env.storage().persistent().has(&DataKey::Escrow(bounty_id)) {
            return Err(Error::BountyExists);
        }

        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let client = token::Client::new(&env, &token_addr);

        // Transfer funds from depositor to contract
        client.transfer(&depositor, &env.current_contract_address(), &amount);

        let escrow = Escrow {
            depositor: depositor.clone(),
            amount,
            status: EscrowStatus::Locked,
            deadline,
        };

        // Extend the TTL of the storage entry to ensure it lives long enough
        env.storage()
            .persistent()
            .set(&DataKey::Escrow(bounty_id), &escrow);

        // Update indexes
        let mut index: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::EscrowIndex)
            .unwrap_or(Vec::new(&env));
        index.push_back(bounty_id);
        env.storage()
            .persistent()
            .set(&DataKey::EscrowIndex, &index);

        let mut depositor_index: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::DepositorIndex(depositor.clone()))
            .unwrap_or(Vec::new(&env));
        depositor_index.push_back(bounty_id);
        env.storage().persistent().set(
            &DataKey::DepositorIndex(depositor.clone()),
            &depositor_index,
        );

        // Emit value allows for off-chain indexing
        emit_funds_locked(
            &env,
            FundsLocked {
                bounty_id,
                amount,
                depositor: depositor.clone(),
                deadline,
            },
        );

        Ok(())
    }

    /// Release funds to the contributor.
    /// Only the admin (backend) can authorize this.
    pub fn release_funds(env: Env, bounty_id: u64, contributor: Address) -> Result<(), Error> {
        if !env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::NotInitialized);
        }

        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();

        if !env.storage().persistent().has(&DataKey::Escrow(bounty_id)) {
            return Err(Error::BountyNotFound);
        }

        let mut escrow: Escrow = env
            .storage()
            .persistent()
            .get(&DataKey::Escrow(bounty_id))
            .unwrap();

        if escrow.status != EscrowStatus::Locked {
            return Err(Error::FundsNotLocked);
        }

        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let client = token::Client::new(&env, &token_addr);

        // Transfer funds to contributor
        client.transfer(
            &env.current_contract_address(),
            &contributor,
            &escrow.amount,
        );

        escrow.status = EscrowStatus::Released;
        env.storage()
            .persistent()
            .set(&DataKey::Escrow(bounty_id), &escrow);

        emit_funds_released(
            &env,
            FundsReleased {
                bounty_id,
                amount: escrow.amount,
                recipient: contributor.clone(),
                timestamp: env.ledger().timestamp(),
            },
        );

        Ok(())
    }

    /// Refund funds to the original depositor if the deadline has passed.
    pub fn refund(env: Env, bounty_id: u64) -> Result<(), Error> {
        // We'll allow anyone to trigger the refund if conditions are met,
        // effectively making it permissionless but conditional.
        // OR we can require depositor auth. Let's make it permissionless to ensure funds aren't stuck if depositor key is lost,
        // but strictly logic bound.
        // However, usually refund is triggered by depositor. Let's stick to logic.

        if !env.storage().persistent().has(&DataKey::Escrow(bounty_id)) {
            return Err(Error::BountyNotFound);
        }

        let mut escrow: Escrow = env
            .storage()
            .persistent()
            .get(&DataKey::Escrow(bounty_id))
            .unwrap();

        if escrow.status != EscrowStatus::Locked {
            return Err(Error::FundsNotLocked);
        }

        let now = env.ledger().timestamp();
        if now < escrow.deadline {
            return Err(Error::DeadlineNotPassed);
        }

        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let client = token::Client::new(&env, &token_addr);

        // Transfer funds back to depositor
        client.transfer(
            &env.current_contract_address(),
            &escrow.depositor,
            &escrow.amount,
        );

        escrow.status = EscrowStatus::Refunded;
        env.storage()
            .persistent()
            .set(&DataKey::Escrow(bounty_id), &escrow);

        emit_funds_refunded(
            &env,
            FundsRefunded {
                bounty_id,
                amount: escrow.amount,
                refund_to: escrow.depositor,
                timestamp: env.ledger().timestamp(),
            },
        );

        Ok(())
    }

    /// view function to get escrow info
    pub fn get_escrow_info(env: Env, bounty_id: u64) -> Result<Escrow, Error> {
        if !env.storage().persistent().has(&DataKey::Escrow(bounty_id)) {
            return Err(Error::BountyNotFound);
        }
        Ok(env
            .storage()
            .persistent()
            .get(&DataKey::Escrow(bounty_id))
            .unwrap())
    }

    /// view function to get contract balance of the token
    pub fn get_balance(env: Env) -> Result<i128, Error> {
        if !env.storage().instance().has(&DataKey::Token) {
            return Err(Error::NotInitialized);
        }
        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let client = token::Client::new(&env, &token_addr);
        Ok(client.balance(&env.current_contract_address()))
    }

    /// Query escrows with filtering and pagination
    /// Pass 0 for min values and i128::MAX/u64::MAX for max values to disable those filters
    pub fn query_escrows_by_status(
        env: Env,
        status: EscrowStatus,
        offset: u32,
        limit: u32,
    ) -> Vec<EscrowWithId> {
        let index: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::EscrowIndex)
            .unwrap_or(Vec::new(&env));
        let mut results = Vec::new(&env);
        let mut count = 0u32;
        let mut skipped = 0u32;

        for i in 0..index.len() {
            if count >= limit {
                break;
            }

            let bounty_id = index.get(i).unwrap();
            if let Some(escrow) = env
                .storage()
                .persistent()
                .get::<DataKey, Escrow>(&DataKey::Escrow(bounty_id))
            {
                if escrow.status == status {
                    if skipped < offset {
                        skipped += 1;
                        continue;
                    }
                    results.push_back(EscrowWithId { bounty_id, escrow });
                    count += 1;
                }
            }
        }
        results
    }

    /// Query escrows with amount range filtering
    pub fn query_escrows_by_amount(
        env: Env,
        min_amount: i128,
        max_amount: i128,
        offset: u32,
        limit: u32,
    ) -> Vec<EscrowWithId> {
        let index: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::EscrowIndex)
            .unwrap_or(Vec::new(&env));
        let mut results = Vec::new(&env);
        let mut count = 0u32;
        let mut skipped = 0u32;

        for i in 0..index.len() {
            if count >= limit {
                break;
            }

            let bounty_id = index.get(i).unwrap();
            if let Some(escrow) = env
                .storage()
                .persistent()
                .get::<DataKey, Escrow>(&DataKey::Escrow(bounty_id))
            {
                if escrow.amount >= min_amount && escrow.amount <= max_amount {
                    if skipped < offset {
                        skipped += 1;
                        continue;
                    }
                    results.push_back(EscrowWithId { bounty_id, escrow });
                    count += 1;
                }
            }
        }
        results
    }

    /// Query escrows with deadline range filtering
    pub fn query_escrows_by_deadline(
        env: Env,
        min_deadline: u64,
        max_deadline: u64,
        offset: u32,
        limit: u32,
    ) -> Vec<EscrowWithId> {
        let index: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::EscrowIndex)
            .unwrap_or(Vec::new(&env));
        let mut results = Vec::new(&env);
        let mut count = 0u32;
        let mut skipped = 0u32;

        for i in 0..index.len() {
            if count >= limit {
                break;
            }

            let bounty_id = index.get(i).unwrap();
            if let Some(escrow) = env
                .storage()
                .persistent()
                .get::<DataKey, Escrow>(&DataKey::Escrow(bounty_id))
            {
                if escrow.deadline >= min_deadline && escrow.deadline <= max_deadline {
                    if skipped < offset {
                        skipped += 1;
                        continue;
                    }
                    results.push_back(EscrowWithId { bounty_id, escrow });
                    count += 1;
                }
            }
        }
        results
    }

    /// Query escrows by depositor
    pub fn query_escrows_by_depositor(
        env: Env,
        depositor: Address,
        offset: u32,
        limit: u32,
    ) -> Vec<EscrowWithId> {
        let index: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::DepositorIndex(depositor))
            .unwrap_or(Vec::new(&env));
        let mut results = Vec::new(&env);
        let start = offset.min(index.len());
        let end = (offset + limit).min(index.len());

        for i in start..end {
            let bounty_id = index.get(i).unwrap();
            if let Some(escrow) = env
                .storage()
                .persistent()
                .get::<DataKey, Escrow>(&DataKey::Escrow(bounty_id))
            {
                results.push_back(EscrowWithId { bounty_id, escrow });
            }
        }
        results
    }

    /// Get aggregate statistics
    pub fn get_aggregate_stats(env: Env) -> AggregateStats {
        let index: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::EscrowIndex)
            .unwrap_or(Vec::new(&env));
        let mut stats = AggregateStats {
            total_locked: 0,
            total_released: 0,
            total_refunded: 0,
            count_locked: 0,
            count_released: 0,
            count_refunded: 0,
        };

        for i in 0..index.len() {
            let bounty_id = index.get(i).unwrap();
            if let Some(escrow) = env
                .storage()
                .persistent()
                .get::<DataKey, Escrow>(&DataKey::Escrow(bounty_id))
            {
                match escrow.status {
                    EscrowStatus::Locked => {
                        stats.total_locked += escrow.amount;
                        stats.count_locked += 1;
                    }
                    EscrowStatus::Released => {
                        stats.total_released += escrow.amount;
                        stats.count_released += 1;
                    }
                    EscrowStatus::Refunded => {
                        stats.total_refunded += escrow.amount;
                        stats.count_refunded += 1;
                    }
                }
            }
        }
        stats
    }

    /// Get total count of escrows
    pub fn get_escrow_count(env: Env) -> u32 {
        let index: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::EscrowIndex)
            .unwrap_or(Vec::new(&env));
        index.len()
    }

    /// Get escrow IDs by status
    pub fn get_escrow_ids_by_status(
        env: Env,
        status: EscrowStatus,
        offset: u32,
        limit: u32,
    ) -> Vec<u64> {
        let index: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::EscrowIndex)
            .unwrap_or(Vec::new(&env));
        let mut results = Vec::new(&env);
        let mut count = 0u32;
        let mut skipped = 0u32;

        for i in 0..index.len() {
            if count >= limit {
                break;
            }
            let bounty_id = index.get(i).unwrap();
            if let Some(escrow) = env
                .storage()
                .persistent()
                .get::<DataKey, Escrow>(&DataKey::Escrow(bounty_id))
            {
                if escrow.status == status {
                    if skipped < offset {
                        skipped += 1;
                        continue;
                    }
                    results.push_back(bounty_id);
                    count += 1;
                }
            }
        }
        results
    }
}

#[cfg(test)]
mod test;
