// This file is part of Substrate.

// Copyright (C) 2020-2022 Parity Technologies (UK) Ltd.
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

//! # Parallel Tasks Example Pallet
//!
//! This example pallet demonstrates parallelizing validation of the enlisted participants
//! (see `enlist_participants` dispatch).
//!
//! **This pallet serves as an example and is not meant to be used in production.**

#![cfg_attr(not(feature = "std"), no_std)]

use sp_runtime::RuntimeDebug;

use codec::{Decode, Encode};
use sp_std::vec::Vec;

#[cfg(test)]
mod tests;

pub use pallet::*;

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching dispatch call type.
		type Call: From<Call<Self>>;
	}

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	#[pallet::without_storage_info]
	pub struct Pallet<T>(_);

	/// A public part of the pallet.
	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Get the new event running.
		#[pallet::weight(0)]
		pub fn run_event(origin: OriginFor<T>, id: Vec<u8>) -> DispatchResultWithPostInfo {
			let _ = ensure_signed(origin)?;
			<Participants<T>>::kill();
			<CurrentEventId<T>>::mutate(move |event_id| *event_id = id);
			Ok(().into())
		}

		/// Submit list of participants to the current event.
		///
		/// The example utilizes parallel execution by checking half of the
		/// signatures in spawned task.
		#[pallet::weight(0)]
		pub fn enlist_participants(
			origin: OriginFor<T>,
			participants: Vec<EnlistedParticipant>,
		) -> DispatchResultWithPostInfo {
			let _ = ensure_signed(origin)?;

			if validate_participants_parallel(&<CurrentEventId<T>>::get(), &participants[..]) {
				for participant in participants {
					<Participants<T>>::append(participant.account);
				}
			}
			Ok(().into())
		}

		/// Submit list of pending participants to the current event.
		#[pallet::weight(0)]
		pub fn enlist_pending_participants(
			origin: OriginFor<T>,
			participants: Vec<EnlistedParticipant>,
		) -> DispatchResult {
			let _ = ensure_signed(origin)?;

			for participant in participants {
				<PendingParticipants<T>>::append(participant);
			}
			Ok(())
		}

		/// Validate a given number of pending participant.
		///
		/// This uses the current read state to validate in parallel.
		/// It removes participants that are invalid from pending list
		/// and process the valid ones.
		#[pallet::weight(0)]
		pub fn validate_pendings_participants(
			_origin: OriginFor<T>,
			number: u32,
		) -> DispatchResult {
			validate_pending_participants_parallel::<T>(number as usize);
			Ok(())
		}
	}

	/// A vector of current participants
	///
	/// To enlist someone to participate, signed payload should be
	/// sent to `enlist`.
	#[pallet::storage]
	#[pallet::getter(fn participants)]
	pub(super) type Participants<T: Config> = StorageValue<_, Vec<Vec<u8>>, ValueQuery>;

	/// A vector of pending participants
	///
	/// They need to be verified before being added to participants.
	#[pallet::storage]
	#[pallet::getter(fn pending_participants)]
	pub(super) type PendingParticipants<T: Config> =
		StorageValue<_, Vec<EnlistedParticipant>, ValueQuery>;

	/// Current event id to enlist participants to.
	#[pallet::storage]
	#[pallet::getter(fn get_current_event_id)]
	pub(super) type CurrentEventId<T: Config> = StorageValue<_, Vec<u8>, ValueQuery>;
}

/// Request to enlist participant.
#[derive(Encode, Decode, Clone, PartialEq, Eq, RuntimeDebug, scale_info::TypeInfo)]
pub struct EnlistedParticipant {
	pub account: Vec<u8>,
	pub signature: Vec<u8>,
}

impl EnlistedParticipant {
	fn verify(&self, event_id: &[u8]) -> bool {
		use sp_core::ByteArray;
		use sp_runtime::traits::Verify;

		match sp_core::sr25519::Signature::try_from(&self.signature[..]) {
			Ok(signature) => match sp_core::sr25519::Public::from_slice(self.account.as_ref()) {
				Err(()) => false,
				Ok(signer) => signature.verify(event_id, &signer),
			},
			_ => false,
		}
	}
}

fn validate_participants_parallel(event_id: &[u8], participants: &[EnlistedParticipant]) -> bool {
	fn spawn_verify(data: Vec<u8>) -> Vec<u8> {
		let stream = &mut &data[..];
		let event_id = Vec::<u8>::decode(stream).expect("Failed to decode");
		let participants = Vec::<EnlistedParticipant>::decode(stream).expect("Failed to decode");

		for participant in participants {
			if !participant.verify(&event_id) {
				return false.encode()
			}
		}
		true.encode()
	}

	let mut async_payload = Vec::new();
	event_id.encode_to(&mut async_payload);
	participants[..participants.len() / 2].encode_to(&mut async_payload);

	let handle =
		sp_tasks::spawn(spawn_verify, async_payload, sp_tasks::WorkerDeclaration::stateless())
			.expect("Worker run as stateless");
	let mut result = true;

	for participant in &participants[participants.len() / 2..] {
		if !participant.verify(event_id) {
			result = false;
			break
		}
	}
	match handle.join() {
		Some(encoded) =>
			bool::decode(&mut &encoded[..]).expect("Failed to decode result") && result,
		None => {
			unreachable!("Worker run as stateless")
		},
	}
}

fn validate_pending_participants_parallel<T: Config>(number: usize) {
	fn spawn_verify<T: Config>(data: Vec<u8>) -> Vec<u8> {
		let stream = &mut &data[..];
		let split = u32::decode(stream).expect("Failed to decode") as usize;
		let participants = PendingParticipants::<T>::get();
		let event_id = CurrentEventId::<T>::get();
		let mut to_skip = Vec::new();

		for (index, participant) in (&participants[..split]).iter().enumerate() {
			if !participant.verify(&event_id) {
				to_skip.push(index as u32);
			}
		}
		to_skip.encode()
	}

	let participants = PendingParticipants::<T>::get();
	let event_id = CurrentEventId::<T>::get();

	let number = sp_std::cmp::min(participants.len(), number);
	let split = number / 2;
	let mut async_payload = Vec::new();
	// We should really skip spawn when split is 0, but this is just an example.
	(split as u32).encode_to(&mut async_payload);

	let handle = sp_tasks::spawn(
		spawn_verify::<T>,
		async_payload,
		sp_tasks::WorkerType::ReadAtSpawn.into(),
	)
	.expect("Declaration incompatible with other running workers.");

	for participant in &participants[split..number] {
		if participant.verify(&event_id) {
			Participants::<T>::append(participant.account.clone());
		}
	}
	let mut to_skip: Vec<u32> = match handle.join() {
		Some(result) => Decode::decode(&mut &result[..]).expect("Failed to decode result"),
		None => {
			unreachable!("Transaction bug")
		},
	};

	for (index, participant) in (&participants[..split]).iter().enumerate() {
		if Some(&(index as u32)) == to_skip.first() {
			to_skip.remove(0);
		} else {
			Participants::<T>::append(participant.account.clone());
		}
	}
	let mut participants = participants;
	PendingParticipants::<T>::set(participants.split_off(number));
}
