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
		current_top: Option<Vec<u8>>,
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
			if let Some(encode)	= sp_io::default_child_storage::get(storage_key, current_key) {
				let len = encode.len() as u32;
				if len >= self.threshold {
					sp_io::default_child_storage::set(storage_key, current_key, encode.as_slice());
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
				if let Some(next_key)	=	if let Some(storage_key) = self.current_top.as_ref() {
					sp_io::default_child_storage::next_key(storage_key, current_child)
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
					if let Some(next_key)	= sp_io::storage::next_key(current_top) {
						self.new_next();
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
