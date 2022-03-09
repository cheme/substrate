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

//! Local scope for worker.
//!
//! Declaring a write local scope allows working on local change
//! without write conflict.
//! The absence of conflict is related to the fact that changes in
//! local scope never reach the parent worker and are dropped.
//! Changes can still be return as result, but this implies no
//! concurrent access.
//!
//! Read access in write local scope are still checked for concurrency
//! conflict (we do not run on a copy of the state).

use sp_externalities::AccessDeclaration;

/// Storage of scope and related operation.
#[derive(Debug, Clone, Default)]
pub(super) struct WriteLocalScope {
	// TODO
//	write_local_scope: Option<FilterTrees<Filter>>,
}

#[derive(Debug, Clone)]
pub enum Filter {
	Prefix,
	Key,
}

// just to satisfy bound
impl Default for Filter {
	fn default() -> Self {
		Filter::Key
	}
}

impl WriteLocalScope {
	pub(super) fn set_write_local_scope(&mut self, scope: Option<AccessDeclaration>) {
		if let Some(declaration) = scope {
			unimplemented!();
		}
	}
}
