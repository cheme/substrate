// This file is part of Substrate.

// Copyright (C) 2022 Parity Technologies (UK) Ltd.
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

//! Binary for checking migration state through remote externality rpc.
//!
//! Please use `WS_API` environment variable to set the rpc web socket
//! endpoint.

use pallet_state_trie_migration::remote_tests::migration_state;
use remote_externalities::Mode;

type Block = sp_runtime::testing::Block<Extrinsic>;

fn main() {
	sp_tracing::try_init_simple();
	let mode = Mode::OfflineOrElseOnline(
		OfflineConfig { state_snapshot: "~/remote-builds/state".to_owned().into() },
		OnlineConfig {
			transport: std::env!("WS_API").to_owned().into(),
			state_snapshot: None,
			..Default::default()
		},
	);

	let (remaining_top, remaining_child) = migration_state::<Test, Block>(mode).await;
	println!("Remaining top : {:?}, Remaining children : {:?}", remaining_top, remaining_child);
}
