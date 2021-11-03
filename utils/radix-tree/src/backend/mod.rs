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

use crate::{TreeConf, KeyIndexFor, ValueIndexFor,
	NodeBox, Node, ValueState, IndexFor};
use alloc::vec::Vec;
use alloc::boxed::Box;

pub mod node_test;
pub mod art;
pub mod test_backend;

/// Node backend management.
pub trait Backend: Clone {
	const ACTIVE: bool = true;

	type Index: Eq;

	type ValueIndex: Clone + Eq;

	/// Get a root node from the backend.
	fn get_root(&self) -> Option<(Vec<u8>, Self::Index)>;

	fn get_node(&self, index: Self::Index) -> Option<Vec<u8>>;

	fn get_value(&self, index: Self::ValueIndex) -> Option<Vec<u8>>;

	fn update_node(&self, index: Self::Index, node: Vec<u8>) -> Option<Self::Index>;

	fn update_value(&self, index: Self::ValueIndex, value: Vec<u8>) -> Option<Self::ValueIndex>;

	fn remove_node(&self, index: Self::Index);

	fn remove_value(&self, index: Self::ValueIndex);

	fn set_new_node(&self, node: Vec<u8>) -> Self::Index;

	fn set_new_value(&self, value: Vec<u8>) -> Self::ValueIndex;

	/// Root call back.
	fn set_root(&self, root: Self::Index);
}

impl Backend for () {
	const ACTIVE: bool = false;
	type Index = ();
	type ValueIndex = ();
	fn get_root(&self) -> Option<(Vec<u8>, Self::Index)> { None }

	fn get_node(&self, _index: Self::Index) -> Option<Vec<u8>> { None }

	fn get_value(&self, _index: Self::ValueIndex) -> Option<Vec<u8>> { None }

	fn update_node(&self, _index: Self::Index, _node: Vec<u8>) -> Option<Self::Index> { None }

	fn update_value(&self, _index: Self::ValueIndex, _value: Vec<u8>) -> Option<Self::ValueIndex> {
		None
	}

	fn remove_node(&self, _index: Self::Index) { }

	fn remove_value(&self, _index: Self::ValueIndex) { }

	fn set_new_node(&self, _node: Vec<u8>) -> Self::Index { () }

	fn set_new_value(&self, _value: Vec<u8>) -> Self::ValueIndex { () }

	fn set_root(&self, _root: Self::Index) { }
}

/// Information stored at node level for backend management.
pub trait NodeBackend<N: TreeConf>: Clone {
	// fuse node only on commit allows making change
	// without querying additional nodes, but also
	// creates empty node until commit.
	const FUSE_ON_COMMIT: bool = false;

	// for some backend we want to delete child
	// in a first separate iteration to avoid going
	// over some size thresholds.
	// TODO same for art children
	const DELETE_CHILD_FIRST: bool = false;

	/// Just use `Box<Node<N>>` if inactive or `Child<Node<N>>` if active.
	type ChildState: crate::WithChildState<crate::Node<N>>;

	/// Get a root node from the backend.
	fn init_root(encoded_root: Vec<u8>, root_index: IndexFor<N>) -> Option<Node<N>>;

	fn new_node_backend() -> Self;

	/// Get next defined node.
	fn get_next_child_index(&self, from: Option<KeyIndexFor<N>>) -> Option<KeyIndexFor<N>>;

	fn get_node(&self, index: IndexFor<N>, backend: &N::Backend) -> Option<Node<N>>;

	// TODO replace by backend_hash_index -> bool ?
	fn fetch_children_index(&self, at: KeyIndexFor<N>) -> Option<IndexFor<N>>;

	/// Get a child node from the backend.
	fn fetch_children(&self, at: KeyIndexFor<N>, backend: &N::Backend) -> Option<Node<N>>;

	/// Original number of children stored in backend.
	fn fetch_nb_children(&mut self) -> Option<usize>;

	/// Current number of children for the node.
	/// If backend is inactive it can be simply read from node children
	/// and the function should never be call by code.
	fn current_nb_children(&self) -> usize;

	fn increase_number_of_children(&mut self);

	fn decrease_number_of_children(&mut self);

	// TODO replace by backend_hash_value -> bool ?
	fn fetch_value_index(&self) -> Option<ValueIndexFor<N>>;

