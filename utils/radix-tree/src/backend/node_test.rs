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
	NodeBox, RadixConf, IndexFor, ValueIndexFor,
	PrefixKeyConf, Node, PrefixKey, AlignmentFor,
	ValueState};
use crate::children::NodeIndex;
use alloc::vec;
use alloc::vec::Vec;
use codec::{Encode, Decode, Input};
use crate::radix::{MaskFor, MaskKeyByte};
use core::fmt::Debug;

/// Test only backend with lazy access.
/// TODO Index as param
#[derive(Clone, Debug)]
pub struct NodeTestBackend<N: TreeConf>
	where
		IndexFor<N>: Clone + Debug,
		ValueIndexFor<N>: Clone + Debug,
{
	// None for new node only.
	index: Option<IndexFor<N>>,
	encoded: Vec<u8>,
	child_index: Vec<Option<IndexFor<N>>>,
	nb_children: usize,
	pending_nb_children: usize,
	value: Option<ValueIndexFor<N>>,
	value_state: ValueState,
	children_changed: bool,
	prefix_changed: bool,
	_ph: core::marker::PhantomData<N>,
}

// TODO replace backend by N::Backend when attached
fn decode<N>(
	encoded: Vec<u8>,
	index: IndexFor<N>,
) -> (NodeTestBackend<N>, PrefixKey<Vec<u8>, AlignmentFor<N>, N::LimitPartial>)
	where
		N: TreeConf,
		N::Value: Decode,
		IndexFor<N>: Clone + Debug + Decode,
		ValueIndexFor<N>: Clone + Debug + Decode,
{
	let input = &mut encoded.as_slice();
	let value: Option<ValueIndexFor<N>> = Decode::decode(input).unwrap();
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
	use crate::LimitPartial;
	let (prefix, is_limitted) = if let Some(limit) = N::LimitPartial::LIMIT_SIZE {
		let size = input.read_byte().unwrap();
		if size == u8::MAX {
			let mut prefix = vec![0; limit];
			input.read(&mut prefix[..]).unwrap();
			// TODO check if it can be ALWAYS filled from impl (especially split)
			// -> then just encode size and is split if size >= limit.
			let real_size: codec::Compact<u32> = Decode::decode(input).unwrap();
			(prefix, Some((limit, real_size.0 as usize)))
		} else {
			let mut prefix = vec![0; size as usize];
			input.read(&mut prefix[..]).unwrap();
			(prefix, None)
		}
	} else {
		let prefix: Vec<u8> = Decode::decode(input).unwrap();
		(prefix, None)
	};

	let prefix = if let Some((limit, real_size)) = is_limitted {
		PrefixKey::new_offset_limitted(prefix, start, limit, real_size)
	} else {
		let end_mask = if let Some(mask) = <N::Radix as RadixConf>::Alignment::DEFAULT {
			mask
		} else {
			let b = input.read_byte().unwrap();
			<N::Radix as RadixConf>::Alignment::decode_mask(b)
		};
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
		PrefixKey::new_offset(prefix, start, end)
	};

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
	ValueIndexFor<N>: Clone + Debug + Encode,
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
	use crate::LimitPartial;
	let with_end = if let Some(limit) = N::LimitPartial::LIMIT_SIZE {
		if let Some(real_size) = node.key.partial.partial_full_depth() {
			result.push(u8::MAX);
			node.key.data[..limit].encode_to(&mut result);
			codec::Compact(real_size as u32).encode_to(&mut result);
			false
		} else if node.key.data.len() > limit {
			result.push(u8::MAX);
			use codec::Output;
			result.write(&node.key.data[..limit]);
			let real_size = node.key.depth();
			codec::Compact(real_size as u32).encode_to(&mut result);
			false
		} else {
			result.push(node.key.data.len() as u8);
			use codec::Output;
			result.write(&node.key.data[..]);
			true
		}
	} else {
		node.key.data.encode_to(&mut result);
		true
	};
	if with_end {
		if let Some(_) = <N::Radix as RadixConf>::Alignment::DEFAULT {
		} else {
			result.push(<N::Radix as RadixConf>::Alignment::encode_mask(node.key.end));
		};
	}

	result
}


