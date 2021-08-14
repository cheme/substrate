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


//! Encoded ART node, backend side.

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


/// Art Encoded nodes backend.
/// Index is fix u64, but could be parameterizable
/// to any fix length serializable type.
#[derive(Clone, Debug)]
pub struct ArtLocalBackend<N>
	where
		N: TreeConf,
		N::Backend: Backend<Index = u64>,
{
	// None for new node only.
	index: Option<IndexFor<N>>,
	// TODO could use smallvec by specializing backend.
	encoded: ArtEncoded<N>,
	// Number of children in backend before changes.
	original_nb_children: usize,
	// Number of children with pending changes.
	pending_nb_children: usize,
	// State for current value.
	value_state: ValueState,
	// True when any children is modified.
	children_changed: bool,
	// True when stored prefix or encoded node is modified.
	prefix_changed: bool,
}

impl<N> ArtLocalBackend<N>
	where
		N: TreeConf,
		N::Backend: Backend<Index = u64>,
		N::Value: AsRef<[u8]>,
{
	pub fn read(
		encoded: Vec<u8>,
		index: IndexFor<N>,
	) -> (ArtLocalBackend<N>, PrefixKey<Vec<u8>, AlignmentFor<N>>) {
		let (encoded, prefix) = ArtEncoded::read(encoded, index);
		let original_nb_children = encoded.nb_children();

		(ArtLocalBackend {
			index: Some(index),
			encoded,
			value_state: ValueState::Unfetched,
			pending_nb_children: original_nb_children,
			original_nb_children,
			children_changed: false,
			prefix_changed: false,
		}, prefix)
	}

	pub fn with_capacity(
		capacity: usize,
	) -> ArtLocalBackend<N> {
		let encoded = match capacity {
			c if c < 5 => ArtEncoded::Art4(Art4::empty(), core::marker::PhantomData),
			c if c < 17 => ArtEncoded::Art16(Art16::empty()),
			c if c < 49 => ArtEncoded::Art48(Art48::empty()),
			c if c < 257 => ArtEncoded::Art256(Art256::empty()),
			_ => panic!("Invalid capacity for node"),
		};
		ArtLocalBackend {
			index: None,
			encoded,
			original_nb_children: 0,
			pending_nb_children: 0,
			children_changed: false,
			prefix_changed: true, // new node need update
			value_state: ValueState::Resolved,
		}
	}
}

#[derive(Clone, Debug)]
enum ArtEncoded<N> {
	Art4(Art4, core::marker::PhantomData<N>),
	Art16(Art16),
	Art48(Art48),
	Art256(Art256),
}

#[derive(Clone, Debug)]
struct Art4(Vec<u8>);
#[derive(Clone, Debug)]
struct Art16(Vec<u8>);
#[derive(Clone, Debug)]
struct Art48(Vec<u8>);
#[derive(Clone, Debug)]
struct Art256(Vec<u8>);

