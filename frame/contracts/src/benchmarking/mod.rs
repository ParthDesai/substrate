// This file is part of Substrate.

// Copyright (C) 2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Benchmarks for the contracts pallet

#![cfg(feature = "runtime-benchmarks")]

mod code;
mod sandbox;

use crate::{
	*, Module as Contracts,
	exec::StorageKey,
	rent::Rent,
	schedule::{API_BENCHMARK_BATCH_SIZE, INSTR_BENCHMARK_BATCH_SIZE},
	storage::Storage,
};
use self::{
	code::{
		body::{self, DynInstr::*},
		ModuleDefinition, DataSegment, ImportedMemory, ImportedFunction, WasmModule,
	},
	sandbox::Sandbox,
};
use frame_benchmarking::{benchmarks, account, whitelisted_caller};
use frame_system::{Module as System, RawOrigin};
use parity_wasm::elements::{Instruction, ValueType, BlockType};
use sp_runtime::traits::{Hash, Bounded, Zero};
use sp_std::{default::Default, convert::{TryInto}, vec::Vec, vec};
use pallet_contracts_primitives::RentProjection;

/// How many batches we do per API benchmark.
const API_BENCHMARK_BATCHES: u32 = 20;

/// How many batches we do per Instruction benchmark.
const INSTR_BENCHMARK_BATCHES: u32 = 1;

/// An instantiated and deployed contract.
struct Contract<T: Config> {
	caller: T::AccountId,
	account_id: T::AccountId,
	addr: <T::Lookup as StaticLookup>::Source,
	endowment: BalanceOf<T>,
	code_hash: <T::Hashing as Hash>::Output,
}

/// Describes how much balance should be transferred on instantiate from the caller.
enum Endow {
	/// Endow the contract with a maximum amount of balance. This value is described by
	/// `Contract::max_endowment`.
	Max,
	/// Endow so that the amount of balance that is transferred is big but not so big
	/// to offset the rent payment. This is needed in order to test rent collection.
	CollectRent,
}

impl Endow {
	/// The maximum amount of balance a caller can transfer without being brought below
	/// the existential deposit. This assumes that every caller is funded with the amount
	/// returned by `caller_funding`.
	fn max<T:Config>() -> BalanceOf<T> {
		caller_funding::<T>().saturating_sub(T::Currency::minimum_balance())
	}
}

impl<T: Config> Contract<T>
where
	T: Config,
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>,
{
	/// Create new contract and use a default account id as instantiator.
	fn new(
		module: WasmModule<T>,
		data: Vec<u8>,
		endowment: Endow,
	) -> Result<Contract<T>, &'static str> {
		Self::with_index(0, module, data, endowment)
	}

	/// Create new contract and use an account id derived from the supplied index as instantiator.
	fn with_index(
		index: u32,
		module: WasmModule<T>,
		data: Vec<u8>,
		endowment: Endow,
	) -> Result<Contract<T>, &'static str> {
		Self::with_caller(account("instantiator", index, 0), module, data, endowment)
	}

	/// Create new contract and use the supplied `caller` as instantiator.
	fn with_caller(
		caller: T::AccountId,
		module: WasmModule<T>,
		data: Vec<u8>,
		endowment: Endow,
	) -> Result<Contract<T>, &'static str>
	{
		use sp_runtime::traits::{CheckedDiv, SaturatedConversion};
		let (storage_size, endowment) = match endowment {
			Endow::CollectRent => {
				// storage_size cannot be zero because otherwise a contract that is just above
				// the subsistence threshold does not pay rent given a large enough subsistence
				// threshold. But we need rent payments to occur in order to benchmark for worst cases.
				let storage_size = ConfigCache::<T>::subsistence_threshold_uncached()
					.checked_div(&T::RentDepositOffset::get())
					.unwrap_or_else(Zero::zero);

				// Endowment should be large but not as large to inhibit rent payments.
				let endowment = T::RentDepositOffset::get()
					.saturating_mul(storage_size + T::StorageSizeOffset::get().into())
					.saturating_sub(1u32.into());

				(storage_size, endowment)
			},
			Endow::Max => (0u32.into(), Endow::max::<T>()),
		};
		T::Currency::make_free_balance_be(&caller, caller_funding::<T>());
		let salt = vec![0xff];
		let addr = Contracts::<T>::contract_address(&caller, &module.hash, &salt);

		// The default block number is zero. The benchmarking system bumps the block number
		// to one for the benchmarking closure when it is set to zero. In order to prevent this
		// undesired implicit bump (which messes with rent collection), we do the bump ourselves
		// in the setup closure so that both the instantiate and subsequent call are run with the
		// same block number.
		System::<T>::set_block_number(1u32.into());

		Contracts::<T>::put_code_raw(module.code)?;
		Contracts::<T>::instantiate(
			RawOrigin::Signed(caller.clone()).into(),
			endowment,
			Weight::max_value(),
			module.hash,
			data,
			salt,
		)?;

		let result = Contract {
			caller,
			account_id: addr.clone(),
			addr: T::Lookup::unlookup(addr),
			endowment,
			code_hash: module.hash.clone(),
		};

		let mut contract = result.alive_info()?;
		contract.storage_size = storage_size.saturated_into::<u32>();
		ContractInfoOf::<T>::insert(&result.account_id, ContractInfo::Alive(contract));

		Ok(result)
	}

	/// Store the supplied storage items into this contracts storage.
	fn store(&self, items: &Vec<(StorageKey, Vec<u8>)>) -> Result<(), &'static str> {
		let info = self.alive_info()?;
		for item in items {
			Storage::<T>::write(
				&self.account_id,
				&info.trie_id,
				&item.0,
				Some(item.1.clone()),
			)
			.map_err(|_| "Failed to write storage to restoration dest")?;
		}
		Ok(())
	}

	/// Get the `AliveContractInfo` of the `addr` or an error if it is no longer alive.
	fn address_alive_info(addr: &T::AccountId) -> Result<AliveContractInfo<T>, &'static str> {
		ContractInfoOf::<T>::get(addr).and_then(|c| c.get_alive())
			.ok_or("Expected contract to be alive at this point.")
	}

	/// Get the `AliveContractInfo` of this contract or an error if it is no longer alive.
	fn alive_info(&self) -> Result<AliveContractInfo<T>, &'static str> {
		Self::address_alive_info(&self.account_id)
	}

	/// Return an error if this contract is no tombstone.
	fn ensure_tombstone(&self) -> Result<(), &'static str> {
		ContractInfoOf::<T>::get(&self.account_id).and_then(|c| c.get_tombstone())
			.ok_or("Expected contract to be a tombstone at this point.")
			.map(|_| ())
	}

	/// Get the block number when this contract will be evicted. Returns an error when
	/// the rent collection won't happen because the contract has to much endowment.
	fn eviction_at(&self) -> Result<T::BlockNumber, &'static str> {
		let projection = Rent::<T>::compute_projection(&self.account_id)
			.map_err(|_| "Invalid acc for rent")?;
		match projection {
			RentProjection::EvictionAt(at) => Ok(at),
			_ => Err("Account does not pay rent.")?,
		}
	}
}

/// A `Contract` that contains some storage items.
///
/// This is used to benchmark contract destruction and resurection. Those operations'
/// weight depend on the amount of storage accumulated.
struct ContractWithStorage<T: Config> {
	/// The contract that was evicted.
	contract: Contract<T>,
	/// The storage the contract held when it was avicted.
	storage: Vec<(StorageKey, Vec<u8>)>,
}

impl<T: Config> ContractWithStorage<T>
where
	T: Config,
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>,
{
	/// Same as [`Self::with_code`] but with dummy contract code.
	fn new(stor_num: u32, stor_size: u32) -> Result<Self, &'static str> {
		Self::with_code(WasmModule::dummy(), stor_num, stor_size)
	}

	/// Create and evict a new contract with the supplied storage item count and size each.
	fn with_code(code: WasmModule<T>, stor_num: u32, stor_size: u32) -> Result<Self, &'static str> {
		let contract = Contract::<T>::new(code, vec![], Endow::CollectRent)?;
		let storage_items = create_storage::<T>(stor_num, stor_size)?;
		contract.store(&storage_items)?;
		Ok(Self {
			contract,
			storage: storage_items,
		})
	}

	/// Increase the system block number so that this contract is eligible for eviction.
	fn set_block_num_for_eviction(&self) -> Result<(), &'static str>  {
		System::<T>::set_block_number(
			self.contract.eviction_at()? + T::SignedClaimHandicap::get() + 5u32.into()
		);
		Ok(())
	}

	/// Evict this contract.
	fn evict(&mut self) -> Result<(), &'static str> {
		self.set_block_num_for_eviction()?;
		Rent::<T>::snitch_contract_should_be_evicted(&self.contract.account_id, Zero::zero())?;
		self.contract.ensure_tombstone()
	}
}

