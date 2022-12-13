// This file is part of Substrate.

// Copyright (C) 2018-2022 Parity Technologies (UK) Ltd.
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

//! Substrate Blake2b Hasher implementation

use crate::{hash32_with_algorithm_inline, Hash32State};
use sp_storage::transient::Hash32Algorithm;

pub mod blake2 {
	use super::*;
	use crate::hash::H256;
	use hash256_std_hasher::Hash256StdHasher;
	use hash_db::Hasher;
	use sp_storage::transient::{HasherState, Hashers};

	/// Concrete implementation of Hasher using Blake2b 256-bit hashes
	#[derive(Debug)]
	pub struct Blake2Hasher;

	impl Hasher for Blake2Hasher {
		type Out = H256;
		type StdHasher = Hash256StdHasher;
		const LENGTH: usize = 32;

		fn hash(data: &[u8]) -> Self::Out {
			crate::hashing::blake2_256(data).into()
		}
	}

	impl Hashers for Blake2Hasher {
		const IS_HOST: bool = false;
		type HasherState = Hash32State;

		fn hash_with(data: &[u8], algo: Hash32Algorithm) -> [u8; 32] {
			hash32_with_algorithm_inline(data, algo)
		}

		fn hash_state_with(algo: Hash32Algorithm) -> Option<HasherState<Self::HasherState>> {
			Some(HasherState::Local(Hash32State::new(algo)))
		}

		fn hash_update(data: &[u8], state: &mut HasherState<Self::HasherState>) -> bool {
			let HasherState::Local(state) = state else {
				return false;
			};
			state.update(data);
			true
		}

		fn hash_finalize(state: HasherState<Self::HasherState>) -> Option<[u8; 32]> {
			let HasherState::Local(state) = state else {
				return None;
			};
			Some(state.finalize())
		}

		fn hash_drop(_state: Option<HasherState<Self::HasherState>>) {
			()
		}
	}
}

pub mod keccak {
	use super::*;
	use crate::hash::H256;
	use hash256_std_hasher::Hash256StdHasher;
	use hash_db::Hasher;
	use sp_storage::transient::{HasherState, Hashers};

	/// Concrete implementation of Hasher using Keccak 256-bit hashes
	#[derive(Debug)]
	pub struct KeccakHasher;

	impl Hasher for KeccakHasher {
		type Out = H256;
		type StdHasher = Hash256StdHasher;
		const LENGTH: usize = 32;

		fn hash(data: &[u8]) -> Self::Out {
			crate::hashing::keccak_256(data).into()
		}
	}

	impl Hashers for KeccakHasher {
		const IS_HOST: bool = false;
		type HasherState = Hash32State;

		fn hash_with(data: &[u8], algo: Hash32Algorithm) -> [u8; 32] {
			hash32_with_algorithm_inline(data, algo)
		}

		fn hash_state_with(algo: Hash32Algorithm) -> Option<HasherState<Self::HasherState>> {
			Some(HasherState::Local(Hash32State::new(algo)))
		}

		fn hash_update(data: &[u8], state: &mut HasherState<Self::HasherState>) -> bool {
			let HasherState::Local(state) = state else {
				return false;
			};
			state.update(data);
			true
		}

		fn hash_finalize(state: HasherState<Self::HasherState>) -> Option<[u8; 32]> {
			let HasherState::Local(state) = state else {
				return None;
			};
			Some(state.finalize())
		}

		fn hash_drop(_state: Option<HasherState<Self::HasherState>>) {
			()
		}
	}
}