impl<N> ArtEncoded<N>
	where
		N: TreeConf,
		N::Backend: Backend<Index = u64>,
		N::Value: AsRef<[u8]>,
{
	pub fn read(
		encoded: Vec<u8>,
		index: IndexFor<N>,
	) -> (ArtEncoded<N>, PrefixKey<Vec<u8>, AlignmentFor<N>>) {
		// TODO could add a meta in backend to save this byte.
		match encoded[0] {
			0 => { 
				let (art, offset) = Art4::read(encoded);
				let prefix = Self::read_prefix(&art.as_ref()[offset..], index);
				(ArtEncoded::Art4(art, core::marker::PhantomData), prefix)
			},
			1 => { 
				let (art, offset) = Art16::read(encoded);
				let prefix = Self::read_prefix(&art.as_ref()[offset..], index);
				(ArtEncoded::Art16(art), prefix)
			},
			2 => { 
				let (art, offset) = Art48::read(encoded);
				let prefix = Self::read_prefix(&art.as_ref()[offset..], index);
				(ArtEncoded::Art48(art), prefix)
			},
			3 => { 
				let (art, offset) = Art256::read(encoded);
				let prefix = Self::read_prefix(&art.as_ref()[offset..], index);
				(ArtEncoded::Art256(art), prefix)
			},
			_ => panic!("Invalid art node type"),
		}
	}

	// TODO share code with node_test
	pub fn read_prefix(mut encoded: &[u8], index: IndexFor<N>) -> PrefixKey<Vec<u8>, AlignmentFor<N>> {
		let start_mask = if let Some(mask) = <N::Radix as RadixConf>::Alignment::DEFAULT {
			mask
		} else {
			let b = encoded[0];
			encoded = &encoded[1..];
			<N::Radix as RadixConf>::Alignment::decode_mask(b)
		};
		let start = PositionFor::<N> {
			index: 0,
			mask: start_mask,
		};
		let end_mask = if let Some(mask) = <N::Radix as RadixConf>::Alignment::DEFAULT {
			mask
		} else {
			let b = encoded[0];
			encoded = &encoded[1..];
			<N::Radix as RadixConf>::Alignment::decode_mask(b)
		};
		let (size, r) = varint_decode(encoded);
		encoded = &encoded[r..];

		debug_assert!(size as usize == encoded.len()); // TODO assert?
		let prefix: Vec<u8> = encoded[..size as usize].to_vec();
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
	}
	fn nb_children(&self) -> usize {
		match self {
			ArtEncoded::Art4(art, _) => art.nb_children(),
			ArtEncoded::Art16(art) => art.nb_children(),
			ArtEncoded::Art48(art) => art.nb_children(),
			ArtEncoded::Art256(art) => art.nb_children(),
		}
	}

	fn get_next_child_index(&self, index: Option<usize>) -> Option<usize> {
		match self {
			ArtEncoded::Art4(art, _) => art.get_next_child_index(index),
			ArtEncoded::Art16(art) => art.get_next_child_index(index),
			ArtEncoded::Art48(art) => art.get_next_child_index(index),
			ArtEncoded::Art256(art) => art.get_next_child_index(index),
		}
	}

	fn fetch_value_index (&self) -> Option<IndexFor<N>> {
		match self {
			ArtEncoded::Art4(art, _) => art.fetch_value_index(),
			ArtEncoded::Art16(art) => art.fetch_value_index(),
			ArtEncoded::Art48(art) => art.fetch_value_index(),
			ArtEncoded::Art256(art) => art.fetch_value_index(),
		}
	}

	fn fetch_children_index (&self, at: KeyIndexFor<N>) -> Option<IndexFor<N>> {
		let at = at.to_usize();
		match self {
			ArtEncoded::Art4(art, _) => art.fetch_children_index(at),
			ArtEncoded::Art16(art) => art.fetch_children_index(at),
			ArtEncoded::Art48(art) => art.fetch_children_index(at),
			ArtEncoded::Art256(art) => art.fetch_children_index(at),
		}
	}
}

impl Art4 {
	fn empty() -> Self {
		unimplemented!()
	}

	fn read(encoded: Vec<u8>) -> (Art4, usize) {
		unimplemented!()
	}

	fn nb_children(&self) -> usize {
		unimplemented!()
	}

	fn get_next_child_index(&self, index: Option<usize>) -> Option<usize> {
		unimplemented!()
	}

	fn fetch_value_index (&self) -> Option<u64> {
		unimplemented!()
	}

	fn fetch_children_index (&self, at: usize) -> Option<u64> {
		unimplemented!()
	}

	fn as_ref(&self) -> &[u8] {
		&self.0[..]
	}
}

impl Art16 {
	fn empty() -> Self {
		unimplemented!()
	}

	fn read(encoded: Vec<u8>) -> (Art16, usize) {
		unimplemented!()
	}

	fn nb_children(&self) -> usize {
		unimplemented!()
	}

	fn get_next_child_index(&self, index: Option<usize>) -> Option<usize> {
		unimplemented!()
	}

	fn fetch_value_index (&self) -> Option<u64> {
		unimplemented!()
	}

	fn fetch_children_index (&self, at: usize) -> Option<u64> {
		unimplemented!()
	}

	fn as_ref(&self) -> &[u8] {
		&self.0[..]
	}
}

impl Art48 {
	fn empty() -> Self {
		unimplemented!()
	}

	fn read(encoded: Vec<u8>) -> (Art48, usize) {
		unimplemented!()
	}

	fn nb_children(&self) -> usize {
		unimplemented!()
	}

	fn get_next_child_index(&self, index: Option<usize>) -> Option<usize> {
		unimplemented!()
	}

	fn fetch_value_index (&self) -> Option<u64> {
		unimplemented!()
	}

	fn fetch_children_index (&self, at: usize) -> Option<u64> {
		unimplemented!()
	}

	fn as_ref(&self) -> &[u8] {
		&self.0[..]
	}
}

impl Art256 {
	fn empty() -> Self {
		unimplemented!()
	}

	fn read(encoded: Vec<u8>) -> (Art256, usize) {
		unimplemented!()
	}

	fn nb_children(&self) -> usize {
		unimplemented!()
	}

