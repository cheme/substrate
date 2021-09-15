// This file is part of Substrate.

// Copyright (C) 2021 Parity Technologies (UK) Ltd.
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

//! Benchmarks for state-migrate-0-to-1

#![cfg(feature = "runtime-benchmarks")]

use frame_benchmarking::{benchmarks, impl_benchmark_test_suite};

use super::*;
use crate::Pallet as StateMigrate0To1;

benchmarks! {
	migrate {
		let n in 1 .. 5;
		/* TODO adding state this way do not work, result it trie backend error.
		 * (not in test benchmark)
		//let s in 10 .. 100;
		let s = 100;
		let seed = 394u32;
		let storage = sp_state_machine::build_random_storage(seed, s as usize);
		for (child_info, storage) in storage.0.into_iter() {
			if let Some(child) = child_info {
				for (key, value) in storage.into_iter() {
					sp_io::default_child_storage::set(child.storage_key(), key.as_slice(), value.as_slice());
				}
			} else {
				for (key, value) in storage.into_iter() {
					sp_io::storage::set(key.as_slice(), value.as_slice());
				}
			}
		}
		*/
	}: {
		for i in 0..10 {
			crate::on_initialize::<T>();
		}
	}
	verify {
		// no block run after pending
		matches!(
			crate::MigrationProgress::<T>::get(),
			MigrationState::Pending{ .. },
		)
	}
}

impl_benchmark_test_suite!(StateMigrate0To1, crate::mock::new_test_ext(true, Some((2342u32, 100))), crate::mock::Test);
