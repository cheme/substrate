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

//! Node backends for storing tree nodes.

use crate::{TreeConf, KeyIndexFor,
	NodeBox, Node, ValueState, IndexFor};
use alloc::vec::Vec;
use alloc::rc::Rc;
use alloc::boxed::Box;
use core::cell::RefCell;

pub mod node_test;

/// Node backend management.
pub trait Backend: Clone {
	const ACTIVE: bool = true;

	type Index: Eq;

	/// Get a root node from the backend.
	fn get_root(&self) -> Option<(Vec<u8>, Self::Index)>;

	fn get_node(&self, index: Self::Index) -> Option<Vec<u8>>;

	fn update_node(&self, index: Self::Index, node: Vec<u8>) -> Option<Self::Index>;

	fn remove_node(&self, index: Self::Index);

	fn set_new_node(&self, node: Vec<u8>) -> Self::Index;

	/// Root call back.
	fn set_root(&self, root: Self::Index);
}

impl Backend for () {
	const ACTIVE: bool = false;
	type Index = ();
	fn get_root(&self) -> Option<(Vec<u8>, Self::Index)> { None }

	fn get_node(&self, _index: Self::Index) -> Option<Vec<u8>> { None }

	fn update_node(&self, _index: Self::Index, _node: Vec<u8>) -> Option<Self::Index> { None }

	fn remove_node(&self, _index: Self::Index) { }

	fn set_new_node(&self, _node: Vec<u8>) -> Self::Index { () }

	fn set_root(&self, _root: Self::Index) { }
}

/// Information stored at node level for backend management.
pub trait NodeBackend<N: TreeConf>: Clone {
	// fuse node only on commit allows making change
	// without querying additional nodes, but also
	// creates empty node until commit.
	const FUSE_ON_COMMIT: bool = false;

	/// Just use `Box<Node<N>>` if inactive or `Child<Node<N>>` if active.
	type ChildState: crate::WithChildState<crate::Node<N>>;

	/// Get a root node from the backend.
	fn init_root(encoded_root: Vec<u8>, root_index: IndexFor<N>) -> Option<NodeBox<N>>;

	fn new_node_backend() -> Self;

	/// Get next defined node.
	fn get_next_child_index(&self, from: Option<KeyIndexFor<N>>) -> Option<KeyIndexFor<N>>;
	// TODO usefull ? fetch children?
	fn get_node(&self, index: IndexFor<N>, backend: &N::Backend) -> Option<NodeBox<N>>;

	// TODO replace by backend_hash_index -> bool ?
	fn fetch_children_index(&self, at: KeyIndexFor<N>) -> Option<IndexFor<N>>;
	/// Get a child node from the backend.
	/// TODO use Self::Index instead of usize??
	fn fetch_children(&mut self, at: KeyIndexFor<N>, backend: &N::Backend) -> Option<Option<NodeBox<N>>>;

	fn fetch_children_no_cache(&self, at: KeyIndexFor<N>, backend: &N::Backend) -> Option<NodeBox<N>>;

//	fn resolve_parent(&self) -> Option<NodeBox<N>>;

	// TODO this is not really good, probably need to be updated
	// redundant with node -> children method
	// -> new nb_children for node-children to set size and have
	// update from fetch not changing it.
	/// Original number of children stored in backend.
	fn fetch_nb_children(&mut self) -> Option<usize>;

	/// Current number of children for the node.
	/// If backend is inactive it can be simply read from node children
	/// and the function should never be call by code.
	fn current_nb_children(&self) -> usize;

	fn increase_number_of_children(&mut self);

	fn decrease_number_of_children(&mut self);

	// TODO replace by backend_hash_value -> bool ?
	fn fetch_value_index (&self) -> Option<IndexFor<N>>;

	fn fetch_value(&mut self, backend: &N::Backend) -> Option<Option<N::Value>>;

	fn fetch_value_no_cache(&self, backend: &N::Backend) -> Option<N::Value>;

	/// Indicate a child node was change.
	/// TODO could consider adding position to avoid iterating on all child
	/// TODO remove
	fn set_change(&mut self);

	fn set_children_change(&mut self) {
		self.set_change()
	}

	fn set_prefix_change(&mut self);
	
	/// Delete a node.
	/// Does flush change immediatly.
	fn delete(self, backend: &N::Backend);

	/// Flush changes that happens to a node.
	/// return new backend index if changed.
	fn commit_change(
		node: &mut NodeBox<N>,
		remove_node: &mut Vec<N::NodeBackend>,
		backend: &N::Backend,
	) -> Option<IndexFor<N>>;

	fn commit_change_fuse(
		node: &mut NodeBox<N>,
		remove_node: &mut Vec<N::NodeBackend>,
		backend: &N::Backend,
	) -> Option<IndexFor<N>> {
		if Self::FUSE_ON_COMMIT
			&& node.number_child() == 1
			&& !node.has_value()
		{
			Node::<N>::fuse_child(node, remove_node);
		}
		Self::commit_change(node, remove_node, backend)
	}

