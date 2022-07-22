// This file is part of Substrate.

// Copyright (C) 2017-2022 Parity Technologies (UK) Ltd.
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

//! Remark storage pallet. Indexes remarks and stores them off chain.

// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

mod benchmarking;
pub mod weights;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

use sp_std::prelude::*;

// Re-export pallet items so that they can be accessed from the crate namespace.
pub use pallet::*;
pub use weights::WeightInfo;

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;
		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Attempting to store empty data.
		Empty,
		/// Attempted to call `store` outside of block execution.
		BadContext,
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		#[pallet::weight(T::WeightInfo::store(input.len() as u32))]
		pub fn hash_io(origin: OriginFor<T>, input: Vec<u8>) -> DispatchResult {
			for i in 0..10 {
				let hash = sp_io::hashing::twox_128(&input[..]);
				Self::deposit_event(Event::Hash { hash });
			}
			Ok(())
		}

		#[pallet::weight(T::WeightInfo::store(input.len() as u32))]
		pub fn hash_wasm(origin: OriginFor<T>, input: Vec<u8>) -> DispatchResult {
			use byteorder::{ByteOrder, LittleEndian};
			use core::hash::Hasher;
			use digest::Digest;
//					consts::{U16, U32, U8},

			for i in 0..10 {
				let mut dest = [0u8; 16];
				let r0 = twox_hash::XxHash::with_seed(0).chain_update(&input[..]).finish();
				let r1 = twox_hash::XxHash::with_seed(1).chain_update(&input[..]).finish();
				LittleEndian::write_u64(&mut dest[0..8], r0);
				LittleEndian::write_u64(&mut dest[8..16], r1);
				Self::deposit_event(Event::Hash { hash: dest });
			}
			Ok(())
		}

		/// Index and store data off chain.
		#[pallet::weight(T::WeightInfo::store(remark.len() as u32))]
		pub fn store(origin: OriginFor<T>, remark: Vec<u8>) -> DispatchResultWithPostInfo {
			ensure!(!remark.is_empty(), Error::<T>::Empty);
			let sender = ensure_signed(origin)?;
			let content_hash = sp_io::hashing::blake2_256(&remark);
			let extrinsic_index = <frame_system::Pallet<T>>::extrinsic_index()
				.ok_or_else(|| Error::<T>::BadContext)?;
			sp_io::transaction_index::index(extrinsic_index, remark.len() as u32, content_hash);
			Self::deposit_event(Event::Stored { sender, content_hash: content_hash.into() });
			Ok(().into())
		}
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Stored data off chain.
		Stored { sender: T::AccountId, content_hash: sp_core::H256 },
		Hash { hash: [u8; 16] },
	}
}
