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

use crate::{TreeConf, PositionFor, KeyIndexFor,
	NodeBox, RadixConf,
	PrefixKeyConf, Node, PrefixKey, AlignmentFor,
	ValueState};
use crate::radix::{MaskFor, MaskKeyByte};
use crate::children::NodeIndex;
use alloc::vec::Vec;
use alloc::rc::Rc;
use alloc::boxed::Box;
use alloc::vec;
use codec::{Encode, Decode, Input};
use core::cell::RefCell;

/// Node backend management.
pub trait Backend: Clone {
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
	const ACTIVE: bool = true;
	// fuse node only on commit allows making change
	// without querying additional nodes, but also
	// creates empty node until commit.
	const FUSE_ON_COMMIT: bool = false;

	/// Just use `Box<Node<N>>` if inactive or `Child<Node<N>>` if active.
	type ChildState: crate::WithChildState<crate::Node<N>>;

	/// Inner backend used.
	type Backend: Backend<Index = Self::Index>;
	type Index;

	/// TODO consider keeping backend out of node.
	fn backend(&self) -> &Self::Backend;

	/// Get a root node from the backend.
	/// TODO specialize return node to commit recursively on drop.
	fn get_root(init: &Self::Backend) -> Option<NodeBox<N>>;

	/// Get next defined node.
	fn get_next_child_index(&self, from: Option<KeyIndexFor<N>>) -> Option<KeyIndexFor<N>>;
	// TODO usefull ? fetch children?
	fn get_node(&self, index: Self::Index) -> Option<NodeBox<N>>;

	// TODO replace by backend_hash_index -> bool ?
	fn fetch_children_index(&self, at: KeyIndexFor<N>) -> Option<Self::Index>;
	/// Get a child node from the backend.
	/// TODO use Self::Index instead of usize??
	fn fetch_children(&mut self, at: KeyIndexFor<N>) -> Option<Option<NodeBox<N>>>;

	fn fetch_children_no_cache(&self, at: KeyIndexFor<N>) -> Option<NodeBox<N>>;

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
	fn fetch_value_index (&self) -> Option<Self::Index>;

	fn fetch_value(&mut self) -> Option<Option<N::Value>>;

	fn fetch_value_no_cache(&self) -> Option<N::Value>;

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
	fn delete(self);

	/// Flush changes that happens to a node.
	/// return new backend index if changed.
	fn commit_change(node: &mut NodeBox<N>, remove_node: &mut Vec<N::NodeBackend>) -> Option<Self::Index>;

	fn commit_change_fuse(node: &mut NodeBox<N>, remove_node: &mut Vec<N::NodeBackend>) -> Option<Self::Index> {
		if Self::FUSE_ON_COMMIT
			&& node.number_child() == 1
			&& !node.has_value()
		{
			Node::<N>::fuse_child(node, remove_node);
		}
		Self::commit_change(node, remove_node)
	}

	fn new_node_backend(backend: &Self::Backend) -> Self;

	/// Value state is stored in backend.
	fn value_state(&self) -> ValueState;

	/// Value state is stored in backend.
	fn set_value_state(&mut self, state: ValueState);

	/// Clear content to allow rewriting a different node instead.
	fn clear_content(&mut self);
}


impl<N: TreeConf> NodeBackend<N> for () {
	const ACTIVE: bool = false;
	type ChildState = Box<Node<N>>;

	type Backend = ();
	type Index = ();

	fn backend(&self) -> &Self::Backend { self }

	fn get_root(_init: &Self::Backend) -> Option<NodeBox<N>> { None }

	fn get_node(&self, _index: Self::Index) -> Option<NodeBox<N>> {
		None
	}
	fn get_next_child_index(&self, _index: Option<KeyIndexFor<N>>) -> Option<KeyIndexFor<N>> {
		None
	}
	fn fetch_children(&mut self, _at: KeyIndexFor<N>) -> Option<Option<NodeBox<N>>> {
		None
	}

	fn fetch_children_index(&self, _at: KeyIndexFor<N>) -> Option<Self::Index> {
		None
	}

	fn fetch_children_no_cache(&self, _at: KeyIndexFor<N>) -> Option<NodeBox<N>> {
		None
	}

	fn fetch_nb_children(&mut self) -> Option<usize> {
		None
	}

	fn fetch_value_index (&self) -> Option<Self::Index> {
		None
	}