	/// Value state is stored in backend.
	fn value_state(&self) -> ValueState;

	/// Value state is stored in backend.
	fn set_value_state(&mut self, state: ValueState);

	/// Clear content to allow rewriting a different node instead.
	fn clear_content(&mut self);
}

impl<N: TreeConf> NodeBackend<N> for () {
	type ChildState = Box<Node<N>>;

	fn init_root(_encoded_root: Vec<u8>, _root_index: IndexFor<N>) -> Option<NodeBox<N>> {
		None
	}

	fn get_node(&self, _index: IndexFor<N>, _backend: &N::Backend) -> Option<NodeBox<N>> {
		None
	}

	fn get_next_child_index(&self, _index: Option<KeyIndexFor<N>>) -> Option<KeyIndexFor<N>> {
		None
	}
	fn fetch_children(&mut self, _at: KeyIndexFor<N>, _backend: &N::Backend) -> Option<Option<NodeBox<N>>> {
		None
	}

	fn fetch_children_index(&self, _at: KeyIndexFor<N>) -> Option<IndexFor<N>> {
		None
	}

	fn fetch_children_no_cache(&self, _at: KeyIndexFor<N>, _backend: &N::Backend) -> Option<NodeBox<N>> {
		None
	}

	fn fetch_nb_children(&mut self) -> Option<usize> {
		None
	}

	fn fetch_value_index (&self) -> Option<IndexFor<N>> {
		None
	}

	fn fetch_value(&mut self, _backend: &N::Backend) -> Option<Option<N::Value>> {
		None
	}

	fn fetch_value_no_cache(&self, _backend: &N::Backend) -> Option<N::Value> {
		None
	}

	fn set_change(&mut self) {
	}
	
	fn set_prefix_change(&mut self) {
	}

	fn delete(self, _: &N::Backend) {
	}

	fn commit_change(
		_node: &mut NodeBox<N>,
		_remove_node: &mut Vec<N::NodeBackend>,
		_: &N::Backend,
	) -> Option<IndexFor<N>> {
		None
	}

	fn new_node_backend() -> Self {
		()
	}
	fn value_state(&self) -> ValueState {
		ValueState::Resolved
	}
	fn set_value_state(&mut self, _state: ValueState) {
		()
	}

	fn clear_content(&mut self) { }

	fn current_nb_children(&self) -> usize {
		panic!("Inactive backend, no counting of children");
	}

	fn increase_number_of_children(&mut self) { }

	fn decrease_number_of_children(&mut self) { }
}

/// Backend containing encoded nodes.
#[derive(Clone, Debug)]
pub struct TestBackend {
	encoded_nodes: Vec<(Vec<u8>, Option<u64>)>,
	free_node: u64,
	new_node_on_update: bool,
	root_index: u64,
}

#[cfg(feature = "hash_backend")]
/// Set based backend containing encoded nodes
/// with hash as key.
/// Root is always stored at empty key.
#[derive(Clone, Debug)]
pub struct HashedTestBackend {
	encoded_nodes: hashbrown::HashMap<Vec<u8>, Vec<u8>>,
	pub root: Option<Vec<u8>>,
}

impl Default for TestBackend {
	fn default() -> Self {
		TestBackend {
			encoded_nodes: Vec::new(),
			free_node: 0,
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

	fn get_root(&self) -> Option<(Vec<u8>, u64)> {
		let s = self.borrow();
		s.get(s.root_index as usize).map(|v| (v, s.root_index))
	}

	fn get_node(&self, index: Self::Index) -> Option<Vec<u8>> {
		self.borrow().get(index as usize)
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

	fn remove_node(&self, index: Self::Index) {
		self.borrow_mut().remove(index)
	}

	fn set_new_node(&self, node: Vec<u8>) -> Self::Index {
		self.borrow_mut().insert(node)
	}

	fn set_root(&self, root: Self::Index) {
		self.borrow_mut().root_index = root;
	}
}

#[cfg(feature = "hash_backend")]
impl Backend for Rc<RefCell<HashedTestBackend>> {
	type Index = Vec<u8>; // TODO could switch to Blake2bResult (but need a reserved root key)
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

	fn remove_node(&self, index: Self::Index) {
		self.borrow_mut().encoded_nodes.remove(&index);
	}

	fn set_new_node(&self, node: Vec<u8>) -> Self::Index {
		let index = blake2_rfc::blake2b::blake2b(64, &[], &node).as_bytes().to_vec();
		self.borrow_mut().encoded_nodes.insert(index.clone(), node);
		index
	}

	fn set_root(&self, root: Self::Index) {
		self.borrow_mut().root = Some(root);
	}
}
