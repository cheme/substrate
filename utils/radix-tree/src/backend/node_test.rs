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


//! Node backend for testing.

use super::{Backend, NodeBackend};
use crate::{TreeConf, PositionFor, KeyIndexFor,
	NodeBox, RadixConf, IndexFor,
	PrefixKeyConf, Node, PrefixKey, AlignmentFor,
	ValueState};
use crate::children::NodeIndex;
use alloc::vec;
use codec::{Encode, Decode, Input};
use crate::radix::{MaskFor, MaskKeyByte};
use core::fmt::Debug;

/// Test only backend with lazy access.
/// TODO Index as param
#[derive(Clone, Debug)]
pub struct NodeTestBackend<N: TreeConf>
	where IndexFor<N>: Clone + Debug,
{
	// None for new node only.
	index: Option<IndexFor<N>>,
	encoded: Vec<u8>,
	child_index: Vec<Option<IndexFor<N>>>,
	nb_children: usize,
	pending_nb_children: usize,
	value: Option<IndexFor<N>>,
	value_state: ValueState,
	children_changed: bool,
	prefix_changed: bool,
	_ph: core::marker::PhantomData<N>,
}

// TODO replace backend by N::Backend when attached
fn decode<N>(
	encoded: Vec<u8>,
	index: IndexFor<N>,
) -> (NodeTestBackend<N>, PrefixKey<Vec<u8>, AlignmentFor<N>>)
	where
		N: TreeConf,
		N::Value: Decode,
		IndexFor<N>: Clone + Debug + Decode,
{
	let input = &mut encoded.as_slice();
	let value: Option<IndexFor<N>> = Decode::decode(input).unwrap();
	let mut child_index = Vec::new();
	let mut nb_children = 0;
	for _ in 0..<<N as TreeConf>::Radix as RadixConf>::CHILDREN_CAPACITY {
		let child: Option<IndexFor<N>> = Decode::decode(input).unwrap();
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
		children_changed: false,
		prefix_changed: false,
		_ph: Default::default(),
	}, prefix)
}

fn encode_node<N>(node: &Node<N>) -> Vec<u8>
where
	N: TreeConf<NodeBackend = NodeTestBackend<N>>,
	N::Value: Encode,
	IndexFor<N>: Clone + Debug + Encode,
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


impl<N> NodeBackend<N> for NodeTestBackend<N>
	where
		N: TreeConf<NodeBackend = Self>,
		N::Value: Decode + Encode,
		IndexFor<N>: Clone + Debug + Decode + Encode,
{
	const FUSE_ON_COMMIT: bool = true;
	type ChildState = crate::Child<Node<N>>;
	//type ChildState = Box<Node<N>>;

	fn init_root(encoded: Vec<u8>, index: IndexFor<N>) -> Option<NodeBox<N>> {
		let (backend, prefix) = decode::<N>(encoded, index);
		Some(Node::<N>::new_box_unfetched(
			prefix,
			backend,
		))
	}

	fn get_node(&self, index: IndexFor<N>, backend: &N::Backend) -> Option<NodeBox<N>> {
		if let Some(encoded) = backend.get_node(index.clone()) {
			let (backend, prefix) = decode::<N>(encoded, index);
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

	fn fetch_children_index(&self, at: KeyIndexFor<N>) -> Option<IndexFor<N>> {
		if let Some(Some(ix)) = self.child_index.get(at.to_usize()) {
			Some(ix.clone())
		} else {
			None
		}
	}

	fn fetch_children(&mut self, at: KeyIndexFor<N>, backend: &N::Backend) -> Option<Option<NodeBox<N>>> {
		if let Some(child) = self.child_index.get_mut(at.to_usize()) {
			if let Some(index) = child.clone() {
				return Some(self.get_node(index, backend));
			} else {
				return Some(None);
			}
		}
		None
	}

	fn fetch_children_no_cache(&self, at: KeyIndexFor<N>, backend: &N::Backend) -> Option<NodeBox<N>> {
		if let Some(child) = self.child_index.get(at.to_usize()) {
			if let Some(index) = child.clone() {
				return self.get_node(index, backend);
			}
		}
		None
	}

	fn fetch_nb_children(&mut self) -> Option<usize> {
		Some(self.nb_children)
	}

	fn fetch_value_index (&self) -> Option<IndexFor<N>> {
		debug_assert!(self.value_state == ValueState::Unfetched);
		self.value.clone()
	}

	fn fetch_value(&mut self, backend: &N::Backend) -> Option<Option<N::Value>> {
		if self.value_state != ValueState::Unfetched {
			None
		} else {
			self.value_state = ValueState::Resolved;
			if let Some(ix) = self.value.clone() {
				let value = backend.get_node(ix).expect("Value stored with node.");
				Some(Some(Decode::decode(&mut value.as_slice()).unwrap()))
			} else {
				Some(None)
			}
		}
	}
	fn fetch_value_no_cache(&self, backend: &N::Backend) -> Option<N::Value> {
		if let Some(ix) = self.value.clone() {
			backend.get_node(ix).map(|value| Decode::decode(&mut value.as_slice()).unwrap())
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

	fn delete(self, backend: &N::Backend) {
		if let Some(index) = self.index {
			backend.remove_node(index);
		}
	}

	fn new_node_backend() -> Self {
		NodeTestBackend {
			index: None,
			encoded: Vec::new(),
			child_index: vec![None; <<N as TreeConf>::Radix as RadixConf>::CHILDREN_CAPACITY],
			value: None,
			nb_children: 0,
			pending_nb_children: 0,
			children_changed: false,
			prefix_changed: true, // new node need update
			value_state: ValueState::Resolved,
			_ph: Default::default(),
		}
	}

	fn commit_change(
		node: &mut NodeBox<N>,
		remove_node: &mut Vec<N::NodeBackend>,
		backend: &N::Backend,
	) -> Option<IndexFor<N>> {
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
					if let Some(ix) = Self::commit_change_fuse(&mut child, remove_node, backend) {
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
					backend.update_node(previous.clone(), encoded).unwrap_or(previous)
				} else {
					backend.set_new_node(encoded)
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
				backend.remove_node(previous);
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
			let encoded = encode_node::<N>(node);
			// TODO additional change check??
			if let Some(i) = node.backend.index.as_ref() {
				backend.update_node(i.clone(), encoded)
			} else {
				Some(backend.set_new_node(encoded))
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
		self.pending_nb_children = 0;
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