impl<N> NodeBackend<N> for NodeTestBackend<N>
	where
		N: TreeConf<NodeBackend = Self>,
		N::Value: Decode + Encode,
		IndexFor<N>: Clone + Debug + Decode + Encode,
		ValueIndexFor<N>: Clone + Debug + Decode + Encode,
{
	const FUSE_ON_COMMIT: bool = true;
	type ChildState = crate::Child<Node<N>>;
	//type ChildState = Box<Node<N>>;

	fn init_root(encoded: Vec<u8>, index: IndexFor<N>) -> Option<Node<N>> {
		let (backend, prefix) = decode::<N>(encoded, index);
		Some(Node::<N>::new_unfetched(
			prefix,
			backend,
		))
	}

	fn get_node(&self, index: IndexFor<N>, backend: &N::Backend) -> Option<Node<N>> {
		if let Some(encoded) = backend.get_node(index.clone()) {
			let (backend, prefix) = decode::<N>(encoded, index);
			Some(Node::<N>::new_unfetched(
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

	fn fetch_children(&self, at: KeyIndexFor<N>, backend: &N::Backend) -> Option<Node<N>> {
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

	fn fetch_value_index(&self) -> Option<ValueIndexFor<N>> {
		debug_assert!(self.value_state == ValueState::Unfetched);
		self.value.clone()
	}

	fn fetch_value(&self, backend: &N::Backend) -> Option<N::Value> {
		if let Some(ix) = self.value.clone() {
			let value = backend.get_value(ix).expect("Value stored with node.");
			Some(Decode::decode(&mut value.as_slice()).unwrap())
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

	fn commit_change_children_part(
		node: &mut NodeBox<N>,
		remove_node: &mut Vec<N::NodeBackend>,
		remove_value: &mut Vec<ValueIndexFor<N>>,
		backend: &N::Backend,
	) -> bool {
		let mut encoded_change = false;
		if node.backend.children_changed {
			// recurse commit of resolved children and update backend indexes.
			let mut index = None;
			use crate::children::Children;
			while let Some(i) = node.children.get_next_child_index(index) {
				let backend_ix = i.to_usize();
				use crate::WithChildState;
				match node.children.get_child_mut(i).map(|c| c.node_mut()) {
					Some(Some(mut child)) => {
						if let Some(ix) = Self::commit_change_fuse(&mut child, remove_node, remove_value, backend) {
							encoded_change = true;
							node.backend.child_index[backend_ix] = Some(ix);
						} else {
							// set old index (can be unset due to value not new but split
							// that makes it a new child.
							node.backend.child_index[backend_ix] = child.backend.index.clone();
							node.children.remove_child(i);
						}
					},
					Some(None) => {
						encoded_change = true;
						node.backend.child_index[backend_ix] = None;
					},
					None => (),
				}
				index = Some(i);
			}
		}
		encoded_change
	}

	fn commit_change_value_part(
		node: &mut NodeBox<N>,
		_remove_node: &mut Vec<N::NodeBackend>,
		_remove_value: &mut Vec<ValueIndexFor<N>>,
		backend: &N::Backend,
	) -> bool {
		let mut encoded_change = false;
		if node.backend.value_state == ValueState::Modified || node.backend.value_state == ValueState::ModifiedWithFullKey {
			if let Some(value) = node.value.as_ref() {
				let encoded = value.encode();
				let ix = if let Some(previous) = node.backend.value.clone() {
					backend.update_value(previous.clone(), encoded).unwrap_or(previous)
				} else {
					backend.set_new_value(encoded)
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
				// TODO not why added to remove value vec
				backend.remove_value(previous);
				encoded_change = true;
			} else {
				unreachable!("deleted should be some");
			};
			node.backend.value = None;
			node.backend.value_state = ValueState::Resolved;
		}

		encoded_change
	}
	
	fn commit_change(
		node: &mut NodeBox<N>,
		remove_node: &mut Vec<N::NodeBackend>,
		remove_value: &mut Vec<ValueIndexFor<N>>,
		backend: &N::Backend,
	) -> Option<IndexFor<N>> {
		let mut encoded_change = false;
		let changed = node.backend.children_changed
			|| node.backend.prefix_changed
			|| node.backend.value_state == ValueState::Modified
			|| node.backend.value_state == ValueState::ModifiedWithFullKey
			|| node.backend.value_state == ValueState::Deleted;
		if !changed {
			return None;
		}
		encoded_change |= Self::commit_change_children_part(node, remove_node, remove_value, backend);
		encoded_change |= Self::commit_change_value_part(node, remove_node, remove_value, backend);


		node.backend.children_changed = false;
		node.backend.nb_children = {
			node.backend.pending_nb_children
		};
		encoded_change = encoded_change || node.backend.prefix_changed;

		node.backend.prefix_changed = false;

		if encoded_change {
			let encoded = encode_node::<N>(node);
			// TODO additional change check??
			let result = if let Some(i) = node.backend.index.as_ref() {
				backend.update_node(i.clone(), encoded)
			} else {
				Some(backend.set_new_node(encoded))
			};
			result.as_ref().map(|index| {
				// TODO Api is bad, this update should not be implemented for each trait impl
				node.backend.index = Some(index.clone());
			});
			result
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

	fn clear_content(&mut self) -> Option<ValueIndexFor<N>> {
		self.encoded.clear();
		self.child_index = vec![None; <<N as TreeConf>::Radix as RadixConf>::CHILDREN_CAPACITY];
		let result = self.value.take();
		self.nb_children = 0;
		self.pending_nb_children = 0;
		self.children_changed = false;
		self.prefix_changed = true; // to ensure update
		self.value_state = ValueState::Resolved;
		result
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