	fn fetch_value(&self, backend: &N::Backend) -> Option<N::Value>;

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
	/// TODO remove commit change and have a single struct
	/// implementation for it: just implement read/writes/set as
	/// codec/accessor kind.
	fn commit_change(
		node: &mut NodeBox<N>,
		remove_node: &mut Vec<N::NodeBackend>,
		remove_value: &mut Vec<ValueIndexFor<N>>,
		backend: &N::Backend,
	) -> Option<IndexFor<N>>;

	fn commit_change_children_part(
		node: &mut NodeBox<N>,
		remove_node: &mut Vec<N::NodeBackend>,
		remove_value: &mut Vec<ValueIndexFor<N>>,
		backend: &N::Backend,
	) -> bool;

	// TODO not really needed, we need the chilren part for fuse,
	// but this can be deduced from clear content.
	fn commit_change_value_part(
		node: &mut NodeBox<N>,
		remove_node: &mut Vec<N::NodeBackend>,
		remove_value: &mut Vec<ValueIndexFor<N>>,
		backend: &N::Backend,
	) -> bool;

	fn commit_change_fuse(
		node: &mut NodeBox<N>,
		remove_node: &mut Vec<N::NodeBackend>,
		remove_value: &mut Vec<ValueIndexFor<N>>,
		backend: &N::Backend,
	) -> Option<IndexFor<N>> {
		if Self::FUSE_ON_COMMIT
			&& node.number_child() == 1
			&& !node.has_value()
		{
			Node::<N>::fuse_child(node, remove_node, remove_value, backend);
		}
		Self::commit_change(node, remove_node, remove_value, backend)
	}

	/// Value state is stored in backend.
	fn value_state(&self) -> ValueState;

	/// Value state is stored in backend.
	fn set_value_state(&mut self, state: ValueState);

	/// Clear content to allow rewriting a different node instead.
	/// Return value index to remove if there was a value.
	fn clear_content(&mut self) -> Option<ValueIndexFor<N>>;
}

impl<N: TreeConf> NodeBackend<N> for () {
	type ChildState = Box<Node<N>>;

	fn init_root(_encoded_root: Vec<u8>, _root_index: IndexFor<N>) -> Option<Node<N>> {
		None
	}

	fn get_node(&self, _index: IndexFor<N>, _backend: &N::Backend) -> Option<Node<N>> {
		None
	}

	fn get_next_child_index(&self, _index: Option<KeyIndexFor<N>>) -> Option<KeyIndexFor<N>> {
		None
	}
	fn fetch_children(&self, _at: KeyIndexFor<N>, _backend: &N::Backend) -> Option<Node<N>> {
		None
	}

	fn fetch_children_index(&self, _at: KeyIndexFor<N>) -> Option<IndexFor<N>> {
		None
	}

	fn fetch_nb_children(&mut self) -> Option<usize> {
		None
	}

	fn fetch_value_index(&self) -> Option<ValueIndexFor<N>> {
		None
	}

	fn fetch_value(&self, _backend: &N::Backend) -> Option<N::Value> {
		None
	}

	fn set_change(&mut self) {
	}
	
	fn set_prefix_change(&mut self) {
	}

	fn delete(self, _: &N::Backend) {
	}

	fn commit_change_children_part(
		_node: &mut NodeBox<N>,
		_remove_node: &mut Vec<N::NodeBackend>,
		_remove_value: &mut Vec<ValueIndexFor<N>>,
		_: &N::Backend,
	) -> bool {
		false
	}

	fn commit_change_value_part(
		_node: &mut NodeBox<N>,
		_remove_node: &mut Vec<N::NodeBackend>,
		_remove_value: &mut Vec<ValueIndexFor<N>>,
		_: &N::Backend,
	) -> bool {
		false
	}

	fn commit_change(
		_node: &mut NodeBox<N>,
		_remove_node: &mut Vec<N::NodeBackend>,
		_remove_value: &mut Vec<ValueIndexFor<N>>,
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

	fn clear_content(&mut self) -> Option<ValueIndexFor<N>> { None }

	fn current_nb_children(&self) -> usize {
		panic!("Inactive backend, no counting of children");
	}

	fn increase_number_of_children(&mut self) { }

	fn decrease_number_of_children(&mut self) { }
}
