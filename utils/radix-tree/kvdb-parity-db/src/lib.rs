// This file is part of Substrate.

// Copyright (C) 2020-2020 Parity Technologies (UK) Ltd.
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

//! Index stored as radix tree in parity db free tables.
//!
//! Keyvalue db interface for paritydb.
//! Warning paritydb collection must have
//! a freetable declared in their column option
//! to store indexing.

use radix_tree::backend::Backend;
use parity_db::{Db, Result as DbResult};
use std::sync::Arc;

// Store test encoded 256 node (really unoptimal).
// and keep root_index as a standard index entry,
// by convention first file and first entry.
#[derive(Clone)]
struct TestBackend {
	db: Arc<Db>,
	columns: Vec<Option<Arc<ColBackend>>>,
	// TODO trie instance
}

struct ColBackend {
	root_index: u64,
	sizes: [u16; 15],
	col_values: Option<u8>,
	// TODO unimplemented: use col storage for values.
	// (then column must be define with rc).
	indexed_values: bool,
}

impl TestBackend {
	/// Instantiate new with dedicated index.
	pub fn new(db: Arc<Db>, columns_ix: &[(u8, Option<u8>)]) -> DbResult<Option<Self>> {
		let mut columns = Vec::new();
		for col in columns_ix {
			while columns.len() <= *col as usize {
				columns.push(None);
			}
			if let Some(root_index) = columns[*col as usize] {
				// TODO clone content change and then new Arc.
/*				if db.get(*col, root_index)?.is_none() {
					// initialize an empty radix tree (collection is considered empty).
					unimplemented!("TODO write empty root and write u64 index in tables: assert gives 0 ix");
				}*/
			}
		}
		Ok(Some(TestBackend {
			db,
			columns,
		}))
	}
}

impl Backend for TestBackend {
	type Index = u64;

	fn get_root(&self) -> Option<(Vec<u8>, u64)> {
		let s = self.0.read();
		s.get(s.root_index).map(|v| (v, s.root_index))
	}

	fn get_node(&self, index: Self::Index) -> Option<Vec<u8>> {
		self.0.read().get(index)
	}

	fn update_node(&self, index: Self::Index, node: Vec<u8>) -> Option<Self::Index> {
		 self.0.write().update(index, node)
	}

	fn remove_node(&self, index: Self::Index) {
		self.0.write().remove(index)
	}

	fn set_new_node(&self, node: Vec<u8>) -> Self::Index {
		self.0.write().insert(node)
	}

	fn set_root(&self, root: Self::Index) {
		self.0.write().root_index = root;
	}
}
