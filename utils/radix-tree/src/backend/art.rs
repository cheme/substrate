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
	NodeBox, RadixConf, IndexFor, ValueIndexFor,
	PrefixKeyConf, Node, PrefixKey, AlignmentFor,
	ValueState};
use crate::children::NodeIndex;
use alloc::vec;
use alloc::vec::Vec;
use crate::radix::{MaskFor, MaskKeyByte};
use core::fmt::Debug;
use core::marker::PhantomData;

/// Fix size index that can be read or written for these art.
pub trait Index: Clone + Debug + Eq {
	const SIZE: usize;
	// TODO with current code UNDEFINED must be 0 encoded (no specific initializing).
	// can be ok, just need proper doc
	const UNDEFINED: Self;

	fn read_index(enc: &[u8]) -> Self;
	fn write_index(&self, dest: &mut [u8]);
}

impl Index for u64 {
	const SIZE: usize = 8;
	const UNDEFINED: Self = 0;

	fn read_index(enc: &[u8]) -> Self {
		read_u64(enc)
	}
	fn write_index(&self, dest: &mut [u8]) {
		write_u64(*self, dest)
	}
}

impl Index for u32 {
	const SIZE: usize = 4;
	const UNDEFINED: Self = 0;

	fn read_index(enc: &[u8]) -> Self {
		read_u32(enc)
	}
	fn write_index(&self, dest: &mut [u8]) {
		write_u32(*self, dest)
	}
}

#[cfg(test)]
impl Index for u128 {
	const SIZE: usize = 16;
	const UNDEFINED: Self = 0;

	fn read_index(enc: &[u8]) -> Self {
		read_u128(enc)
	}
	fn write_index(&self, dest: &mut [u8]) {
		write_u128(*self, dest)
	}
}


