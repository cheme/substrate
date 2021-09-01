// This file is part of Substrate.

// Copyright (C) 2021-2021 Parity Technologies (UK) Ltd.
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


//! Migration from state V0 to state V1.
//!
//! This pallet should be included to runtime AFTER the state did switch
//! to state V1.
//!
//! It will ensure the full state gets migrated to V1.
//! When the full state is migrated (`MigrationProgress` is `Finished`), the
//! module can be remove from runtime.

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, Encode};
use sp_runtime::{
	StateVersion, RuntimeDebug,
};
use frame_support::{
	pallet_prelude::*,
};
use frame_system::{
	pallet_prelude::*, WeightInfo,
};

pub use pallet::*;

// syntactic sugar for logging.
#[macro_export]
macro_rules! log {
	($level:tt, $patter:expr $(, $values:expr)* $(,)?) => {
		log::$level!(
			target: crate::LOG_TARGET,
			concat!("[{:?}] 💸 ", $patter), <frame_system::Pallet<T>>::block_number() $(, $values)*
		)
	};
}

/// State of state migration from V0 to V1.
#[derive(Encode, Decode, RuntimeDebug)]
pub enum MigrationState {
	/// Started migration and current progress.
	Pending {
		// If None, nothing was processed and current_child is also None.
		// If Some, its value is already processed.
		current_top: Option<Vec<u8>>,
		// If Some, its value is already processed and additional content
		// in child trie need processing. current_top is the child trie root
		// in this case.
		// If None, current top is NOT a child trie root and current processing is top trie.
		//
		// So when current top is a child trie root, this should always be paused on a `Some`
		// varaiant.
		// So when pausing on a child trie processing the algorithm must ensure at least first
		// entry is processed.
		current_child: Option<Vec<u8>>,
	},
	/// Finished migration, this module will not
	/// do anything and can be removed.
	Finished,
}

impl Default for MigrationState {
	fn default() -> Self {
		MigrationState::Pending {
			current_top: None,
			current_child: None,
		}
	}
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::pallet]
	#[pallet::generate_store(pub(crate) trait Store)]
	pub struct Pallet<T>(_);

	#[pallet::type_value]
	pub(crate) fn MigrationProgressOnEmpty() -> MigrationState {
		MigrationState::default()
	}

	#[pallet::config]
	pub trait Config: frame_system::Config {
		#[pallet::constant]
		type MigrationWeight: Get<Weight>;
		#[pallet::constant]
		type MigrationMaxSize: Get<u32>;
		#[pallet::constant]
		type Threshold: Get<u32>;
	}

	/// Migration progress.
	///
	#[pallet::storage]
	#[pallet::getter(fn state_migration_v0_v1_progress)]
	pub(crate) type MigrationProgress<T> = StorageValue<_, MigrationState, ValueQuery, MigrationProgressOnEmpty>;

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {

		fn on_initialize(_now: BlockNumberFor<T>) -> Weight {
			let migration_progress = MigrationProgress::<T>::get();
	
			let (current_top, current_child) = match migration_progress {
				MigrationState::Finished => return T::DbWeight::get().reads(1),
				MigrationState::Pending { current_top, current_child } => (current_top, current_child),
			};
			let max_weight = T::MigrationWeight::get();
			let max_size = T::MigrationMaxSize::get();
			let threshold = T::Threshold::get();
			let mut pending = PendingMigration::<T> {
				current_top,
				current_child,
				current_weight: 0 as Weight,
				current_size: 0u32,
				max_weight,
				max_size,
				threshold,
				touched: false,
				_ph: sp_std::marker::PhantomData,
			};
			pending.migrate();
			if pending.current_top.is_some() {
				MigrationProgress::<T>::set(MigrationState::Pending {
					current_top: pending.current_top,
					current_child: pending.current_child,
				});
			} else {
				MigrationProgress::<T>::set(MigrationState::Finished);
			}
			T::DbWeight::get().reads(1)
				+ T::SystemWeightInfo::set_storage(1) // TODO size incorrect for migration pending
				+ pending.current_weight
		}

		fn on_finalize(_n: BlockNumberFor<T>) {
		}
	}
}

struct PendingMigration<T> {
	current_top: Option<Vec<u8>>,
	current_child: Option<Vec<u8>>,
	current_weight: Weight,
	current_size: u32,
	threshold: u32,
	max_weight: Weight,
	max_size: u32,
	touched: bool,
	_ph: sp_std::marker::PhantomData<T>,
}

impl<T: pallet::Config> PendingMigration<T> {
	fn new_read(&mut self, len: u32) -> bool {
		// TODO should it take size in account?
		self.current_weight += T::DbWeight::get().reads(1);
		self.current_size += len;
		self.current_size > self.max_size
			|| self.current_weight > self.max_weight
	}