	fn get_next_child_index(&self, index: Option<usize>) -> Option<usize> {
		unimplemented!()
	}

	fn fetch_value_index (&self) -> Option<u64> {
		unimplemented!()
	}

	fn fetch_children_index (&self, at: usize) -> Option<u64> {
		unimplemented!()
	}

	fn as_ref(&self) -> &[u8] {
		&self.0[..]
	}
}

impl<N> NodeBackend<N> for ArtLocalBackend<N>
	where
		N: TreeConf<NodeBackend = Self, Value = Vec<u8>>,
		N::Backend: Backend<Index = u64>,
//		N::Value: AsRef<[u8]>,
{
	const FUSE_ON_COMMIT: bool = true;

	type ChildState = crate::Child<Node<N>>;

	fn init_root(encoded: Vec<u8>, index: IndexFor<N>) -> Option<NodeBox<N>> {
		let (backend, prefix) = ArtLocalBackend::<N>::read(encoded, index);
		Some(Node::<N>::new_box_unfetched(
			prefix,
			backend,
		))
	}

	fn get_node(&self, index: IndexFor<N>, backend: &N::Backend) -> Option<NodeBox<N>> {
		if let Some(encoded) = backend.get_node(index.clone()) {
			let (backend, prefix) = ArtLocalBackend::<N>::read(encoded, index);
			Some(Node::<N>::new_box_unfetched(
				prefix,
				backend,
			))
		} else {
			None
		}
	}

	fn get_next_child_index(&self, index: Option<KeyIndexFor<N>>) -> Option<KeyIndexFor<N>> {
		if let Some(i) = self.encoded.get_next_child_index(index.map(|i| i.to_usize())) {
			return Some(KeyIndexFor::<N>::from_usize(i));
		}
		None
	}

	fn fetch_children_index(&self, at: KeyIndexFor<N>) -> Option<IndexFor<N>> {
		if let Some(i) = self.encoded.fetch_children_index(at) {
			return Some(i);
		}
		None
	}

	fn fetch_children(&self, at: KeyIndexFor<N>, backend: &N::Backend) -> Option<NodeBox<N>> {
		if let Some(index) = self.fetch_children_index(at) {
			return self.get_node(index, backend);
		}
		None
	}

	fn fetch_nb_children(&mut self) -> Option<usize> {
		Some(self.original_nb_children)
	}

	fn fetch_value_index (&self) -> Option<IndexFor<N>> {
		debug_assert!(self.value_state == ValueState::Unfetched);
		self.encoded.fetch_value_index()
	}

	fn fetch_value(&self, backend: &N::Backend) -> Option<N::Value> {
		if let Some(ix) = self.encoded.fetch_value_index() {
			backend.get_node(ix)
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
		ArtLocalBackend::with_capacity(0)
	}

	fn commit_change(
		node: &mut NodeBox<N>,
		remove_node: &mut Vec<N::NodeBackend>,
		backend: &N::Backend,
	) -> Option<IndexFor<N>> {
		unimplemented!()
/*		let mut encoded_change = false;
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
*/
	}

	fn value_state(&self) -> ValueState {
		self.value_state
	}

	fn set_value_state(&mut self, state: ValueState) {
		self.value_state = state
	}

	fn clear_content(&mut self) {
		let index = self.index;
		*self = ArtLocalBackend::with_capacity(self.original_nb_children);
		self.children_changed = true;
		self.prefix_changed = true;
		self.index = index; // got index relative to a capacity.
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

fn varint_encode(mut size: u64, buf: &mut [u8; 10]) -> &[u8] {
	let nb = varint_encoded_len(size);
	for i in (0..nb).rev() {
		buf[i] = (size as u8) | 0b1000_0000;
		size >>= 7;
		buf[i] |= 0b1000_0000;
	}
	buf[nb - 1] &= 0b0111_1111;
	&buf[..nb]
}

fn varint_decode(buf: &[u8]) -> (u64, usize) {
	let mut result = 0u64;
	// out of bound error on > u64
	for i in 0..buf.len() {
		if buf[i] & 0b1000_0000 > 0 {
			result <<= 7;
			result |= (buf[i] & 0b0111_1111) as u64;
		} else {
			result <<= 7;
			result |= buf[i] as u64;
			return (result, i + 1);
		}
	}
	panic!("Out of range varint");
}

fn varint_encoded_len(size: u64) -> usize {
	let nb_bit = std::cmp::max(64 - size.leading_zeros(), 1);
	(nb_bit as usize + 6) / 7
}