/// Art Encoded nodes backend.
/// Index is fix u64, but could be parameterizable
/// to any fix length serializable type.
#[derive(Clone, Debug)]
pub struct ArtLocalBackend<N>
	where
		N: TreeConf,
		IndexFor<N>: Index,
		ValueIndexFor<N>: Index,
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
		N::Value: AsRef<[u8]>,
		IndexFor<N>: Index,
		ValueIndexFor<N>: Index,
{
	pub fn read(
		encoded: Vec<u8>,
		index: IndexFor<N>,
	) -> (ArtLocalBackend<N>, PrefixKey<Vec<u8>, AlignmentFor<N>>) {
		let (encoded, prefix) = ArtEncoded::read(encoded);
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
			c if c < 5 => ArtEncoded::Art4(Art4::empty(), PhantomData),
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

	fn need_resize(&self) -> Option<Self> {
		match self.encoded {
			ArtEncoded::Art4(..) => self.pending_nb_children > 4,
			ArtEncoded::Art16(..) => self.pending_nb_children > 16
					|| self.pending_nb_children < 5,
			ArtEncoded::Art48(..) => self.pending_nb_children > 48
					|| self.pending_nb_children < 17,
			ArtEncoded::Art256(..) => self.pending_nb_children < 49,
		}.then(|| Self::with_capacity(self.pending_nb_children))
	}
}

#[derive(Clone, Debug)]
enum ArtEncoded<N>
	where
		N: TreeConf,
		IndexFor<N>: Index,
		ValueIndexFor<N>: Index,
{
	Art4(Art4<IndexFor<N>, ValueIndexFor<N>>, PhantomData<N>),
	Art16(Art16<IndexFor<N>, ValueIndexFor<N>>),
	Art48(Art48<IndexFor<N>, ValueIndexFor<N>>),
	Art256(Art256<IndexFor<N>, ValueIndexFor<N>>),
}

#[derive(Clone, Debug)]
struct Art4<I, VI>(Vec<u8>, PhantomData<(I, VI)>);
#[derive(Clone, Debug)]
struct Art16<I, VI>(Vec<u8>, PhantomData<(I, VI)>);
#[derive(Clone, Debug)]
struct Art48<I, VI>(Vec<u8>, PhantomData<(I, VI)>);
#[derive(Clone, Debug)]
struct Art256<I, VI>(Vec<u8>, PhantomData<(I, VI)>);

impl<N> ArtEncoded<N>
	where
		N: TreeConf,
		IndexFor<N>: Index,
		ValueIndexFor<N>: Index,
		N::Value: AsRef<[u8]>,
{
	pub fn read(
		encoded: Vec<u8>,
	) -> (ArtEncoded<N>, PrefixKey<Vec<u8>, AlignmentFor<N>>) {
		// First byte contain size and type, only 256 uses a second byte
		// for size with 0 being 0 or 256 when encoded do not contains
		// a 0 size.
		match (encoded[0] & 0b1100_0000) >> 6 {
			0 => {
				let (art, offset) = Art4::read(encoded);
				let prefix = Self::read_prefix(&art.as_ref()[offset..]);
				(ArtEncoded::Art4(art, PhantomData), prefix)
			},
			1 => { 
				let (art, offset) = Art16::read(encoded);
				let prefix = Self::read_prefix(&art.as_ref()[offset..]);
				(ArtEncoded::Art16(art), prefix)
			},
			2 => { 
				let (art, offset) = Art48::read(encoded);
				let prefix = Self::read_prefix(&art.as_ref()[offset..]);
				(ArtEncoded::Art48(art), prefix)
			},
			3 => {
				let (art, offset) = Art256::read(encoded);
				let prefix = Self::read_prefix(&art.as_ref()[offset..]);
				(ArtEncoded::Art256(art), prefix)
			},
			_ => unreachable!(),
		}
	}

	// TODO share code with node_test
	pub fn read_prefix(mut encoded: &[u8]) -> PrefixKey<Vec<u8>, AlignmentFor<N>> {
		if encoded.len() == 0 {
			return PrefixKey {
				data: Default::default(),
				start: MaskFor::<N::Radix>::FIRST,
				end: MaskFor::<N::Radix>::FIRST,
			}
		}
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

	// TODO use in node_test
	pub fn write_prefix(key: &PrefixKey<Vec<u8>, AlignmentFor<N>>) -> Vec<u8> {
		let mut result = Vec::with_capacity(key.data.len() + 1 + if <N::Radix as RadixConf>::Alignment::DEFAULT.is_some() {
			0
		} else {
			2
		});
		let offset = if <N::Radix as RadixConf>::Alignment::DEFAULT.is_some() {
			0
		} else {
			result.push(<N::Radix as RadixConf>::Alignment::encode_mask(key.start));
			result.len()
		};
		let nb = varint_encoded_len(key.data.len() as u64);
		result.resize(offset + nb, 0);
		varint_encode(key.data.len() as u64, &mut result[offset..], nb);
		result.extend_from_slice(key.data.as_slice());
		if <N::Radix as RadixConf>::Alignment::DEFAULT.is_some() {
		} else {
			result.push(<N::Radix as RadixConf>::Alignment::encode_mask(key.end));
		};
		result
	}

	fn nb_children(&self) -> usize {
		match self {
			ArtEncoded::Art4(art, _) => art.nb_children(),
			ArtEncoded::Art16(art) => art.nb_children(),
			ArtEncoded::Art48(art) => art.nb_children(),
			ArtEncoded::Art256(art) => art.nb_children(),
		}
	}

	// TODO variant with internal usize index for iteration. (here to much
	// lookup)
	fn get_next_child_index(&self, index: Option<usize>) -> Option<usize> {
		match self {
			ArtEncoded::Art4(art, _) => art.get_next_child_index(index),
			ArtEncoded::Art16(art) => art.get_next_child_index(index),
			ArtEncoded::Art48(art) => art.get_next_child_index(index),
			ArtEncoded::Art256(art) => art.get_next_child_index(index),
		}
	}

	fn fetch_value_index(&self) -> Option<ValueIndexFor<N>> {
		match self {
			ArtEncoded::Art4(art, _) => art.fetch_value_index(),
			ArtEncoded::Art16(art) => art.fetch_value_index(),
			ArtEncoded::Art48(art) => art.fetch_value_index(),
			ArtEncoded::Art256(art) => art.fetch_value_index(),
		}
	}

	fn fetch_children_index(&self, at: KeyIndexFor<N>) -> Option<IndexFor<N>> {
		let at = at.to_usize();
		match self {
			ArtEncoded::Art4(art, _) => art.fetch_children_index(at),
			ArtEncoded::Art16(art) => art.fetch_children_index(at),
			ArtEncoded::Art48(art) => art.fetch_children_index(at),
			ArtEncoded::Art256(art) => art.fetch_children_index(at),
		}
	}

	fn set_value_index(&mut self, index: &ValueIndexFor<N>) {
		match self {
			ArtEncoded::Art4(art, _) => art.set_value_index(index),
			ArtEncoded::Art16(art) => art.set_value_index(index),
			ArtEncoded::Art48(art) => art.set_value_index(index),
			ArtEncoded::Art256(art) => art.set_value_index(index),
		}
	}

	fn set_children_index(&mut self, at: KeyIndexFor<N>, index: &IndexFor<N>) -> bool {
		let at = at.to_usize();
		match self {
			ArtEncoded::Art4(art, _) => art.set_children_index(at, index),
			ArtEncoded::Art16(art) => art.set_children_index(at, index),
			ArtEncoded::Art48(art) => art.set_children_index(at, index),
			ArtEncoded::Art256(art) => art.set_children_index(at, index),
		}
	}

	fn remove_children_index(&mut self, at: KeyIndexFor<N>) {
		let at = at.to_usize();
		match self {
			ArtEncoded::Art4(art, _) => art.remove_children_index(at),
			ArtEncoded::Art16(art) => art.remove_children_index(at),
			ArtEncoded::Art48(art) => art.remove_children_index(at),
			ArtEncoded::Art256(art) => art.remove_children_index(at),
		}
	}

	fn as_ref(&self) -> &[u8] {
		match self {
			ArtEncoded::Art4(art, _) => art.as_ref(),
			ArtEncoded::Art16(art) => art.as_ref(),
			ArtEncoded::Art48(art) => art.as_ref(),
			ArtEncoded::Art256(art) => art.as_ref(),
		}
	}

	fn set_prefix(&mut self, prefix: &PrefixKey<Vec<u8>, AlignmentFor<N>>) {
		let encoded_prefix = Self::write_prefix(prefix);
		match self {
			ArtEncoded::Art4(art, _) => art.set_prefix(encoded_prefix),
			ArtEncoded::Art16(art) => art.set_prefix(encoded_prefix),
			ArtEncoded::Art48(art) => art.set_prefix(encoded_prefix),
			ArtEncoded::Art256(art) => art.set_prefix(encoded_prefix),
		}
	}
}

macro_rules! art_impl_1 {
	(
		$art: ident,
		$art_nb: ident,
		$art_fix_len: ident,
		$art_test: ident
	) => {

impl<I: Index, VI: Index> $art<I, VI> {
	fn read_child_index(&self, at: usize) -> I {
		let i = $art_nb - at;
		let i = $art_fix_len(I::SIZE, VI::SIZE) - i * I::SIZE - VI::SIZE;
		I::read_index(&self.0[i .. i + I::SIZE])
	}

	fn read(encoded: Vec<u8>) -> ($art<I, VI>, usize) {
		debug_assert!(encoded.len() >= $art_fix_len(I::SIZE, VI::SIZE));
		($art(encoded, PhantomData), $art_fix_len(I::SIZE, VI::SIZE))
	}

	fn fetch_value_index(&self) -> Option<VI> {
		let index = VI::read_index(&self.0[$art_fix_len(I::SIZE, VI::SIZE) - VI::SIZE .. $art_fix_len(I::SIZE, VI::SIZE)]);
		(index != VI::UNDEFINED).then(|| index)
	}

	fn set_value_index(&mut self, index: &VI) {
		let i = $art_fix_len(I::SIZE, VI::SIZE) - VI::SIZE;
		index.write_index(&mut self.0[i .. i + VI::SIZE]);
	}

	fn as_ref(&self) -> &[u8] {
		&self.0[..]
	}

	fn set_prefix(&mut self, mut prefix: Vec<u8>) {
		self.0.truncate($art_fix_len(I::SIZE, VI::SIZE));
		self.0.append(&mut prefix);
	}
}


#[test]
fn $art_test() {
	macro_rules! inner {
		($i: ty, $vi: ty) => {
		let mut art = $art::<$i, $vi>::empty();
		assert_eq!(art.nb_children(), 0);
		assert_eq!(art.fetch_children_index(0), None);
		assert_eq!(art.fetch_children_index(255), None);
		assert!(art.fetch_value_index().is_none());
		assert!(art.set_children_index(5, &44));
		assert!(art.set_children_index(0, &48));
		assert!(art.set_children_index(8, &50));
		assert_eq!(art.nb_children(), 3);
		art.set_value_index(&83747);
		assert_eq!(art.fetch_children_index(0), Some(48));
		assert_eq!(art.fetch_children_index(1), None);
		assert_eq!(art.fetch_children_index(255), None);
		assert_eq!(art.fetch_children_index(5), Some(44));
		assert_eq!(art.fetch_children_index(8), Some(50));
		assert_eq!(art.fetch_value_index(), Some(83747));
		assert!(art.set_children_index(1, &15));
		assert!(art.set_children_index(8, &30));
		if $art_nb == 4 {
			assert!(art.set_children_index(7, &30) == false);
		}
		assert_eq!(art.fetch_children_index(0), Some(48));
		assert_eq!(art.fetch_children_index(1), Some(15));
		assert_eq!(art.fetch_children_index(8), Some(30));
		assert_eq!(art.fetch_value_index(), Some(83747));
		let mut art = $art::<$i, $vi>::empty();
		assert!(art.set_children_index(255, &2450));
		assert_eq!(art.fetch_children_index(255), Some(2450));
		}
	}
	inner!(u128, u128);
	inner!(u128, u32);
	inner!(u32, u128);
	inner!(u64, u64);
	inner!(u64, u32);
	inner!(u32, u64);
}

}}

macro_rules! art_impl_2 {
	(
		$art: ident,
		$art_nb: ident,
		$art_fix_len: ident,
		$art_test: ident
	) => {

impl<I: Index, VI: Index> $art<I, VI> {
	fn nb_children(&self) -> usize {
		(self.0[0] & 0b0011_1111) as usize
	}

	fn get_next_child_index(&self, index: Option<usize>) -> Option<usize> {
		for i in 0 .. $art_nb {
			let v = self.0[i + 1] as usize;
			if index.is_none() || Some(v) > index {
				if v == 0 && self.read_child_index(i) == I::UNDEFINED {
					return None;
				}
				return Some(v);
			}
		}
		None
	}

	fn fetch_children_index(&self, at: usize) -> Option<I> {
		for i in 0 .. $art_nb {
			let v = self.0[i + 1] as usize;
			if v == at {
				let index = self.read_child_index(i);
				if index == I::UNDEFINED {
					return None;
				}
				return Some(index);
			} else if v > at {
				break;
			}
		}
		None
	}

	fn set_children_index(&mut self, at: usize, index: &I) -> bool {
		let mut four_ix = $art_nb;
		for i in 0 .. $art_nb {
			let ix = self.0[i + 1] as usize;
			if ix == 0 {
				let index = self.read_child_index(i);
				if index == I::UNDEFINED {
					// empty, set here.
					four_ix = i;
					self.0[0] += 1;
					break;
				}
			}
			if at == ix {
				four_ix = i;
				break;
			} else if ix > at {
				if self.nb_children() > $art_nb - 1 {
					return false;
				}
				self.0[0] += 1;
				four_ix = i;
				self.0.copy_within(i + 1 .. $art_nb, i + 2);
				let i = $art_nb - four_ix;
				let i = $art_fix_len(I::SIZE, VI::SIZE) - i * I::SIZE - VI::SIZE;
				let ie = $art_fix_len(I::SIZE, VI::SIZE) - I::SIZE - VI::SIZE;
				self.0.copy_within(i .. ie, i + I::SIZE);
				break;
			}
		}
		if four_ix != $art_nb {
			self.0[four_ix + 1] = at as u8;
			let i = $art_nb - four_ix;
			let i = $art_fix_len(I::SIZE, VI::SIZE) - i * I::SIZE - VI::SIZE;
			index.write_index(&mut self.0[i .. i + I::SIZE]);
			true
		} else {
			false
		}
	}

	fn remove_children_index(&mut self, at: usize) {
		for i in 0 .. $art_nb {
			let ix = self.0[i + 1] as usize;
			if at == ix {
				if at == 0 && self.read_child_index(i) == I::UNDEFINED {
					break;
				}
				self.0[0] -= 1;
				self.0.copy_within(i + 2 .. $art_nb + 1, i + 1);
				self.0[$art_nb] = 0;
				let i = $art_nb - i - 1;
				let i =	$art_fix_len(I::SIZE, VI::SIZE) - i * I::SIZE - VI::SIZE;
				let ie = $art_fix_len(I::SIZE, VI::SIZE) - VI::SIZE;
				self.0.copy_within(i .. ie, i - I::SIZE);
				I::UNDEFINED.write_index(&mut self.0[i - I::SIZE .. i]);
				break;
			} else if ix < at {
				break;
			}
		}
	}
}

}}


const ART4_NB: usize = 4;
const ART16_NB: usize = 16;
const ART48_NB: usize = 48;
const ART256_NB: usize = 256;

const fn art4_fix_len(index_size: usize, value_index_size: usize) -> usize {
	1 + 4 + 4 * index_size + value_index_size
}

const fn art16_fix_len(index_size: usize, value_index_size: usize) -> usize {
	1 + 16 + 16 * index_size + value_index_size
}

const fn art48_fix_len(index_size: usize, value_index_size: usize) -> usize {
	1 + 256 + 48 * index_size + value_index_size
}

const fn art256_fix_len(index_size: usize, value_index_size: usize) -> usize {
	1 + 1 + 256 * index_size + value_index_size
}

art_impl_1!(Art4, ART4_NB, art4_fix_len, test_art4);
art_impl_1!(Art16, ART16_NB, art16_fix_len, test_art16);
art_impl_1!(Art48, ART48_NB, art48_fix_len, test_art48);
art_impl_1!(Art256, ART256_NB, art256_fix_len, test_art256);
art_impl_2!(Art4, ART4_NB, art4_fix_len, test_art4);
art_impl_2!(Art16, ART16_NB, art16_fix_len, test_art16);

impl<I: Index, VI: Index> Art4<I, VI> {
	fn empty() -> Self {
		Art4(vec![0; art4_fix_len(I::SIZE, VI::SIZE)], PhantomData)
	}
}

impl<I: Index, VI: Index> Art16<I, VI> {
	fn empty() -> Self {
		let mut result = Art16(vec![0; art16_fix_len(I::SIZE, VI::SIZE)], PhantomData);
		result.0[0] |= 1 << 6;
		result
	}
}

impl<I: Index, VI: Index> Art48<I, VI> {
	fn empty() -> Self {
		let mut result = Art48(vec![0; art48_fix_len(I::SIZE, VI::SIZE)], PhantomData);
		result.0[0] |= 2 << 6;
		result
	}

	fn nb_children(&self) -> usize {
		(self.0[0] & 0b0011_1111) as usize
	}

	fn get_next_child_index(&self, index: Option<usize>) -> Option<usize> {
		for i in index.map(|i| i + 1).unwrap_or(0) .. 256 {
			let v = self.0[i + 1] as usize;
			if v != 0 {
				return Some(i);
			}
		}
		None
	}

	fn fetch_children_index (&self, at: usize) -> Option<I> {
		let at = self.0[at + 1];
		(at != 0).then(|| self.read_child_index(at as usize - 1))
	}

	fn set_children_index(&mut self, at: usize, index: &I) -> bool {
		if self.nb_children() == 48 {
			return false;
		}
		self.0[at + 1] = self.nb_children() as u8 + 1;
		let i = ART48_NB - self.nb_children();
		let i = art48_fix_len(I::SIZE, VI::SIZE) - i * I::SIZE - VI::SIZE;
		if I::read_index(&self.0[i .. i + I::SIZE]) == I::UNDEFINED {
			self.0[0] += 1;
		}
		index.write_index(&mut self.0[i .. i + I::SIZE]);
		true
	}

	fn remove_children_index(&mut self, at: usize) {
		self.0[at] = 0;
		let i = ART48_NB - at;
		let i = art48_fix_len(I::SIZE, VI::SIZE) - i * I::SIZE - VI::SIZE;
		if I::read_index(&self.0[i .. i + I::SIZE]) != I::UNDEFINED {
			self.0[0] -= 1;
		}
		I::UNDEFINED.write_index(&mut self.0[i .. i + I::SIZE]);
	}
}

impl<I: Index, VI: Index> Art256<I, VI> {
	fn empty() -> Self {
		let mut result = Art256(vec![0; art256_fix_len(I::SIZE, VI::SIZE)], PhantomData);
		result.0[0] |= 3 << 6;
		result
	}

	fn nb_children(&self) -> usize {
		if (self.0[0] & 0b0000_0001) > 0 {
			256
		} else {
			self.0[1] as usize
		}
	}

	fn get_next_child_index(&self, index: Option<usize>) -> Option<usize> {
		for i in index.map(|i| i + 1).unwrap_or(0) .. art256_fix_len(I::SIZE, VI::SIZE) {
			let v = self.read_child_index(i);
			if v != I::UNDEFINED {
				return Some(i);
			}
		}
		None
	}

	fn fetch_children_index(&self, at: usize) -> Option<I> {
		let v = self.read_child_index(at);
		(v != I::UNDEFINED).then(|| v)
	}

	fn set_children_index(&mut self, at: usize, index: &I) -> bool {
		let i = ART256_NB - at;
		let i = art256_fix_len(I::SIZE, VI::SIZE) - i * I::SIZE - VI::SIZE;
		if I::read_index(&self.0[i .. i + I::SIZE]) == I::UNDEFINED {
			if self.0[1] == 255 {
				self.0[0] |= 0b0000_0001;
			} else {
				self.0[1] += 1;
			}
		}
		index.write_index(&mut self.0[i .. i + I::SIZE]);
		true
	}

	fn remove_children_index(&mut self, at: usize) {
		let i = ART256_NB - at;
		let i = art256_fix_len(I::SIZE, VI::SIZE) - i * I::SIZE - VI::SIZE;
		if I::read_index(&self.0[i .. i + I::SIZE]) != I::UNDEFINED {
			if self.0[0] & 0b0000_0001 > 0 {
				self.0[0] |= !0b0000_0001;
			} else {
				self.0[1] -= 1;
			}
		}
		I::UNDEFINED.write_index(&mut self.0[i .. i + I::SIZE]);
	}
}

impl<N> NodeBackend<N> for ArtLocalBackend<N>
	where
		N: TreeConf<NodeBackend = Self, Value = Vec<u8>>,
		N::Backend: Backend,
		IndexFor<N>: Index,
		ValueIndexFor<N>: Index,
{
	const FUSE_ON_COMMIT: bool = true;

	type ChildState = crate::Child<Node<N>>;

	fn init_root(encoded: Vec<u8>, index: IndexFor<N>) -> Option<Node<N>> {
		let (backend, prefix) = ArtLocalBackend::<N>::read(encoded, index);
		Some(Node::<N>::new_unfetched(
			prefix,
			backend,
		))
	}

	fn get_node(&self, index: IndexFor<N>, backend: &N::Backend) -> Option<Node<N>> {
		if let Some(encoded) = backend.get_node(index.clone()) {
			let (backend, prefix) = ArtLocalBackend::<N>::read(encoded, index);
			Some(Node::<N>::new_unfetched(
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

	fn fetch_children(&self, at: KeyIndexFor<N>, backend: &N::Backend) -> Option<Node<N>> {
		if let Some(index) = self.fetch_children_index(at) {
			return self.get_node(index, backend);
		}
		None
	}

	fn fetch_nb_children(&mut self) -> Option<usize> {
		Some(self.original_nb_children)
	}

	fn fetch_value_index(&self) -> Option<ValueIndexFor<N>> {
		debug_assert!(self.value_state == ValueState::Unfetched);
		self.encoded.fetch_value_index()
	}

	fn fetch_value(&self, backend: &N::Backend) -> Option<N::Value> {
		if let Some(ix) = self.encoded.fetch_value_index() {
			backend.get_value(ix)
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
		// TODO could just expose backend index
		// and remove this function.
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
		remove_value: &mut Vec<ValueIndexFor<N>>,
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
			use crate::children::Children;
			use crate::WithChildState;
			if let Some(mut new) = node.backend.need_resize() {
				encoded_change = true;
				let mut next_change = node.children.get_next_child_index(None);
				let mut next_backend = node.backend.encoded.get_next_child_index(None);
				// insert from old and node change.
				loop {
					let (do_change, do_backend) = match (next_change, next_backend) {
						(Some(c), Some(b)) if b > c.to_usize() => (Some(c), None),
						(Some(c), None) => (Some(c), None),
						(Some(c), Some(b)) if b == c.to_usize() => {
							next_backend = node.backend.encoded.get_next_child_index(Some(b));
							(Some(c), None)
						},
						(None, Some(b)) | (Some(_), Some(b)) => (None, Some(b)),
						(None, None) => break,
					};
					if let Some(change_ix) = do_change {
						next_change = node.children.get_next_child_index(Some(change_ix));
						match node.children.get_child_mut(change_ix).map(|c| c.node_mut()) {
							Some(Some(mut child)) => {
								if let Some(ix) = Self::commit_change_fuse(&mut child, remove_node, remove_value, backend) {
									new.encoded.set_children_index(change_ix, &ix);
								} else {
									if let Some(ix) = child.backend.index.clone() {
										new.encoded.set_children_index(change_ix, &ix);
									}
								}
							},
							Some(None) => {
								// no need to remove on new backend.
								node.children.remove_child(change_ix);
							},
							None => (),
						}
					} else if let Some(backend_ix) = do_backend {
						next_backend = node.backend.encoded.get_next_child_index(Some(backend_ix));
						let backend_ix = KeyIndexFor::<N>::from_usize(backend_ix);
						if let Some(ix) = node.backend.encoded.fetch_children_index(backend_ix) {
							new.encoded.set_children_index(backend_ix, &ix);
						}
					}
				}
				node.backend = new;
			} else {
				let mut from = None;
				// iterate child change and add to existing backend
				while let Some(next) = node.children.get_next_child_index(from) {
					from = Some(next);
					match node.children.get_child_mut(next).map(|c| c.node_mut()) {
						Some(Some(mut child)) => {
							if let Some(ix) = Self::commit_change_fuse(&mut child, remove_node, remove_value, backend) {
								encoded_change = true;
								node.backend.encoded.set_children_index(next, &ix);
							} else {
								// set old index (can be unset due to value not new but split
								// that makes it a new child. TODO manage differently. eg return if modified from
								// commit.
								if let Some(ix) = child.backend.index.clone() {
									node.backend.encoded.set_children_index(next, &ix);
								}
							}
						},
						Some(None) => {
							encoded_change = true;
							node.backend.encoded.remove_children_index(next);
							node.children.remove_child(next);
						},
						None => (),
					}
				}
			}
		}
		if node.backend.value_state == ValueState::Modified {
			if let Some(value) = node.value.as_ref() {
				let ix = if let Some(previous) = node.backend.encoded.fetch_value_index() {
					backend.update_value(previous.clone(), value.clone()).unwrap_or(previous)
				} else {
					backend.set_new_value(value.clone())
				};

				node.backend.encoded.set_value_index(&ix);
				encoded_change = true;
				node.backend.value_state = ValueState::Resolved;
			} else {
				unreachable!("modified should be some");
			}
		}
		if node.backend.value_state == ValueState::Deleted {
			if let Some(previous) = node.backend.encoded.fetch_value_index() {
				backend.remove_value(previous);
				encoded_change = true;
			} else {
				unreachable!("deleted should be some");
			};
			node.backend.encoded.set_value_index(&ValueIndexFor::<N>::UNDEFINED);
			node.backend.value_state = ValueState::Resolved;
		}

		node.backend.children_changed = false;
		node.backend.original_nb_children = node.backend.pending_nb_children;
		encoded_change = encoded_change || node.backend.prefix_changed;
		if node.backend.prefix_changed {
			node.backend.encoded.set_prefix(&node.key);
			encoded_change = true;
			node.backend.prefix_changed = false;
		}

		if encoded_change {
			let encoded = node.backend.encoded.as_ref().to_vec();
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

	fn clear_content(&mut self) -> Option<ValueIndexFor<N>> {
		let index = self.index.clone();
		let result = self.encoded.fetch_value_index();
		debug_assert!(result.is_none() || self.value_state == ValueState::Deleted); // TODO can actually only fetch if deleted, but assert for now.
		*self = ArtLocalBackend::with_capacity(self.original_nb_children);
		self.children_changed = true;
		self.prefix_changed = true;
		self.index = index; // got index relative to a capacity.
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

fn varint_encode(mut size: u64, buf: &mut [u8], encoded_len: usize) -> &[u8] {
	let nb = encoded_len;
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
	let nb_bit = core::cmp::max(64 - size.leading_zeros(), 1);
	(nb_bit as usize + 6) / 7
}

fn read_u64(enc: &[u8]) -> u64 {
	let mut buf = [0; 8];
	buf[..].copy_from_slice(&enc[..8]);
	u64::from_le_bytes(buf)
}

fn write_u64(ix: u64, dest: &mut [u8]) {
	let buf = ix.to_le_bytes();
	dest[..8].copy_from_slice(&buf[..]);
}

fn read_u32(enc: &[u8]) -> u32 {
	let mut buf = [0; 4];
	buf[..].copy_from_slice(&enc[..4]);
	u32::from_le_bytes(buf)
}

fn write_u32(ix: u32, dest: &mut [u8]) {
	let buf = ix.to_le_bytes();
	dest[..4].copy_from_slice(&buf[..]);
}

#[cfg(test)]
fn read_u128(enc: &[u8]) -> u128 {
	let mut buf = [0; 16];
	buf[..].copy_from_slice(&enc[..16]);
	u128::from_le_bytes(buf)
}

#[cfg(test)]
fn write_u128(ix: u128, dest: &mut [u8]) {
	let buf = ix.to_le_bytes();
	dest[..16].copy_from_slice(&buf[..]);
}