/// Generate `stor_num` storage items. Each has the size `stor_size`.
fn create_storage<T: Config>(
	stor_num: u32,
	stor_size: u32
) -> Result<Vec<(StorageKey, Vec<u8>)>, &'static str> {
	(0..stor_num).map(|i| {
		let hash = T::Hashing::hash_of(&i)
			.as_ref()
			.try_into()
			.map_err(|_| "Hash too big for storage key")?;
		Ok((hash, vec![42u8; stor_size as usize]))
	}).collect::<Result<Vec<_>, &'static str>>()
}

/// The funding that each account that either calls or instantiates contracts is funded with.
fn caller_funding<T: Config>() -> BalanceOf<T> {
	BalanceOf::<T>::max_value() / 2u32.into()
}

benchmarks! {
	where_clause { where
		T::AccountId: UncheckedFrom<T::Hash>,
		T::AccountId: AsRef<[u8]>,
	}

	_ {
	}

	// The base weight without any actual work performed apart from the setup costs.
	on_initialize {}: {
		Storage::<T>::process_deletion_queue_batch(Weight::max_value())
	}

	on_initialize_per_trie_key {
		let k in 0..1024;
		let instance = ContractWithStorage::<T>::new(k, T::MaxValueSize::get())?;
		Storage::<T>::queue_trie_for_deletion(&instance.contract.alive_info()?)?;
	}: {
		Storage::<T>::process_deletion_queue_batch(Weight::max_value())
	}

	on_initialize_per_queue_item {
		let q in 0..1024.min(T::DeletionQueueDepth::get());
		for i in 0 .. q {
			let instance = Contract::<T>::with_index(i, WasmModule::dummy(), vec![], Endow::Max)?;
			Storage::<T>::queue_trie_for_deletion(&instance.alive_info()?)?;
			ContractInfoOf::<T>::remove(instance.account_id);
		}
	}: {
		Storage::<T>::process_deletion_queue_batch(Weight::max_value())
	}

	// This extrinsic is pretty much constant as it is only a simple setter.
	update_schedule {
		let schedule = Schedule {
			version: 1,
			.. Default::default()
		};
	}: _(RawOrigin::Root, schedule)

	// This constructs a contract that is maximal expensive to instrument.
	// It creates a maximum number of metering blocks per byte.
	// `n`: Size of the code in kilobytes.
	put_code {
		let n in 0 .. Contracts::<T>::current_schedule().limits.code_size / 1024;
		let caller = whitelisted_caller();
		T::Currency::make_free_balance_be(&caller, caller_funding::<T>());
		let module = WasmModule::<T>::sized(n * 1024);
		let origin = RawOrigin::Signed(caller);
	}: _(origin, module.code)

	// Instantiate uses a dummy contract constructor to measure the overhead of the instantiate.
	// The size of the input data influences the runtime because it is hashed in order to determine
	// the contract address.
	// `n`: Size of the data passed to constructor in kilobytes.
	// `s`: Size of the salt in kilobytes.
	instantiate {
		let n in 0 .. code::max_pages::<T>() * 64;
		let s in 0 .. code::max_pages::<T>() * 64;
		let data = vec![42u8; (n * 1024) as usize];
		let salt = vec![42u8; (s * 1024) as usize];
		let endowment = ConfigCache::<T>::subsistence_threshold_uncached();
		let caller = whitelisted_caller();
		T::Currency::make_free_balance_be(&caller, caller_funding::<T>());
		let WasmModule { code, hash, .. } = WasmModule::<T>::dummy_with_mem();
		let origin = RawOrigin::Signed(caller.clone());
		let addr = Contracts::<T>::contract_address(&caller, &hash, &salt);
		Contracts::<T>::put_code_raw(code)?;
	}: _(origin, endowment, Weight::max_value(), hash, data, salt)
	verify {
		// endowment was removed from the caller
		assert_eq!(T::Currency::free_balance(&caller), caller_funding::<T>() - endowment);
		// contract has the full endowment because no rent collection happended
		assert_eq!(T::Currency::free_balance(&addr), endowment);
		// instantiate should leave a alive contract
		Contract::<T>::address_alive_info(&addr)?;
	}

	// We just call a dummy contract to measure to overhead of the call extrinsic.
	// The size of the data has no influence on the costs of this extrinsic as long as the contract
	// won't call `seal_input` in its constructor to copy the data to contract memory.
	// The dummy contract used here does not do this. The costs for the data copy is billed as
	// part of `seal_input`.
	call {
		let data = vec![42u8; 1024];
		let instance = Contract::<T>::with_caller(
			whitelisted_caller(), WasmModule::dummy_with_mem(), vec![], Endow::CollectRent
		)?;
		let value = T::Currency::minimum_balance() * 100u32.into();
		let origin = RawOrigin::Signed(instance.caller.clone());
		let callee = instance.addr.clone();

		// trigger rent collection for worst case performance of call
		System::<T>::set_block_number(instance.eviction_at()? - 5u32.into());
		let before = T::Currency::free_balance(&instance.account_id);
	}: _(origin, callee, value, Weight::max_value(), data)
	verify {
		// endowment and value transfered via call should be removed from the caller
		assert_eq!(
			T::Currency::free_balance(&instance.caller),
			caller_funding::<T>() - instance.endowment - value,
		);
		// rent should have lowered the amount of balance of the contract
		assert!(T::Currency::free_balance(&instance.account_id) < before + value);
		// but it should not have been evicted by the rent collection
		instance.alive_info()?;
	}

	// We benchmark the costs for sucessfully evicting an empty contract.
	// The actual costs are depending on how many storage items the evicted contract
	// does have. However, those costs are not to be payed by the sender but
	// will be distributed over multiple blocks using a scheduler. Otherwise there is
	// no incentive to remove large contracts when the removal is more expensive than
	// the reward for removing them.
	claim_surcharge {
		let instance = Contract::<T>::with_caller(
			whitelisted_caller(), WasmModule::dummy(), vec![], Endow::CollectRent
		)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
		let account_id = instance.account_id.clone();

		// instantiate should leave us with an alive contract
		instance.alive_info()?;

		// generate enough rent so that the contract is evicted
		System::<T>::set_block_number(
			instance.eviction_at()? + T::SignedClaimHandicap::get() + 5u32.into()
		);
	}: _(origin, account_id, None)
	verify {
		// the claim surcharge should have evicted the contract
		instance.ensure_tombstone()?;

		// the caller should get the reward for being a good snitch
		assert_eq!(
			T::Currency::free_balance(&instance.caller),
			caller_funding::<T>() - instance.endowment + <T as Config>::SurchargeReward::get(),
		);
	}

	seal_caller {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = Contract::<T>::new(WasmModule::getter(
			"seal_caller", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	seal_address {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = Contract::<T>::new(WasmModule::getter(
			"seal_address", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	seal_gas_left {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = Contract::<T>::new(WasmModule::getter(
			"seal_gas_left", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	seal_balance {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = Contract::<T>::new(WasmModule::getter(
			"seal_balance", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	seal_value_transferred {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = Contract::<T>::new(WasmModule::getter(
			"seal_value_transferred", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	seal_minimum_balance {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = Contract::<T>::new(WasmModule::getter(
			"seal_minimum_balance", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	seal_tombstone_deposit {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = Contract::<T>::new(WasmModule::getter(
			"seal_tombstone_deposit", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	seal_rent_allowance {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = Contract::<T>::new(WasmModule::getter(
			"seal_rent_allowance", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	seal_block_number {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = Contract::<T>::new(WasmModule::getter(
			"seal_block_number", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	seal_now {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = Contract::<T>::new(WasmModule::getter(
			"seal_now", r * API_BENCHMARK_BATCH_SIZE
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	seal_weight_to_fee {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let pages = code::max_pages::<T>();
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_weight_to_fee",
				params: vec![ValueType::I64, ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![DataSegment {
				offset: 0,
				value: (pages * 64 * 1024 - 4).to_le_bytes().to_vec(),
			}],
			call_body: Some(body::repeated(r * API_BENCHMARK_BATCH_SIZE, &[
				Instruction::I64Const(500_000),
				Instruction::I32Const(4),
				Instruction::I32Const(0),
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	seal_gas {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let code = WasmModule::<T>::from(ModuleDefinition {
			imported_functions: vec![ImportedFunction {
				name: "gas",
				params: vec![ValueType::I32],
				return_type: None,
			}],
			call_body: Some(body::repeated(r * API_BENCHMARK_BATCH_SIZE, &[
				Instruction::I32Const(42),
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());

	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// We cannot call seal_input multiple times. Therefore our weight determination is not
	// as precise as with other APIs. Because this function can only be called once per
	// contract it cannot be used for Dos.
	seal_input {
		let r in 0 .. 1;
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_input",
				params: vec![ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: 0u32.to_le_bytes().to_vec(),
				},
			],
			call_body: Some(body::repeated(r, &[
				Instruction::I32Const(4), // ptr where to store output
				Instruction::I32Const(0), // ptr to length
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	seal_input_per_kb {
		let n in 0 .. code::max_pages::<T>() * 64;
		let pages = code::max_pages::<T>();
		let buffer_size = pages * 64 * 1024 - 4;
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_input",
				params: vec![ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: buffer_size.to_le_bytes().to_vec(),
				},
			],
			call_body: Some(body::plain(vec![
				Instruction::I32Const(4), // ptr where to store output
				Instruction::I32Const(0), // ptr to length
				Instruction::Call(0),
				Instruction::End,
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let data = vec![42u8; (n * 1024).min(buffer_size) as usize];
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), data)

	// The same argument as for `seal_input` is true here.
	seal_return {
		let r in 0 .. 1;
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_return",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			call_body: Some(body::repeated(r, &[
				Instruction::I32Const(0), // flags
				Instruction::I32Const(0), // data_ptr
				Instruction::I32Const(0), // data_len
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	seal_return_per_kb {
		let n in 0 .. code::max_pages::<T>() * 64;
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_return",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			call_body: Some(body::plain(vec![
				Instruction::I32Const(0), // flags
				Instruction::I32Const(0), // data_ptr
				Instruction::I32Const((n * 1024) as i32), // data_len
				Instruction::Call(0),
				Instruction::End,
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// The same argument as for `seal_input` is true here.
	seal_terminate {
		let r in 0 .. 1;
		let beneficiary = account::<T::AccountId>("beneficiary", 0, 0);
		let beneficiary_bytes = beneficiary.encode();
		let beneficiary_len = beneficiary_bytes.len();
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_terminate",
				params: vec![ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: beneficiary_bytes,
				},
			],
			call_body: Some(body::repeated(r, &[
				Instruction::I32Const(0), // beneficiary_ptr
				Instruction::I32Const(beneficiary_len as i32), // beneficiary_len
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
		assert_eq!(T::Currency::total_balance(&beneficiary), 0u32.into());
		assert_eq!(T::Currency::total_balance(&instance.account_id), Endow::max::<T>());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])
	verify {
		if r > 0 {
			assert_eq!(T::Currency::total_balance(&instance.account_id), 0u32.into());
			assert_eq!(T::Currency::total_balance(&beneficiary), Endow::max::<T>());
		}
	}

	seal_restore_to {
		let r in 0 .. 1;

		// Restore just moves the trie id from origin to destination and therefore
		// does not depend on the size of the destination contract. However, to not
		// trigger any edge case we won't use an empty contract as destination.
		let mut tombstone = ContractWithStorage::<T>::new(10, T::MaxValueSize::get())?;
		tombstone.evict()?;

		let dest = tombstone.contract.account_id.encode();
		let dest_len = dest.len();
		let code_hash = tombstone.contract.code_hash.encode();
		let code_hash_len = code_hash.len();
		let rent_allowance = BalanceOf::<T>::max_value().encode();
		let rent_allowance_len = rent_allowance.len();

		let dest_offset = 0;
		let code_hash_offset = dest_offset + dest_len;
		let rent_allowance_offset = code_hash_offset + code_hash_len;

		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_restore_to",
				params: vec![
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
				],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: dest_offset as u32,
					value: dest,
				},
				DataSegment {
					offset: code_hash_offset as u32,
					value: code_hash,
				},
				DataSegment {
					offset: rent_allowance_offset as u32,
					value: rent_allowance,
				},
			],
			call_body: Some(body::repeated(r, &[
				Instruction::I32Const(dest_offset as i32),
				Instruction::I32Const(dest_len as i32),
				Instruction::I32Const(code_hash_offset as i32),
				Instruction::I32Const(code_hash_len as i32),
				Instruction::I32Const(rent_allowance_offset as i32),
				Instruction::I32Const(rent_allowance_len as i32),
				Instruction::I32Const(0), // delta_ptr
				Instruction::I32Const(0), // delta_count
				Instruction::Call(0),
			])),
			.. Default::default()
		});

		let instance = Contract::<T>::with_caller(
			account("origin", 0, 0), code, vec![], Endow::Max
		)?;
		instance.store(&tombstone.storage)?;
		System::<T>::set_block_number(System::<T>::block_number() + 1u32.into());

		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])
	verify {
		if r > 0 {
			tombstone.contract.alive_info()?;
		}
	}

	seal_restore_to_per_delta {
		let d in 0 .. API_BENCHMARK_BATCHES;
		let mut tombstone = ContractWithStorage::<T>::new(0, 0)?;
		tombstone.evict()?;
		let delta = create_storage::<T>(d * API_BENCHMARK_BATCH_SIZE, T::MaxValueSize::get())?;

		let dest = tombstone.contract.account_id.encode();
		let dest_len = dest.len();
		let code_hash = tombstone.contract.code_hash.encode();
		let code_hash_len = code_hash.len();
		let rent_allowance = BalanceOf::<T>::max_value().encode();
		let rent_allowance_len = rent_allowance.len();
		let delta_keys = delta.iter().flat_map(|(key, _)| key).cloned().collect::<Vec<_>>();

		let dest_offset = 0;
		let code_hash_offset = dest_offset + dest_len;
		let rent_allowance_offset = code_hash_offset + code_hash_len;
		let delta_keys_offset = rent_allowance_offset + rent_allowance_len;

		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_restore_to",
				params: vec![
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
				],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: dest_offset as u32,
					value: dest,
				},
				DataSegment {
					offset: code_hash_offset as u32,
					value: code_hash,
				},
				DataSegment {
					offset: rent_allowance_offset as u32,
					value: rent_allowance,
				},
				DataSegment {
					offset: delta_keys_offset as u32,
					value: delta_keys,
				},
			],
			call_body: Some(body::plain(vec![
				Instruction::I32Const(dest_offset as i32),
				Instruction::I32Const(dest_len as i32),
				Instruction::I32Const(code_hash_offset as i32),
				Instruction::I32Const(code_hash_len as i32),
				Instruction::I32Const(rent_allowance_offset as i32),
				Instruction::I32Const(rent_allowance_len as i32),
				Instruction::I32Const(delta_keys_offset as i32), // delta_ptr
				Instruction::I32Const(delta.len() as i32), // delta_count
				Instruction::Call(0),
				Instruction::End,
			])),
			.. Default::default()
		});

		let instance = Contract::<T>::with_caller(
			account("origin", 0, 0), code, vec![], Endow::Max
		)?;
		instance.store(&tombstone.storage)?;
		instance.store(&delta)?;
		System::<T>::set_block_number(System::<T>::block_number() + 1u32.into());

		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])
	verify {
		tombstone.contract.alive_info()?;
	}

	// We benchmark only for the maximum subject length. We assume that this is some lowish
	// number (< 1 KB). Therefore we are not overcharging too much in case a smaller subject is
	// used.
	seal_random {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let pages = code::max_pages::<T>();
		let subject_len = Contracts::<T>::current_schedule().limits.subject_len;
		assert!(subject_len < 1024);
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_random",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: (pages * 64 * 1024 - subject_len - 4).to_le_bytes().to_vec(),
				},
			],
			call_body: Some(body::repeated(r * API_BENCHMARK_BATCH_SIZE, &[
				Instruction::I32Const(4), // subject_ptr
				Instruction::I32Const(subject_len as i32), // subject_len
				Instruction::I32Const((subject_len + 4) as i32), // out_ptr
				Instruction::I32Const(0),	// out_len_ptr
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// Overhead of calling the function without any topic.
	// We benchmark for the worst case (largest event).
	seal_deposit_event {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_deposit_event",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			call_body: Some(body::repeated(r * API_BENCHMARK_BATCH_SIZE, &[
				Instruction::I32Const(0), // topics_ptr
				Instruction::I32Const(0), // topics_len
				Instruction::I32Const(0), // data_ptr
				Instruction::I32Const(0), // data_len
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// Benchmark the overhead that topics generate.
	// `t`: Number of topics
	// `n`: Size of event payload in kb
	seal_deposit_event_per_topic_and_kb {
		let t in 0 .. Contracts::<T>::current_schedule().limits.event_topics;
		let n in 0 .. T::MaxValueSize::get() / 1024;
		let mut topics = (0..API_BENCHMARK_BATCH_SIZE)
			.map(|n| (n * t..n * t + t).map(|i| T::Hashing::hash_of(&i)).collect::<Vec<_>>().encode())
			.peekable();
		let topics_len = topics.peek().map(|i| i.len()).unwrap_or(0);
		let topics = topics.flatten().collect();
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_deposit_event",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: topics,
				},
			],
			call_body: Some(body::repeated_dyn(API_BENCHMARK_BATCH_SIZE, vec![
				Counter(0, topics_len as u32), // topics_ptr
				Regular(Instruction::I32Const(topics_len as i32)), // topics_len
				Regular(Instruction::I32Const(0)), // data_ptr
				Regular(Instruction::I32Const((n * 1024) as i32)), // data_len
				Regular(Instruction::Call(0)),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	seal_set_rent_allowance {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let allowance = caller_funding::<T>().encode();
		let allowance_len = allowance.len();
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory { min_pages: 1, max_pages: 1 }),
			imported_functions: vec![ImportedFunction {
				name: "seal_set_rent_allowance",
				params: vec![ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: allowance,
				},
			],
			call_body: Some(body::repeated(r * API_BENCHMARK_BATCH_SIZE, &[
				Instruction::I32Const(0), // value_ptr
				Instruction::I32Const(allowance_len as i32), // value_len
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// Only the overhead of calling the function itself with minimal arguments.
	// The contract is a bit more complex because I needs to use different keys in order
	// to generate unique storage accesses. However, it is still dominated by the storage
	// accesses.
	seal_set_storage {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let keys = (0 .. r * API_BENCHMARK_BATCH_SIZE)
			.flat_map(|n| T::Hashing::hash_of(&n).as_ref().to_vec())
			.collect::<Vec<_>>();
		let key_len = sp_std::mem::size_of::<<T::Hashing as sp_runtime::traits::Hash>::Output>();
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_set_storage",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: keys,
				},
			],
			call_body: Some(body::repeated_dyn(r * API_BENCHMARK_BATCH_SIZE, vec![
				Counter(0, key_len as u32), // key_ptr
				Regular(Instruction::I32Const(0)), // value_ptr
				Regular(Instruction::I32Const(0)), // value_len
				Regular(Instruction::Call(0)),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	seal_set_storage_per_kb {
		let n in 0 .. T::MaxValueSize::get() / 1024;
		let key = T::Hashing::hash_of(&1u32).as_ref().to_vec();
		let key_len = key.len();
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_set_storage",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: key,
				},
			],
			call_body: Some(body::repeated(API_BENCHMARK_BATCH_SIZE, &[
				Instruction::I32Const(0), // key_ptr
				Instruction::I32Const(0), // value_ptr
				Instruction::I32Const((n * 1024) as i32), // value_len
				Instruction::Call(0),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// Similar to seal_set_storage. However, we store all the keys that we are about to
	// delete beforehand in order to prevent any optimizations that could occur when
	// deleting a non existing key.
	seal_clear_storage {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let keys = (0 .. r * API_BENCHMARK_BATCH_SIZE)
			.map(|n| T::Hashing::hash_of(&n).as_ref().to_vec())
			.collect::<Vec<_>>();
		let key_bytes = keys.iter().flatten().cloned().collect::<Vec<_>>();
		let key_len = sp_std::mem::size_of::<<T::Hashing as sp_runtime::traits::Hash>::Output>();
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_clear_storage",
				params: vec![ValueType::I32],
				return_type: None,
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: key_bytes,
				},
			],
			call_body: Some(body::repeated_dyn(r * API_BENCHMARK_BATCH_SIZE, vec![
				Counter(0, key_len as u32),
				Regular(Instruction::Call(0)),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let trie_id = instance.alive_info()?.trie_id;
		for key in keys {
			Storage::<T>::write(
				&instance.account_id,
				&trie_id,
				key.as_slice().try_into().map_err(|e| "Key has wrong length")?,
				Some(vec![42; T::MaxValueSize::get() as usize])
			)
			.map_err(|_| "Failed to write to storage during setup.")?;
		}
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// We make sure that all storage accesses are to unique keys.
	seal_get_storage {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let keys = (0 .. r * API_BENCHMARK_BATCH_SIZE)
			.map(|n| T::Hashing::hash_of(&n).as_ref().to_vec())
			.collect::<Vec<_>>();
		let key_len = sp_std::mem::size_of::<<T::Hashing as sp_runtime::traits::Hash>::Output>();
		let key_bytes = keys.iter().flatten().cloned().collect::<Vec<_>>();
		let key_bytes_len = key_bytes.len();
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_get_storage",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: Some(ValueType::I32),
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: key_bytes,
				},
			],
			call_body: Some(body::repeated_dyn(r * API_BENCHMARK_BATCH_SIZE, vec![
				Counter(0, key_len as u32), // key_ptr
				Regular(Instruction::I32Const((key_bytes_len + 4) as i32)), // out_ptr
				Regular(Instruction::I32Const(key_bytes_len as i32)), // out_len_ptr
				Regular(Instruction::Call(0)),
				Regular(Instruction::Drop),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let trie_id = instance.alive_info()?.trie_id;
		for key in keys {
			Storage::<T>::write(
				&instance.account_id,
				&trie_id,
				key.as_slice().try_into().map_err(|e| "Key has wrong length")?,
				Some(vec![])
			)
			.map_err(|_| "Failed to write to storage during setup.")?;
		}
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	seal_get_storage_per_kb {
		let n in 0 .. T::MaxValueSize::get() / 1024;
		let key = T::Hashing::hash_of(&1u32).as_ref().to_vec();
		let key_len = key.len();
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_get_storage",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: Some(ValueType::I32),
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: key.clone(),
				},
				DataSegment {
					offset: key_len as u32,
					value: T::MaxValueSize::get().to_le_bytes().into(),
				},
			],
			call_body: Some(body::repeated(API_BENCHMARK_BATCH_SIZE, &[
				// call at key_ptr
				Instruction::I32Const(0), // key_ptr
				Instruction::I32Const((key_len + 4) as i32), // out_ptr
				Instruction::I32Const(key_len as i32), // out_len_ptr
				Instruction::Call(0),
				Instruction::Drop,
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let trie_id = instance.alive_info()?.trie_id;
		Storage::<T>::write(
			&instance.account_id,
			&trie_id,
			key.as_slice().try_into().map_err(|e| "Key has wrong length")?,
			Some(vec![42u8; (n * 1024) as usize])
		)
		.map_err(|_| "Failed to write to storage during setup.")?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// We transfer to unique accounts.
	seal_transfer {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let accounts = (0..r * API_BENCHMARK_BATCH_SIZE)
			.map(|i| account::<T::AccountId>("receiver", i, 0))
			.collect::<Vec<_>>();
		let account_len = accounts.get(0).map(|i| i.encode().len()).unwrap_or(0);
		let account_bytes = accounts.iter().flat_map(|x| x.encode()).collect();
		let value = ConfigCache::<T>::subsistence_threshold_uncached();
		assert!(value > 0u32.into());
		let value_bytes = value.encode();
		let value_len = value_bytes.len();
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_transfer",
				params: vec![ValueType::I32, ValueType::I32, ValueType::I32, ValueType::I32],
				return_type: Some(ValueType::I32),
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: value_bytes,
				},
				DataSegment {
					offset: value_len as u32,
					value: account_bytes,
				},
			],
			call_body: Some(body::repeated_dyn(r * API_BENCHMARK_BATCH_SIZE, vec![
				Counter(value_len as u32, account_len as u32), // account_ptr
				Regular(Instruction::I32Const(account_len as i32)), // account_len
				Regular(Instruction::I32Const(0)), // value_ptr
				Regular(Instruction::I32Const(value_len as i32)), // value_len
				Regular(Instruction::Call(0)),
				Regular(Instruction::Drop),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
		for account in &accounts {
			assert_eq!(T::Currency::total_balance(account), 0u32.into());
		}
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])
	verify {
		for account in &accounts {
			assert_eq!(T::Currency::total_balance(account), value);
		}
	}

	// We call unique accounts.
	seal_call {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let dummy_code = WasmModule::<T>::dummy_with_mem();
		let callees = (0..r * API_BENCHMARK_BATCH_SIZE)
			.map(|i| Contract::with_index(i + 1, dummy_code.clone(), vec![], Endow::Max))
			.collect::<Result<Vec<_>, _>>()?;
		let callee_len = callees.get(0).map(|i| i.account_id.encode().len()).unwrap_or(0);
		let callee_bytes = callees.iter().flat_map(|x| x.account_id.encode()).collect();
		let value: BalanceOf<T> = 0u32.into();
		let value_bytes = value.encode();
		let value_len = value_bytes.len();
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_call",
				params: vec![
					ValueType::I32,
					ValueType::I32,
					ValueType::I64,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
				],
				return_type: Some(ValueType::I32),
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: value_bytes,
				},
				DataSegment {
					offset: value_len as u32,
					value: callee_bytes,
				},
			],
			call_body: Some(body::repeated_dyn(r * API_BENCHMARK_BATCH_SIZE, vec![
				Counter(value_len as u32, callee_len as u32), // callee_ptr
				Regular(Instruction::I32Const(callee_len as i32)), // callee_len
				Regular(Instruction::I64Const(0)), // gas
				Regular(Instruction::I32Const(0)), // value_ptr
				Regular(Instruction::I32Const(value_len as i32)), // value_len
				Regular(Instruction::I32Const(0)), // input_data_ptr
				Regular(Instruction::I32Const(0)), // input_data_len
				Regular(Instruction::I32Const(u32::max_value() as i32)), // output_ptr
				Regular(Instruction::I32Const(0)), // output_len_ptr
				Regular(Instruction::Call(0)),
				Regular(Instruction::Drop),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	seal_call_per_transfer_input_output_kb {
		let t in 0 .. 1;
		let i in 0 .. code::max_pages::<T>() * 64;
		let o in 0 .. (code::max_pages::<T>() - 1) * 64;
		let callee_code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_return",
				params: vec![
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
				],
				return_type: None,
			}],
			call_body: Some(body::plain(vec![
				Instruction::I32Const(0), // flags
				Instruction::I32Const(0), // data_ptr
				Instruction::I32Const((o * 1024) as i32), // data_len
				Instruction::Call(0),
				Instruction::End,
			])),
			.. Default::default()
		});
		let callees = (0..API_BENCHMARK_BATCH_SIZE)
			.map(|i| Contract::with_index(i + 1, callee_code.clone(), vec![], Endow::Max))
			.collect::<Result<Vec<_>, _>>()?;
		let callee_len = callees.get(0).map(|i| i.account_id.encode().len()).unwrap_or(0);
		let callee_bytes = callees.iter().flat_map(|x| x.account_id.encode()).collect::<Vec<_>>();
		let callees_len = callee_bytes.len();
		let value: BalanceOf<T> = t.into();
		let value_bytes = value.encode();
		let value_len = value_bytes.len();
		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_call",
				params: vec![
					ValueType::I32,
					ValueType::I32,
					ValueType::I64,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
				],
				return_type: Some(ValueType::I32),
			}],
			data_segments: vec![
				DataSegment {
					offset: 0,
					value: value_bytes,
				},
				DataSegment {
					offset: value_len as u32,
					value: callee_bytes,
				},
				DataSegment {
					offset: (value_len + callees_len) as u32,
					value: (o * 1024).to_le_bytes().into(),
				},
			],
			call_body: Some(body::repeated_dyn(API_BENCHMARK_BATCH_SIZE, vec![
				Counter(value_len as u32, callee_len as u32), // callee_ptr
				Regular(Instruction::I32Const(callee_len as i32)), // callee_len
				Regular(Instruction::I64Const(0)), // gas
				Regular(Instruction::I32Const(0)), // value_ptr
				Regular(Instruction::I32Const(value_len as i32)), // value_len
				Regular(Instruction::I32Const(0)), // input_data_ptr
				Regular(Instruction::I32Const((i * 1024) as i32)), // input_data_len
				Regular(Instruction::I32Const((value_len + callees_len + 4) as i32)), // output_ptr
				Regular(Instruction::I32Const((value_len + callees_len) as i32)), // output_len_ptr
				Regular(Instruction::Call(0)),
				Regular(Instruction::Drop),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// We assume that every instantiate sends at least the subsistence amount.
	seal_instantiate {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let hashes = (0..r * API_BENCHMARK_BATCH_SIZE)
			.map(|i| {
				let code = WasmModule::<T>::from(ModuleDefinition {
					memory: Some(ImportedMemory::max::<T>()),
					call_body: Some(body::plain(vec![
						// we need to add this in order to make contracts unique
						// so that they can be deployed from the same sender
						Instruction::I32Const(i as i32),
						Instruction::Drop,
						Instruction::End,
					])),
					.. Default::default()
				});
				Contracts::<T>::put_code_raw(code.code)?;
				Ok(code.hash)
			})
			.collect::<Result<Vec<_>, &'static str>>()?;
		let hash_len = hashes.get(0).map(|x| x.encode().len()).unwrap_or(0);
		let hashes_bytes = hashes.iter().flat_map(|x| x.encode()).collect::<Vec<_>>();
		let hashes_len = hashes_bytes.len();
		let value = ConfigCache::<T>::subsistence_threshold_uncached();
		assert!(value > 0u32.into());
		let value_bytes = value.encode();
		let value_len = value_bytes.len();
		let addr_len = sp_std::mem::size_of::<T::AccountId>();

		// offsets where to place static data in contract memory
		let value_offset = 0;
		let hashes_offset = value_offset + value_len;
		let addr_len_offset = hashes_offset + hashes_len;
		let addr_offset = addr_len_offset + addr_len;

		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_instantiate",
				params: vec![
					ValueType::I32,
					ValueType::I32,
					ValueType::I64,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
				],
				return_type: Some(ValueType::I32),
			}],
			data_segments: vec![
				DataSegment {
					offset: value_offset as u32,
					value: value_bytes,
				},
				DataSegment {
					offset: hashes_offset as u32,
					value: hashes_bytes,
				},
				DataSegment {
					offset: addr_len_offset as u32,
					value: addr_len.to_le_bytes().into(),
				},
			],
			call_body: Some(body::repeated_dyn(r * API_BENCHMARK_BATCH_SIZE, vec![
				Counter(hashes_offset as u32, hash_len as u32), // code_hash_ptr
				Regular(Instruction::I32Const(hash_len as i32)), // code_hash_len
				Regular(Instruction::I64Const(0)), // gas
				Regular(Instruction::I32Const(value_offset as i32)), // value_ptr
				Regular(Instruction::I32Const(value_len as i32)), // value_len
				Regular(Instruction::I32Const(0)), // input_data_ptr
				Regular(Instruction::I32Const(0)), // input_data_len
				Regular(Instruction::I32Const(addr_offset as i32)), // address_ptr
				Regular(Instruction::I32Const(addr_len_offset as i32)), // address_len_ptr
				Regular(Instruction::I32Const(u32::max_value() as i32)), // output_ptr
				Regular(Instruction::I32Const(0)), // output_len_ptr
				Regular(Instruction::I32Const(0)), // salt_ptr
				Regular(Instruction::I32Const(0)), // salt_ptr_len
				Regular(Instruction::Call(0)),
				Regular(Instruction::Drop),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
		let callee = instance.addr.clone();
		let addresses = hashes
			.iter()
			.map(|hash| Contracts::<T>::contract_address(
				&instance.account_id, hash, &[],
			))
			.collect::<Vec<_>>();

		for addr in &addresses {
			if let Some(_) = ContractInfoOf::<T>::get(&addr) {
				return Err("Expected that contract does not exist at this point.");
			}
		}
	}: call(origin, callee, 0u32.into(), Weight::max_value(), vec![])
	verify {
		for addr in &addresses {
			instance.alive_info()?;
		}
	}

	seal_instantiate_per_input_output_salt_kb {
		let i in 0 .. (code::max_pages::<T>() - 1) * 64;
		let o in 0 .. (code::max_pages::<T>() - 1) * 64;
		let s in 0 .. (code::max_pages::<T>() - 1) * 64;
		let callee_code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_return",
				params: vec![
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
				],
				return_type: None,
			}],
			deploy_body: Some(body::plain(vec![
				Instruction::I32Const(0), // flags
				Instruction::I32Const(0), // data_ptr
				Instruction::I32Const((o * 1024) as i32), // data_len
				Instruction::Call(0),
				Instruction::End,
			])),
			.. Default::default()
		});
		let hash = callee_code.hash.clone();
		let hash_bytes = callee_code.hash.encode();
		let hash_len = hash_bytes.len();
		Contracts::<T>::put_code_raw(callee_code.code)?;
		let inputs = (0..API_BENCHMARK_BATCH_SIZE).map(|x| x.encode()).collect::<Vec<_>>();
		let input_len = inputs.get(0).map(|x| x.len()).unwrap_or(0);
		let input_bytes = inputs.iter().cloned().flatten().collect::<Vec<_>>();
		let inputs_len = input_bytes.len();
		let value = ConfigCache::<T>::subsistence_threshold_uncached();
		assert!(value > 0u32.into());
		let value_bytes = value.encode();
		let value_len = value_bytes.len();
		let addr_len = sp_std::mem::size_of::<T::AccountId>();

		// offsets where to place static data in contract memory
		let input_offset = 0;
		let value_offset = inputs_len;
		let hash_offset = value_offset + value_len;
		let addr_len_offset = hash_offset + hash_len;
		let output_len_offset = addr_len_offset + 4;
		let output_offset = output_len_offset + 4;

		let code = WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			imported_functions: vec![ImportedFunction {
				name: "seal_instantiate",
				params: vec![
					ValueType::I32,
					ValueType::I32,
					ValueType::I64,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
					ValueType::I32,
				],
				return_type: Some(ValueType::I32),
			}],
			data_segments: vec![
				DataSegment {
					offset: input_offset as u32,
					value: input_bytes,
				},
				DataSegment {
					offset: value_offset as u32,
					value: value_bytes,
				},
				DataSegment {
					offset: hash_offset as u32,
					value: hash_bytes,
				},
				DataSegment {
					offset: addr_len_offset as u32,
					value: (addr_len as u32).to_le_bytes().into(),
				},
				DataSegment {
					offset: output_len_offset as u32,
					value: (o * 1024).to_le_bytes().into(),
				},
			],
			call_body: Some(body::repeated_dyn(API_BENCHMARK_BATCH_SIZE, vec![
				Regular(Instruction::I32Const(hash_offset as i32)), // code_hash_ptr
				Regular(Instruction::I32Const(hash_len as i32)), // code_hash_len
				Regular(Instruction::I64Const(0)), // gas
				Regular(Instruction::I32Const(value_offset as i32)), // value_ptr
				Regular(Instruction::I32Const(value_len as i32)), // value_len
				Counter(input_offset as u32, input_len as u32), // input_data_ptr
				Regular(Instruction::I32Const((i * 1024).max(input_len as u32) as i32)), // input_data_len
				Regular(Instruction::I32Const((addr_len_offset + addr_len) as i32)), // address_ptr
				Regular(Instruction::I32Const(addr_len_offset as i32)), // address_len_ptr
				Regular(Instruction::I32Const(output_offset as i32)), // output_ptr
				Regular(Instruction::I32Const(output_len_offset as i32)), // output_len_ptr
				Counter(input_offset as u32, input_len as u32), // salt_ptr
				Regular(Instruction::I32Const((s * 1024).max(input_len as u32) as i32)), // salt_len
				Regular(Instruction::Call(0)),
				Regular(Instruction::I32Eqz),
				Regular(Instruction::If(BlockType::NoResult)),
				Regular(Instruction::Nop),
				Regular(Instruction::Else),
				Regular(Instruction::Unreachable),
				Regular(Instruction::End),
			])),
			.. Default::default()
		});
		let instance = Contract::<T>::new(code, vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// Only the overhead of calling the function itself with minimal arguments.
	seal_hash_sha2_256 {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = Contract::<T>::new(WasmModule::hasher(
			"seal_hash_sha2_256", r * API_BENCHMARK_BATCH_SIZE, 0,
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// `n`: Input to hash in kilobytes
	seal_hash_sha2_256_per_kb {
		let n in 0 .. code::max_pages::<T>() * 64;
		let instance = Contract::<T>::new(WasmModule::hasher(
			"seal_hash_sha2_256", API_BENCHMARK_BATCH_SIZE, n * 1024,
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// Only the overhead of calling the function itself with minimal arguments.
	seal_hash_keccak_256 {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = Contract::<T>::new(WasmModule::hasher(
			"seal_hash_keccak_256", r * API_BENCHMARK_BATCH_SIZE, 0,
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// `n`: Input to hash in kilobytes
	seal_hash_keccak_256_per_kb {
		let n in 0 .. code::max_pages::<T>() * 64;
		let instance = Contract::<T>::new(WasmModule::hasher(
			"seal_hash_keccak_256", API_BENCHMARK_BATCH_SIZE, n * 1024,
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// Only the overhead of calling the function itself with minimal arguments.
	seal_hash_blake2_256 {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = Contract::<T>::new(WasmModule::hasher(
			"seal_hash_blake2_256", r * API_BENCHMARK_BATCH_SIZE, 0,
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// `n`: Input to hash in kilobytes
	seal_hash_blake2_256_per_kb {
		let n in 0 .. code::max_pages::<T>() * 64;
		let instance = Contract::<T>::new(WasmModule::hasher(
			"seal_hash_blake2_256", API_BENCHMARK_BATCH_SIZE, n * 1024,
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// Only the overhead of calling the function itself with minimal arguments.
	seal_hash_blake2_128 {
		let r in 0 .. API_BENCHMARK_BATCHES;
		let instance = Contract::<T>::new(WasmModule::hasher(
			"seal_hash_blake2_128", r * API_BENCHMARK_BATCH_SIZE, 0,
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// `n`: Input to hash in kilobytes
	seal_hash_blake2_128_per_kb {
		let n in 0 .. code::max_pages::<T>() * 64;
		let instance = Contract::<T>::new(WasmModule::hasher(
			"seal_hash_blake2_128", API_BENCHMARK_BATCH_SIZE, n * 1024,
		), vec![], Endow::Max)?;
		let origin = RawOrigin::Signed(instance.caller.clone());
	}: call(origin, instance.addr, 0u32.into(), Weight::max_value(), vec![])

	// We make the assumption that pushing a constant and dropping a value takes roughly
	// the same amount of time. We follow that `t.load` and `drop` both have the weight
	// of this benchmark / 2. We need to make this assumption because there is no way
	// to measure them on their own using a valid wasm module. We need their individual
	// values to derive the weight of individual instructions (by substraction) from
	// benchmarks that include those for parameter pushing and return type dropping.
	// We call the weight of `t.load` and `drop`: `w_param`.
	// The weight that would result from the respective benchmark we call: `w_bench`.
	//
	// w_i{32,64}const = w_drop = w_bench / 2
	instr_i64const {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			call_body: Some(body::repeated_dyn(r * INSTR_BENCHMARK_BATCH_SIZE, vec![
				RandomI64Repeated(1),
				Regular(Instruction::Drop),
			])),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_i{32,64}load = w_bench - 2 * w_param
	instr_i64load {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			call_body: Some(body::repeated_dyn(r * INSTR_BENCHMARK_BATCH_SIZE, vec![
				RandomUnaligned(0, code::max_pages::<T>() * 64 * 1024 - 8),
				Regular(Instruction::I64Load(3, 0)),
				Regular(Instruction::Drop),
			])),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_i{32,64}store{...} = w_bench - 2 * w_param
	instr_i64store {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			call_body: Some(body::repeated_dyn(r * INSTR_BENCHMARK_BATCH_SIZE, vec![
				RandomUnaligned(0, code::max_pages::<T>() * 64 * 1024 - 8),
				RandomI64Repeated(1),
				Regular(Instruction::I64Store(3, 0)),
			])),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_select = w_bench - 4 * w_param
	instr_select {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			call_body: Some(body::repeated_dyn(r * INSTR_BENCHMARK_BATCH_SIZE, vec![
				RandomI64Repeated(1),
				RandomI64Repeated(1),
				RandomI32(0, 2),
				Regular(Instruction::Select),
				Regular(Instruction::Drop),
			])),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_if = w_bench - 3 * w_param
	instr_if {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			call_body: Some(body::repeated_dyn(r * INSTR_BENCHMARK_BATCH_SIZE, vec![
				RandomI32(0, 2),
				Regular(Instruction::If(BlockType::Value(ValueType::I64))),
				RandomI64Repeated(1),
				Regular(Instruction::Else),
				RandomI64Repeated(1),
				Regular(Instruction::End),
				Regular(Instruction::Drop),
			])),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_br = w_bench - 2 * w_param
	instr_br {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			call_body: Some(body::repeated_dyn(r * INSTR_BENCHMARK_BATCH_SIZE, vec![
				Regular(Instruction::Block(BlockType::NoResult)),
				Regular(Instruction::Block(BlockType::NoResult)),
				Regular(Instruction::Block(BlockType::NoResult)),
				Regular(Instruction::Br(1)),
				RandomI64Repeated(1),
				Regular(Instruction::Drop),
				Regular(Instruction::End),
				RandomI64Repeated(1),
				Regular(Instruction::Drop),
				Regular(Instruction::End),
				RandomI64Repeated(1),
				Regular(Instruction::Drop),
				Regular(Instruction::End),
			])),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_br_if = w_bench - 5 * w_param
	// The two additional pushes + drop are only executed 50% of the time.
	// Making it: 3 * w_param + (50% * 4 * w_param)
	instr_br_if {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			call_body: Some(body::repeated_dyn(r * INSTR_BENCHMARK_BATCH_SIZE, vec![
				Regular(Instruction::Block(BlockType::NoResult)),
				Regular(Instruction::Block(BlockType::NoResult)),
				Regular(Instruction::Block(BlockType::NoResult)),
				RandomI32(0, 2),
				Regular(Instruction::BrIf(1)),
				RandomI64Repeated(1),
				Regular(Instruction::Drop),
				Regular(Instruction::End),
				RandomI64Repeated(1),
				Regular(Instruction::Drop),
				Regular(Instruction::End),
				RandomI64Repeated(1),
				Regular(Instruction::Drop),
				Regular(Instruction::End),
			])),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_br_table = w_bench - 3 * w_param
	// 1 * w_param + 0.5 * 2 * w_param + 0.25 * 4 * w_param
	instr_br_table {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let table = Box::new(parity_wasm::elements::BrTableData {
			table: Box::new([0, 1, 2]),
			default: 1,
		});
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			call_body: Some(body::repeated_dyn(r * INSTR_BENCHMARK_BATCH_SIZE, vec![
				Regular(Instruction::Block(BlockType::NoResult)),
				Regular(Instruction::Block(BlockType::NoResult)),
				Regular(Instruction::Block(BlockType::NoResult)),
				RandomI32(0, 4),
				Regular(Instruction::BrTable(table)),
				RandomI64Repeated(1),
				Regular(Instruction::Drop),
				Regular(Instruction::End),
				RandomI64Repeated(1),
				Regular(Instruction::Drop),
				Regular(Instruction::End),
				RandomI64Repeated(1),
				Regular(Instruction::Drop),
				Regular(Instruction::End),
			])),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_br_table_per_entry = w_bench
	instr_br_table_per_entry {
		let e in 1 .. Contracts::<T>::current_schedule().limits.br_table_size;
		let entry: Vec<u32> = [0, 1].iter()
			.cloned()
			.cycle()
			.take((e / 2) as usize).collect();
		let table = Box::new(parity_wasm::elements::BrTableData {
			table: entry.into_boxed_slice(),
			default: 0,
		});
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			call_body: Some(body::repeated_dyn(INSTR_BENCHMARK_BATCH_SIZE, vec![
				Regular(Instruction::Block(BlockType::NoResult)),
				Regular(Instruction::Block(BlockType::NoResult)),
				Regular(Instruction::Block(BlockType::NoResult)),
				RandomI32(0, (e + 1) as i32), // Make sure the default entry is also used
				Regular(Instruction::BrTable(table)),
				RandomI64Repeated(1),
				Regular(Instruction::Drop),
				Regular(Instruction::End),
				RandomI64Repeated(1),
				Regular(Instruction::Drop),
				Regular(Instruction::End),
				RandomI64Repeated(1),
				Regular(Instruction::Drop),
				Regular(Instruction::End),
			])),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_call = w_bench - 2 * w_param
	instr_call {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			// We need to make use of the stack here in order to trigger stack height
			// instrumentation.
			aux_body: Some(body::plain(vec![
				Instruction::I64Const(42),
				Instruction::Drop,
				Instruction::End,
			])),
			call_body: Some(body::repeated(r * INSTR_BENCHMARK_BATCH_SIZE, &[
				Instruction::Call(2), // call aux
			])),
			inject_stack_metering: true,
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_call_indrect = w_bench - 3 * w_param
	instr_call_indirect {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let num_elements = Contracts::<T>::current_schedule().limits.table_size;
		use self::code::TableSegment;
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			// We need to make use of the stack here in order to trigger stack height
			// instrumentation.
			aux_body: Some(body::plain(vec![
				Instruction::I64Const(42),
				Instruction::Drop,
				Instruction::End,
			])),
			call_body: Some(body::repeated_dyn(r * INSTR_BENCHMARK_BATCH_SIZE, vec![
				RandomI32(0, num_elements as i32),
				Regular(Instruction::CallIndirect(0, 0)), // we only have one sig: 0
			])),
			inject_stack_metering: true,
			table: Some(TableSegment {
				num_elements,
				function_index: 2, // aux
			}),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_instr_call_indirect_per_param = w_bench - 1 * w_param
	// Calling a function indirectly causes it to go through a thunk function whose runtime
	// linearly depend on the amount of parameters to this function.
	// Please note that this is not necessary with a direct call.
	instr_call_indirect_per_param {
		let p in 0 .. Contracts::<T>::current_schedule().limits.parameters;
		let num_elements = Contracts::<T>::current_schedule().limits.table_size;
		use self::code::TableSegment;
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			// We need to make use of the stack here in order to trigger stack height
			// instrumentation.
			aux_body: Some(body::plain(vec![
				Instruction::I64Const(42),
				Instruction::Drop,
				Instruction::End,
			])),
			aux_arg_num: p,
			call_body: Some(body::repeated_dyn(INSTR_BENCHMARK_BATCH_SIZE, vec![
				RandomI64Repeated(p as usize),
				RandomI32(0, num_elements as i32),
				Regular(Instruction::CallIndirect(p.min(1), 0)), // aux signature: 1 or 0
			])),
			inject_stack_metering: true,
			table: Some(TableSegment {
				num_elements,
				function_index: 2, // aux
			}),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_local_get = w_bench - 1 * w_param
	instr_local_get {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let max_locals = Contracts::<T>::current_schedule().limits.stack_height;
		let mut call_body = body::repeated_dyn(r * INSTR_BENCHMARK_BATCH_SIZE, vec![
			RandomGetLocal(0, max_locals),
			Regular(Instruction::Drop),
		]);
		body::inject_locals(&mut call_body, max_locals);
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			call_body: Some(call_body),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_local_set = w_bench - 1 * w_param
	instr_local_set {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let max_locals = Contracts::<T>::current_schedule().limits.stack_height;
		let mut call_body = body::repeated_dyn(r * INSTR_BENCHMARK_BATCH_SIZE, vec![
			RandomI64Repeated(1),
			RandomSetLocal(0, max_locals),
		]);
		body::inject_locals(&mut call_body, max_locals);
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			call_body: Some(call_body),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_local_tee = w_bench - 2 * w_param
	instr_local_tee {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let max_locals = Contracts::<T>::current_schedule().limits.stack_height;
		let mut call_body = body::repeated_dyn(r * INSTR_BENCHMARK_BATCH_SIZE, vec![
			RandomI64Repeated(1),
			RandomTeeLocal(0, max_locals),
			Regular(Instruction::Drop),
		]);
		body::inject_locals(&mut call_body, max_locals);
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			call_body: Some(call_body),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_global_get = w_bench - 1 * w_param
	instr_global_get {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let max_globals = Contracts::<T>::current_schedule().limits.globals;
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			call_body: Some(body::repeated_dyn(r * INSTR_BENCHMARK_BATCH_SIZE, vec![
				RandomGetGlobal(0, max_globals),
				Regular(Instruction::Drop),
			])),
			num_globals: max_globals,
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_global_set = w_bench - 1 * w_param
	instr_global_set {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let max_globals = Contracts::<T>::current_schedule().limits.globals;
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			call_body: Some(body::repeated_dyn(r * INSTR_BENCHMARK_BATCH_SIZE, vec![
				RandomI64Repeated(1),
				RandomSetGlobal(0, max_globals),
			])),
			num_globals: max_globals,
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_memory_get = w_bench - 1 * w_param
	instr_memory_current {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory::max::<T>()),
			call_body: Some(body::repeated(r * INSTR_BENCHMARK_BATCH_SIZE, &[
				Instruction::CurrentMemory(0),
				Instruction::Drop
			])),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// w_memory_grow = w_bench - 2 * w_param
	// We can only allow allocate as much memory as it is allowed in a a contract.
	// Therefore the repeat count is limited by the maximum memory any contract can have.
	// Using a contract with more memory will skew the benchmark because the runtime of grow
	// depends on how much memory is already allocated.
	instr_memory_grow {
		let r in 0 .. 1;
		let max_pages = ImportedMemory::max::<T>().max_pages;
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			memory: Some(ImportedMemory {
				min_pages: 0,
				max_pages,
			}),
			call_body: Some(body::repeated(r * max_pages, &[
				Instruction::I32Const(1),
				Instruction::GrowMemory(0),
				Instruction::Drop,
			])),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	// Unary numeric instructions.
	// All use w = w_bench - 2 * w_param.

	instr_i64clz {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::unary_instr(
			Instruction::I64Clz,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64ctz {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::unary_instr(
			Instruction::I64Ctz,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64popcnt {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::unary_instr(
			Instruction::I64Popcnt,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64eqz {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::unary_instr(
			Instruction::I64Eqz,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64extendsi32 {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			call_body: Some(body::repeated_dyn(r * INSTR_BENCHMARK_BATCH_SIZE, vec![
				RandomI32Repeated(1),
				Regular(Instruction::I64ExtendSI32),
				Regular(Instruction::Drop),
			])),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	instr_i64extendui32 {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::from(ModuleDefinition {
			call_body: Some(body::repeated_dyn(r * INSTR_BENCHMARK_BATCH_SIZE, vec![
				RandomI32Repeated(1),
				Regular(Instruction::I64ExtendUI32),
				Regular(Instruction::Drop),
			])),
			.. Default::default()
		}));
	}: {
		sbox.invoke();
	}

	instr_i32wrapi64 {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::unary_instr(
			Instruction::I32WrapI64,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	// Binary numeric instructions.
	// All use w = w_bench - 3 * w_param.

	instr_i64eq {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64Eq,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64ne {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64Ne,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64lts {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64LtS,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64ltu {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64LtU,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64gts {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64GtS,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64gtu {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64GtU,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64les {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64LeS,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64leu {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64LeU,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64ges {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64GeS,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64geu {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64GeU,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64add {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64Add,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64sub {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64Sub,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64mul {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64Mul,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64divs {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64DivS,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64divu {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64DivU,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64rems {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64RemS,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64remu {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64RemU,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64and {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64And,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64or {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64Or,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64xor {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64Xor,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64shl {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64Shl,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64shrs {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64ShrS,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64shru {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64ShrU,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64rotl {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64Rotl,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	instr_i64rotr {
		let r in 0 .. INSTR_BENCHMARK_BATCHES;
		let mut sbox = Sandbox::from(&WasmModule::<T>::binary_instr(
			Instruction::I64Rotr,
			r * INSTR_BENCHMARK_BATCH_SIZE,
		));
	}: {
		sbox.invoke();
	}

	// This is no benchmark. It merely exist to have an easy way to pretty print the curently
	// configured `Schedule` during benchmark development.
	// It can be outputed using the following command:
	// cargo run --manifest-path=bin/node/cli/Cargo.toml --release \
	//     --features runtime-benchmarks -- benchmark --dev --execution=native \
	//     -p pallet_contracts -e print_schedule --no-median-slopes --no-min-squares
	#[extra]
	print_schedule {
		#[cfg(feature = "std")]
		println!("{:#?}", Schedule::<T>::default());
		#[cfg(not(feature = "std"))]
		return Err("Run this bench with a native runtime in order to see the schedule.");
	}: {}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::tests::{ExtBuilder, Test};
	use frame_support::assert_ok;
	use paste::paste;

	macro_rules! create_test {
		($name:ident) => {
			#[test]
			fn $name() {
				ExtBuilder::default().build().execute_with(|| {
					assert_ok!(paste!{
						[<test_benchmark_ $name>]::<Test>()
					});
				});
			}
		}
	}

	create_test!(on_initialize);
	create_test!(on_initialize_per_trie_key);
	create_test!(on_initialize_per_queue_item);

	create_test!(update_schedule);
	create_test!(put_code);
	create_test!(instantiate);
	create_test!(call);
	create_test!(claim_surcharge);

	create_test!(seal_caller);
	create_test!(seal_address);
	create_test!(seal_gas_left);
	create_test!(seal_balance);
	create_test!(seal_value_transferred);
	create_test!(seal_minimum_balance);
	create_test!(seal_tombstone_deposit);
	create_test!(seal_rent_allowance);
	create_test!(seal_block_number);
	create_test!(seal_now);
	create_test!(seal_weight_to_fee);
	create_test!(seal_gas);
	create_test!(seal_input);
	create_test!(seal_input_per_kb);
	create_test!(seal_return);
	create_test!(seal_return_per_kb);
	create_test!(seal_terminate);
	create_test!(seal_restore_to);
	create_test!(seal_restore_to_per_delta);
	create_test!(seal_random);
	create_test!(seal_deposit_event);
	create_test!(seal_deposit_event_per_topic_and_kb);
	create_test!(seal_set_rent_allowance);
	create_test!(seal_set_storage);
	create_test!(seal_set_storage_per_kb);
	create_test!(seal_get_storage);
	create_test!(seal_get_storage_per_kb);
	create_test!(seal_transfer);
	create_test!(seal_call);
	create_test!(seal_call_per_transfer_input_output_kb);
	create_test!(seal_instantiate);
	create_test!(seal_instantiate_per_input_output_salt_kb);
	create_test!(seal_clear_storage);
	create_test!(seal_hash_sha2_256);
	create_test!(seal_hash_sha2_256_per_kb);
	create_test!(seal_hash_keccak_256);
	create_test!(seal_hash_keccak_256_per_kb);
	create_test!(seal_hash_blake2_256);
	create_test!(seal_hash_blake2_256_per_kb);
	create_test!(seal_hash_blake2_128);
	create_test!(seal_hash_blake2_128_per_kb);

	create_test!(instr_i64const);
	create_test!(instr_i64load);
	create_test!(instr_i64store);
	create_test!(instr_select);
	create_test!(instr_if);
	create_test!(instr_br);
	create_test!(instr_br_if);
	create_test!(instr_br_table);
	create_test!(instr_br_table_per_entry);
	create_test!(instr_call);
	create_test!(instr_call_indirect);
	create_test!(instr_call_indirect_per_param);
	create_test!(instr_local_get);
	create_test!(instr_local_set);
	create_test!(instr_local_tee);
	create_test!(instr_global_get);
	create_test!(instr_global_set);
	create_test!(instr_memory_current);
	create_test!(instr_memory_grow);
	create_test!(instr_i64clz);
	create_test!(instr_i64ctz);
	create_test!(instr_i64popcnt);
	create_test!(instr_i64eqz);
	create_test!(instr_i64extendsi32);
	create_test!(instr_i64extendui32);
	create_test!(instr_i32wrapi64);
	create_test!(instr_i64eq);
	create_test!(instr_i64ne);
	create_test!(instr_i64lts);
	create_test!(instr_i64ltu);
	create_test!(instr_i64gts);
	create_test!(instr_i64gtu);
	create_test!(instr_i64les);
	create_test!(instr_i64leu);
	create_test!(instr_i64ges);
	create_test!(instr_i64geu);
	create_test!(instr_i64add);
	create_test!(instr_i64sub);
	create_test!(instr_i64mul);
	create_test!(instr_i64divs);
	create_test!(instr_i64divu);
	create_test!(instr_i64rems);
	create_test!(instr_i64remu);
	create_test!(instr_i64and);
	create_test!(instr_i64or);
	create_test!(instr_i64xor);
	create_test!(instr_i64shl);
	create_test!(instr_i64shrs);
	create_test!(instr_i64shru);
	create_test!(instr_i64rotl);
	create_test!(instr_i64rotr);
}