	fn fetch_value(&mut self) -> Option<Option<N::Value>> {
		None
	}

	fn fetch_value_no_cache(&self) -> Option<N::Value> {
		None
	}

	fn set_change(&mut self) {
	}
	
	fn set_prefix_change(&mut self) {
	}

	fn delete(self) {
	}

	fn commit_change(_node: &mut NodeBox<N>, _remove_node: &mut Vec<N::NodeBackend>) -> Option<Self::Index> {
		None
	}

	fn new_node_backend(_backend: &Self::Backend) -> Self {
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

/// Test only backend with lazy access.
#[derive(Clone, Debug)]
pub struct NodeTestBackend<N: TreeConf, B: Backend> {
	// None for new node only.
	index: Option<B::Index>,
	encoded: Vec<u8>,
	child_index: Vec<Option<B::Index>>,
	nb_children: usize,
	pending_nb_children: usize,
	value: Option<B::Index>,
	value_state: ValueState,
	backend: B,
	children_changed: bool,
	prefix_changed: bool,
	_ph: core::marker::PhantomData<N>,
}

// TODO replace backend by N::Backend when attached
fn decode<N, B>(
	encoded: Vec<u8>,
	backend: &B,
	index: B::Index,
) -> (NodeTestBackend<N, B>, PrefixKey<Vec<u8>, AlignmentFor<N>>)
	where
		N: TreeConf,
		N::Value: Decode,
		B: Backend,
		B::Index: Decode,
{
	let input = &mut encoded.as_slice();
	let value: Option<B::Index> = Decode::decode(input).unwrap();
	let mut child_index = Vec::new();
	let mut nb_children = 0;
	for _ in 0..<<N as TreeConf>::Radix as RadixConf>::CHILDREN_CAPACITY {
		let child: Option<B::Index> = Decode::decode(input).unwrap();
		if child.is_some() {
			nb_children += 1;
		}
		child_index.push(child.map(|v| v));
	}

	let start_mask = if let Some(mask) = <N::Radix as RadixConf>::Alignment::DEFAULT {
		mask
	} else {
		let b = input.read_byte().unwrap();
		<N::Radix as RadixConf>::Alignment::decode_mask(b)
	};
	let start = PositionFor::<N> {
		index: 0,
		mask: start_mask,
	};
	let end_mask = if let Some(mask) = <N::Radix as RadixConf>::Alignment::DEFAULT {
		mask
	} else {
		let b = input.read_byte().unwrap();
		<N::Radix as RadixConf>::Alignment::decode_mask(b)
	};
	let prefix: Vec<u8> = Decode::decode(input).unwrap();
	let end = if end_mask == MaskFor::<N::Radix>::FIRST {
		PositionFor::<N>  {
			index: prefix.len(),
			mask: end_mask,
		}
	} else {
		PositionFor::<N>  {
			index: prefix.len() - 1,
			mask: end_mask,
		}
	};
	let prefix = PrefixKey::new_offset(prefix, start, end);

	(NodeTestBackend {
		index: Some(index),
		encoded,
		child_index,
		value,
		value_state: ValueState::Unfetched,
		pending_nb_children: nb_children,
		nb_children,
		backend: backend.clone(),
		children_changed: false,
		prefix_changed: false,
		_ph: Default::default(),
	}, prefix)
}

fn encode_node<N, B>(node: &Node<N>) -> Vec<u8>
where
		N: TreeConf<NodeBackend = NodeTestBackend<N, B>>,
		N::Value: Encode,
		B: Backend,
		B::Index: Encode,
{
	let mut result = Vec::new();

	/*if node.backend.value_state == ValueState::Modified
		|| node.backend.value_state == ValueState::Deleted {
		node.value.encode_to(&mut result);
	} else {*/
	node.backend.value.encode_to(&mut result);
	//}

	for i in 0..<<N as TreeConf>::Radix as RadixConf>::CHILDREN_CAPACITY {
		let ref_index = &node.backend.child_index[i];
		/*let ref_index: Option<usize> = if fetched {
			if let Some(child) = node.get_child(KeyIndexFor::<N>::from_usize(i)) {
				child.backend.index
			} else {
				unreachable!("was fetched");
			}
		} else {
			ref_index
		};*/
		ref_index.encode_to(&mut result);
	}
	if let Some(_) = <N::Radix as RadixConf>::Alignment::DEFAULT {
	} else {
		result.push(<N::Radix as RadixConf>::Alignment::encode_mask(node.key.start));
	};
	node.key.data.encode_to(&mut result);
	if let Some(_) = <N::Radix as RadixConf>::Alignment::DEFAULT {
	} else {
		result.push(<N::Radix as RadixConf>::Alignment::encode_mask(node.key.end));
	};
	
	result
}


impl<N, B> NodeBackend<N> for NodeTestBackend<N, B>
	where
		N: TreeConf<NodeBackend = Self>,
		N::Value: Decode + Encode,
		B: Backend,
		B::Index: Clone + Decode + Encode,
{
	const ACTIVE: bool = true;
	const FUSE_ON_COMMIT: bool = true;
	type ChildState = crate::Child<Node<N>>;
	//type ChildState = Box<Node<N>>;

	type Backend = B;
	type Index = <Self::Backend as Backend>::Index;

	fn backend(&self) -> &Self::Backend {
		&self.backend
	}

	fn get_root(init: &Self::Backend) -> Option<NodeBox<N>> {
		if let Some((encoded, index)) = init.get_root() {
			let (backend, prefix) = decode::<N, B>(encoded, init, index);
			Some(Node::<N>::new_box_unfetched(
				prefix,
				backend,
			))
		} else {
			None
		}
	}
	fn get_node(&self, index: Self::Index) -> Option<NodeBox<N>> {
		if let Some(encoded) = self.backend.get_node(index.clone()) {
			let (backend, prefix) = decode::<N, B>(encoded, &self.backend, index);
			Some(Node::<N>::new_box_unfetched(
				prefix,
				backend,
			))
		} else {
			None
		}
	}

	fn get_next_child_index(&self, mut index: Option<KeyIndexFor<N>>) -> Option<KeyIndexFor<N>> {
		while let Some(i) = match index {
			Some(i) => i.next(),
			None => Some(KeyIndexFor::<N>::zero()),
		} {
			if let Some(Some(_)) = self.child_index.get(i.to_usize()) {
				return Some(i);
			}
			index = Some(i);
		}
		None
	}

	fn fetch_children_index(&self, at: KeyIndexFor<N>) -> Option<Self::Index> {
		if let Some(Some(ix)) = self.child_index.get(at.to_usize()) {
			Some(ix.clone())
		} else {
			None
		}
	}

	fn fetch_children(&mut self, at: KeyIndexFor<N>) -> Option<Option<NodeBox<N>>> {
		if let Some(child) = self.child_index.get_mut(at.to_usize()) {
			if let Some(index) = child.clone() {
				return Some(self.get_node(index));
			} else {
				return Some(None);
			}
		}
		None
	}

	fn fetch_children_no_cache(&self, at: KeyIndexFor<N>) -> Option<NodeBox<N>> {
		if let Some(child) = self.child_index.get(at.to_usize()) {
			if let Some(index) = child.clone() {
				return self.get_node(index);
			}
		}
		None
	}

	fn fetch_nb_children(&mut self) -> Option<usize> {
		Some(self.nb_children)
	}

	fn fetch_value_index (&self) -> Option<Self::Index> {
		debug_assert!(self.value_state == ValueState::Unfetched);
		self.value.clone()
	}

	fn fetch_value(&mut self) -> Option<Option<N::Value>> {
		if self.value_state != ValueState::Unfetched {
			None
		} else {
			self.value_state = ValueState::Resolved;
			if let Some(ix) = self.value.clone() {
				let value = self.backend.get_node(ix).expect("Value stored with node.");
				Some(Some(Decode::decode(&mut value.as_slice()).unwrap()))
			} else {
				Some(None)
			}
		}
	}
	fn fetch_value_no_cache(&self) -> Option<N::Value> {
		if let Some(ix) = self.value.clone() {
			self.backend.get_node(ix).map(|value| Decode::decode(&mut value.as_slice()).unwrap())
		} else {
			None
		}
	}
	fn set_change(&mut self) {
		self.children_changed = true;
	}
	fn set_prefix_change(&mut self) {
		self.prefix_changed = true;
	}

	fn delete(self) {
		if let Some(index) = self.index {
			self.backend.remove_node(index);
		}
	}

	fn new_node_backend(backend: &Self::Backend) -> Self {
		NodeTestBackend {
			index: None,
			encoded: Vec::new(),
			child_index: vec![None; <<N as TreeConf>::Radix as RadixConf>::CHILDREN_CAPACITY],
			value: None,
			nb_children: 0,
			pending_nb_children: 0,
			backend: backend.clone(),
			children_changed: false,
			prefix_changed: true, // new node need update
			value_state: ValueState::Resolved,
			_ph: Default::default(),
		}
	}

	fn commit_change(node: &mut NodeBox<N>, remove_node: &mut Vec<N::NodeBackend>) -> Option<Self::Index> {
		let mut encoded_change = false;
		let changed = node.backend.children_changed
			|| node.backend.prefix_changed
			|| node.backend.value_state == ValueState::Modified
			|| node.backend.value_state == ValueState::Deleted;
		if !changed {
			return None;
		}
		if node.backend.children_changed {
			// recurse commit of resolved children and update backend indexes.
			let mut index = None;
			let mut backend_ix = 0;
			while let Some(i) = node.get_next_child_index(index) {
				while backend_ix < i.to_usize() {
					node.backend.child_index[backend_ix] = None; // ensure deleted (could also get next on both).
					backend_ix += 1;
				}
				use crate::children::Children;
				use crate::WithChildState;
				if let Some(mut child) = node.children.get_child_mut(i).and_then(|c| c.node_mut()) {
					if let Some(ix) = Self::commit_change_fuse(&mut child, remove_node) {
						encoded_change = true;
						node.backend.child_index[backend_ix] = Some(ix);
					} else {
						// set old index (can be unset due to value not new but split
						// that makes it a new child.
						node.backend.child_index[backend_ix] = child.backend.index.clone();
					}
				}
				backend_ix = i.to_usize() + 1;
				index = Some(i);
			}
			while backend_ix < <<N as TreeConf>::Radix as RadixConf>::CHILDREN_CAPACITY {
				if node.backend.child_index[backend_ix].is_some() {
					node.backend.child_index[backend_ix] = None;
					encoded_change = true;
				}
				backend_ix += 1;
			}
		}
		if node.backend.value_state == ValueState::Modified {
			if let Some(value) = node.value.as_ref() {
				let encoded = value.encode();
				let ix = if let Some(previous) = node.backend.value.clone() {
					node.backend.backend.update_node(previous.clone(), encoded).unwrap_or(previous)
				} else {
					node.backend.backend.set_new_node(encoded)
				};

				if node.backend.value.as_ref() != Some(&ix) {
					node.backend.value = Some(ix);
					encoded_change = true;
				}
				node.backend.value_state = ValueState::Resolved;
			} else {
				unreachable!("modified should be some");
			}
		}
		if node.backend.value_state == ValueState::Deleted {
			if let Some(previous) = node.backend.value.clone() {
				node.backend.backend.remove_node(previous);
				encoded_change = true;
			} else {
				unreachable!("deleted should be some");
			};
			node.backend.value = None;
			node.backend.value_state = ValueState::Resolved;
		}

		node.backend.children_changed = false;
		node.backend.nb_children = {
			use crate::children::Children;
			node.children.number_child()
		};
		encoded_change = encoded_change || node.backend.prefix_changed;

		node.backend.prefix_changed = false;

		if encoded_change {
			let encoded = encode_node::<N, B>(node);
			// TODO additional change check??
			if let Some(i) = node.backend.index.as_ref() {
				node.backend.backend.update_node(i.clone(), encoded)
			} else {
				Some(node.backend.backend.set_new_node(encoded))
			}
		} else {
			None
		}
	}

	fn value_state(&self) -> ValueState {
		self.value_state
	}

	fn set_value_state(&mut self, state: ValueState) {
		self.value_state = state
	}

	fn clear_content(&mut self) {
		self.encoded.clear();
		self.child_index = vec![None; <<N as TreeConf>::Radix as RadixConf>::CHILDREN_CAPACITY];
		// Note here if value is stored in different node, we would need to remove it.
		self.value = None;
		self.nb_children = 0;
		self.children_changed = false;
		self.prefix_changed = true; // to ensure update
		self.value_state = ValueState::Resolved;
	}

	fn current_nb_children(&self) -> usize {
		self.pending_nb_children
	}

	fn increase_number_of_children(&mut self) {
		self.pending_nb_children += 1;
	}

	fn decrease_number_of_children(&mut self) {
		self.pending_nb_children -= 1;
	}
}