	fn new_write(&mut self, len: u32) -> bool {
		self.touched = true;
		self.current_size += len;
		self.current_weight += T::SystemWeightInfo::set_storage(len);
		self.current_size > self.max_size
			|| self.current_weight > self.max_weight
	}

	fn new_next(&mut self) {
		self.current_weight += T::DbWeight::get().reads(1); // TODO what is next key weight?
	}

	// return true if can continue and false if limit reached
	fn process_child_key(&mut self) -> bool {
		if let (Some(storage_key), Some(current_key)) = (self.current_top.as_ref(), self.current_child.as_ref()) {
			if let Some(encode)	= sp_io::default_child_storage::get(child_io_key(storage_key), current_key) {
				let len = encode.len() as u32;
				if len >= self.threshold {
					sp_io::default_child_storage::set(child_io_key(storage_key), current_key, encode.as_slice());
					if self.new_write(len) {
						return false;
					}
				} else {
					if self.new_read(len) {
						return false;
					}
				}
			} else {
				if self.new_read(0) {
					return false;
				}
			}
		}
		true
	}

	// return true if can continue and false if limit reached
	fn process_top_key(&mut self) -> bool {
		if let Some(current_key) = self.current_top.as_ref() {
			if let Some(encode)	= sp_io::storage::get(current_key) {
				let len = encode.len() as u32;
				if len >= self.threshold {
					sp_io::storage::set(current_key, encode.as_slice());
					if self.new_write(len) {
						return false;
					}
				} else {
					if self.new_read(len) {
						return false;
					}
				}
			} else {
				if self.new_read(0) {
					return false;
				}
			}
		}
		true
	}


	// return true if all content did migrate.
	fn migrate_child(&mut self) -> bool {
		if self.current_child.is_none() {
			self.current_child = Some(Vec::new());
			let _ = self.process_child_key();
		}

		loop {
			if let Some(current_child) = self.current_child.as_ref() {
				if let Some(next_key) = if let Some(storage_key) = self.current_top.as_ref() {
					sp_io::default_child_storage::next_key(child_io_key(storage_key), current_child)
				} else {
					None
				} {
					self.new_next();
					self.current_child = Some(next_key);
					if !self.process_child_key() {
						return false;
					}
				} else {
					self.new_next();
					self.current_child = None;
				}
			} else {
				return true;
			}
		}
	}

	// return true if need to switch to child.
	fn migrate_top(&mut self) -> bool {
		if self.current_top.is_none() {
			self.current_top = Some(Vec::new());
			let _ = self.process_top_key();
		}
		loop {
			if let Some(current_top) = self.current_top.as_ref() {
					if let Some(next_key) = sp_io::storage::next_key(current_top) {
						self.new_next();
						if next_key == sp_core::storage::well_known_keys::CODE {
							self.current_top = Some(next_key);
							// skip as was already created on runtime update
							// and is likely to oversize block, same could be done with event.
							continue;
						}
						if next_key.starts_with(sp_core::storage::well_known_keys::CHILD_STORAGE_KEY_PREFIX) {
							self.current_top = Some(next_key);
							let _ = self.process_top_key();
							// always process first child key at least
							return true;
						}
						self.current_top = Some(next_key);
						if !self.process_top_key() {
							return false;
						}
					} else {
						self.current_top = None;
					}
			} else {
				self.new_next();
				return false;
			}
		}
	}

	fn migrate(&mut self) {
		loop {
			if self.current_child.is_some() {
				if !self.migrate_child() {
					break
				}
			} else {
				let do_child = self.migrate_top();
				if do_child {
					if !self.migrate_child() {
						break
					}
				} else {
					break;
				}
			}
		}
	}
}

fn child_io_key(storage_key: &Vec<u8>) -> &[u8] {
	use sp_core::storage::{ChildType, PrefixedStorageKey};
	match ChildType::from_prefixed_key(PrefixedStorageKey::new_ref(storage_key)) {
		Some((ChildType::ParentKeyId, storage_key)) => storage_key,
		None => &[], // Ignore
	}
}


#[cfg(test)]
mod mock {
	use crate as pallet_state_migrate_0_to_1;
	use super::*;
	use frame_support::{
		ord_parameter_types, parameter_types,
	};
	use sp_core::H256;
	use sp_runtime::{
		testing::Header,
		traits::{BlakeTwo256, IdentityLookup},
	};

	type UncheckedExtrinsic = frame_system::mocking::MockUncheckedExtrinsic<Test>;
	type Block = frame_system::mocking::MockBlock<Test>;

	// Configure a mock runtime to test the pallet.
	frame_support::construct_runtime!(
		pub enum Test where
			Block = Block,
			NodeBlock = Block,
			UncheckedExtrinsic = UncheckedExtrinsic,
		{
			System: frame_system::{Pallet, Call, Config, Storage, Event<T>},
			StateMigrate0To1: pallet_state_migrate_0_to_1::{Pallet, Storage},
		}
	);

