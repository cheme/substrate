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

//! Backends for test.

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;
use crate::Backend;

/// Backend containing encoded nodes.
#[derive(Clone, Debug)]
pub struct TestBackend {
	encoded_nodes: Vec<(Vec<u8>, Option<u64>)>,
	free_node: u64,
	root_index: u64,
	new_node_on_update: bool,
}

#[cfg(feature = "hash_backend")]
/// Set based backend containing encoded nodes
/// with hash as key.
/// Root is always stored at empty key.
/// TODO since keys are blake hash, this needs rc.
#[derive(Clone, Debug)]
pub struct HashedTestBackend {
	encoded_nodes: hashbrown::HashMap<Vec<u8>, Vec<u8>>,
	values: hashbrown::HashMap<Vec<u8>, Vec<u8>>,
	pub root: Option<Vec<u8>>,
}

impl Default for TestBackend {
	fn default() -> Self {
		TestBackend {
			// empty root (ptr to del is defined).
			encoded_nodes: vec![(Vec::new(), Some(0))],
			free_node: 1,
			new_node_on_update: true,
			root_index: 0,
		}
	}
}

#[cfg(feature = "hash_backend")]
impl Default for HashedTestBackend {
	fn default() -> Self {
		HashedTestBackend {
			encoded_nodes: Default::default(),
			values: Default::default(),
			root: None,
		}
	}
}

impl TestBackend {
	fn remove(&mut self, index: u64) {
		if let Some(node) = self.encoded_nodes.get_mut(index as usize) {
			if node.1.is_none() {
				node.1 = Some(self.free_node);
				self.free_node = index;
			}
		}
	}
	fn get(&self, index: usize) -> Option<Vec<u8>> {
		if let Some(node) = self.encoded_nodes.get(index) {
			if node.1.is_none() {
				return Some(node.0.clone())
			}
		}
		None
	}
	fn insert(&mut self, content: Vec<u8>) -> u64 {
		let result = self.free_node;
		if self.free_node == self.encoded_nodes.len() as u64 {
			self.encoded_nodes.push((content, None));
			self.free_node = self.encoded_nodes.len() as u64;
		} else {
			let node = self.encoded_nodes.get_mut(self.free_node as usize).expect("Free node oversized.");
			self.free_node = node.1.take().expect("free node contains pointer");
			node.0 = content;
		}
		result
	}
}

impl Backend for Rc<RefCell<TestBackend>> {
	type Index = u64;
	type ValueIndex = u64;

	fn get_root(&self) -> Option<(Vec<u8>, u64)> {
		let s = self.borrow();
		s.get(s.root_index as usize).map(|v| (v, s.root_index))
	}

	fn get_node(&self, index: Self::Index) -> Option<Vec<u8>> {
		self.borrow().get(index as usize)
	}

	fn get_value(&self, index: Self::ValueIndex) -> Option<Vec<u8>> {
		self.get_node(index)
	}

	fn update_node(&self, index: Self::Index, node: Vec<u8>) -> Option<Self::Index> {
		let mut s = self.borrow_mut();
		if s.new_node_on_update {
			let result = s.insert(node);
			s.remove(index);
			Some(result as u64)
		} else {
			s.encoded_nodes[index as usize].0 = node;
			debug_assert!(s.encoded_nodes[index as usize].1.is_none());
			None
		}
	}

	fn update_value(&self, index: Self::ValueIndex, value: Vec<u8>) -> Option<Self::Index> {
		self.update_node(index, value)
	}

	fn remove_node(&self, index: Self::Index) {
		self.borrow_mut().remove(index)
	}

	fn remove_value(&self, index: Self::ValueIndex) {
		self.remove_node(index)
	}

	fn set_new_node(&self, node: Vec<u8>) -> Self::Index {
		self.borrow_mut().insert(node)
	}

	fn set_new_value(&self, value: Vec<u8>) -> Self::ValueIndex {
		self.set_new_node(value)
	}

	fn set_root(&self, root: Self::Index) {
		self.borrow_mut().root_index = root;
	}
}

#[cfg(feature = "hash_backend")]
impl Backend for Rc<RefCell<HashedTestBackend>> {
	type Index = Vec<u8>;
	type ValueIndex = Vec<u8>;
	fn get_root(&self) -> Option<(Vec<u8>, Self::Index)> {
		let s = self.borrow();
		if let Some(root_hash) = s.root.clone() {
			self.get_node(root_hash.clone()).map(|root| (root, root_hash))
		} else {
			None
		}
	}

	fn get_node(&self, index: Self::Index) -> Option<Vec<u8>> {
		self.borrow().encoded_nodes.get(&index).cloned()
	}

	fn get_value(&self, index: Self::ValueIndex) -> Option<Vec<u8>> {
		self.borrow().values.get(&index).cloned()
	}

	fn update_node(&self, index: Self::Index, node: Vec<u8>) -> Option<Self::Index> {
		let mut s = self.borrow_mut();
		let new_index = blake2_rfc::blake2b::blake2b(64, &[], &node).as_bytes().to_vec();
		if new_index == index {
			s.encoded_nodes.insert(index, node);
			None
		} else {
			s.encoded_nodes.insert(new_index.clone(), node);
			s.encoded_nodes.remove(&index);
			Some(new_index)
		}
	}

	fn update_value(&self, index: Self::Index, value: Vec<u8>) -> Option<Self::Index> {
		let mut s = self.borrow_mut();
		let new_index = blake2_rfc::blake2b::blake2b(32, &[], &value).as_bytes().to_vec();
		if new_index == index {
			s.values.insert(index, value);
			None
		} else {
			s.values.insert(new_index.clone(), value);
			s.values.remove(&index);
			Some(new_index)
		}
	}

	fn remove_node(&self, index: Self::Index) {
		self.borrow_mut().encoded_nodes.remove(&index);
	}

	fn remove_value(&self, index: Self::ValueIndex) {
		self.borrow_mut().values.remove(&index);
	}

	fn set_new_node(&self, node: Vec<u8>) -> Self::Index {
		let index = blake2_rfc::blake2b::blake2b(64, &[], &node).as_bytes().to_vec();
		self.borrow_mut().encoded_nodes.insert(index.clone(), node);
		index
	}

	fn set_new_value(&self, value: Vec<u8>) -> Self::ValueIndex {
		let index = blake2_rfc::blake2b::blake2b(32, &[], &value).as_bytes().to_vec();
		self.borrow_mut().values.insert(index.clone(), value);
		index
	}
	fn set_root(&self, root: Self::Index) {
		self.borrow_mut().root = Some(root);
	}
}