	impl frame_system::Config for Test {
		type BaseCallFilter = frame_support::traits::Everything;
		type BlockWeights = ();
		type BlockLength = ();
		type Origin = Origin;
		type Call = Call;
		type Index = u64;
		type BlockNumber = u64;
		type Hash = H256;
		type Hashing = BlakeTwo256;
		type AccountId = u64;
		type Lookup = IdentityLookup<Self::AccountId>;
		type Header = Header;
		type Event = Event;
		type BlockHashCount = BlockHashCount;
		type DbWeight = ();
		type Version = ();
		type PalletInfo = PalletInfo;
		type AccountData = ();
		type OnNewAccount = ();
		type OnKilledAccount = ();
		type SystemWeightInfo = ();
		type SS58Prefix = SS58Prefix;
		type OnSetCode = ();
	}

	pub struct MigrationWeightImpl;
	impl Get<Weight> for MigrationWeightImpl {
		fn get() -> Weight {
			500_000_000_000 as Weight
		}
	}

	pub struct MigrationMaxSizeImpl;
	impl Get<u32> for MigrationMaxSizeImpl {
		fn get() -> u32 {
			500_000
		}
	}

	pub struct ThresholdImpl;
	impl Get<u32> for ThresholdImpl {
		fn get() -> u32 {
			33
		}
	}

	parameter_types! {
		pub const BlockHashCount: u64 = 250;
		pub const SS58Prefix: u8 = 42;
	}

	impl pallet_state_migrate_0_to_1::Config for Test {
		type MigrationWeight = MigrationWeightImpl;
		type MigrationMaxSize = MigrationMaxSizeImpl;
		type Threshold = ThresholdImpl;
	}

	pub fn new_test_ext(switch_version: bool) -> sp_io::TestExternalities {
		use sp_core::storage::ChildInfo;
		let t = frame_system::GenesisConfig::default();
		let mut child_key1 = sp_core::storage::well_known_keys::DEFAULT_CHILD_STORAGE_KEY_PREFIX.to_vec();
		child_key1.extend_from_slice(b"chk1");
		let mut child_key2 = sp_core::storage::well_known_keys::DEFAULT_CHILD_STORAGE_KEY_PREFIX.to_vec();
		child_key2.extend_from_slice(b"chk2");
		let mut storage = sp_core::storage::Storage {
			top: vec![
				(b"key1".to_vec(), vec![1u8; 40]),
				(sp_core::storage::well_known_keys::CODE.to_vec(), vec![1u8; 40]),
			].into_iter().collect(),
			children_default: vec![
				(child_key1, sp_core::storage::StorageChild {
					data: vec![
						(b"key1".to_vec(), vec![2u8; 40]),
						(b"key2".to_vec(), vec![2u8; 4]),
					].into_iter().collect(),
					child_info: ChildInfo::new_default(b"chk1"),
				}),
				(child_key2, sp_core::storage::StorageChild {
					data: vec![
						(b"key1".to_vec(), vec![3u8; 4]),
						(b"key2".to_vec(), vec![3u8; 40]),
					].into_iter().collect(),
					child_info: ChildInfo::new_default(b"chk2"),
				}),
			].into_iter().collect(),
		};
		t.assimilate_storage::<Test>(&mut storage).unwrap();

		if !switch_version {
			return (storage, StateVersion::V0).into();
		}
		let mut test_externalities: sp_io::TestExternalities = (storage, StateVersion::V0).into();

		// rebuild backend with new version.
		let root = test_externalities.backend.root().clone();
		let storage = test_externalities.backend.into_storage();
		test_externalities.backend = sp_api::InMemoryBackend::new(storage, root, sp_core::state_version::DEFAULT_STATE_VERSION);
		test_externalities
	}

	pub fn run_to_block(n: u64) -> H256 {
		use sp_api::HeaderT;
		let mut root = Default::default();
		while System::block_number() < n {
			System::on_finalize(System::block_number());
			StateMigrate0To1::on_finalize(System::block_number());
			System::set_block_number(System::block_number() + 1);
			System::on_initialize(System::block_number());
			StateMigrate0To1::on_initialize(System::block_number());
			root = System::finalize().state_root().clone();
		}
		root
	}
}

#[cfg(test)]
mod test {
	use super::*;
	use crate::mock::*;

	#[test]
	fn run_test() {
		let mut test_ext_no_switch = new_test_ext(false);
		let mut test_ext = new_test_ext(true);
		let initial_root = test_ext.backend.root().clone();
		// TODO simulate a multiblock
		let final_root = test_ext.execute_with(|| {
			let r = run_to_block(2);
			assert!(matches!(
				StateMigrate0To1::state_migration_v0_v1_progress(),
				MigrationState::Finished,
			));
			r
		});
		let final_no_switch = test_ext_no_switch.execute_with(|| {
			run_to_block(2)
		});
		assert!(final_root != initial_root);
		assert!(final_root != final_no_switch);
	}
}
