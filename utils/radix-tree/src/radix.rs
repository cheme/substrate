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

//! Radix related variants for keys and children.

use crate::{PrefixKey, NodeKeyBuff, LimitPartial};
use core::fmt::Debug;
use core::cmp::Ordering;
use alloc::borrow::Borrow;
use derivative::Derivative;
use crate::children::NodeIndex;

/// Masking operation for unaliged prefix key.
///
/// Note that no configuration of `MaskKeyByte` should result
/// in an empty byte. Instead of an empty byte we should use
/// the full byte configuration (`last`) at the previous index.
pub trait MaskKeyByte: Clone + Copy + PartialEq + Debug {
	/// First mask for a byte.
	const FIRST: Self;
	
	/// Last mask for a byte.
	const LAST: Self;

	/// Mask left part of a byte.
	fn mask_start(&self, byte: u8) -> u8;

	/// Mask right part of a byte.
	/// Inclusive position.
	fn mask_end_incl(&self, byte: u8) -> u8;

	/// Mask right part of a byte.
	/// Exclusive position.
	fn mask_end_excl(&self, byte: u8) -> u8;

	/// Extract u8 index from this byte.
	/// TODO const function? TODO rename to 'at'
	fn index(&self, byte: u8) -> u8;

	/// Insert u8 index into this byte.
	/// TODO rename to 'set_at'
	fn set_index(&self, byte: u8, index: u8) -> u8;

	/// Same as `Ord` `cmp`, but not using a reference.
	fn cmp(&self, other: Self) -> Ordering;

	/// Get mask definition from a byte index.
	/// Not out of bound checks done.
	/// TODO unused??
	fn from_index(index: u8) -> Self;

	/// Get internal mask index.
	fn to_index(&self) -> u8;
}

/// Definition of prefix key.
pub trait PrefixKeyConf {
	/// Is key byte align using this definition.
	const ALIGNED: bool;
	/// Default mask value for aligned.
	const DEFAULT: Option<Self::Mask>;
	/// Either u8 or () depending on wether
	/// we use aligned key.
	type Mask: MaskKeyByte;
	/// Encode the byte mask, it needs to be ordered when encoded.
	fn encode_mask(mask: Self::Mask) -> u8;
	/// Decode the byte mask.
	fn decode_mask(mask: u8) -> Self::Mask;
}

/// Mask for radix 4 node.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Mask4(u8);

/// PrefixKeyConf for a radix 4 tree.
pub struct Prefix4;

mod prefix_key_confs_impls {
	use super::*;

	impl PrefixKeyConf for () {
		const ALIGNED: bool = true;

		const DEFAULT: Option<Self::Mask> = Some(());

		type Mask = ();

		fn encode_mask(_mask: Self::Mask) -> u8 {
			0
		}

		fn decode_mask(_mask: u8) -> Self::Mask {
			()
		}
	}

	impl PrefixKeyConf for bool {
		const ALIGNED: bool = false;

		const DEFAULT: Option<Self::Mask> = None;

		type Mask = bool;

		fn encode_mask(mask: Self::Mask) -> u8 {
			if mask {
				1
			} else {
				0
			}
		}

		fn decode_mask(mask: u8) -> Self::Mask {
			mask == 1
		}
	}

	impl PrefixKeyConf for u8 {
		const ALIGNED: bool = false;

		const DEFAULT: Option<Self::Mask> = None;

		type Mask = u8;

		fn encode_mask(mask: Self::Mask) -> u8 {
			mask
		}

		fn decode_mask(mask: u8) -> Self::Mask {
			mask
		}
	}

	impl PrefixKeyConf for Prefix4 {
		const ALIGNED: bool = false;

		const DEFAULT: Option<Self::Mask> = None;

		type Mask = Mask4;

		fn encode_mask(mask: Self::Mask) -> u8 {
			mask.0
		}

		fn decode_mask(mask: u8) -> Self::Mask {
			Mask4(mask)
		}
	}

	// Equivalent to Mask1
	impl MaskKeyByte for () {
		const FIRST: Self = ();

		const LAST: Self = ();

		fn mask_start(&self, byte: u8) -> u8 {
			byte
		}

		fn mask_end_incl(&self, byte: u8) -> u8 {
			byte
		}

		fn mask_end_excl(&self, _byte: u8) -> u8 {
			0
		}

		fn index(&self, byte: u8) -> u8 {
			byte
		}

		fn set_index(&self, _byte: u8, index: u8) -> u8 {
			index
		}

		fn cmp(&self, _other: Self) -> Ordering {
			Ordering::Equal
		}

		fn from_index(_index: u8) -> Self {
			()
		}

		fn to_index(&self) -> u8 {
			0
		}
	}

	// This is equivalent to Mask2
	impl MaskKeyByte for bool {
		const FIRST: Self = false;

		const LAST: Self = true;

		fn mask_start(&self, byte: u8) -> u8 {
			if *self {
				byte & 0x0f
			} else {
				byte
			}
		}

		fn mask_end_incl(&self, byte: u8) -> u8 {
			if *self {
				byte
			} else {
				byte & 0xf0
			}
		}

		fn mask_end_excl(&self, byte: u8) -> u8 {
			if *self {
				byte & 0xf0
			} else {
				0
			}
		}

		fn index(&self, byte: u8) -> u8 {
			if *self {
				byte & 0x0f
			} else {
				(byte & 0xf0) >> 4
			}
		}

		fn set_index(&self, byte: u8, index: u8) -> u8 {
			if *self {
				(byte & 0xf0) | index
			} else {
				(byte & 0x0F) | (index << 4)
			}
		}

		fn cmp(&self, other: Self) -> Ordering {
			match (*self, other) {
				(false, true) => Ordering::Less,
				(true, false) => Ordering::Greater,
				(true, true)
					| (false, false) => Ordering::Equal,
			}
		}

		fn from_index(index: u8) -> Self {
			index != 0
		}

		fn to_index(&self) -> u8 {
			if *self {
				1
			} else {
				0
			}
		}
	}

	// TODO Mask macro for u8 derivative and use Mask8 here
	// u8 is equivalent to Mask8 (for binary tree).
	impl MaskKeyByte for u8 {
		const FIRST: Self = 0;

		const LAST: Self = 7;

		fn mask_start(&self, byte: u8) -> u8 {
			byte & (0b11111111 >> self)
		}

		fn mask_end_incl(&self, byte: u8) -> u8 {
			byte & (0b11111111 << (7 - self) )
		}

		fn mask_end_excl(&self, byte: u8) -> u8 {
			byte & (0b11111111 << (8 - self) )
		}

		fn index(&self, byte: u8) -> u8 {
			(byte & (0b10000000 >> self)) >> (7 - self)
		}

		fn set_index(&self, byte: u8, index: u8) -> u8 {
			(byte & !(0b10000000 >> self)) | (index << (7 - self))
		}

		fn cmp(&self, other: Self) -> Ordering {
			<u8 as core::cmp::Ord>::cmp(self, &other)
		}

		fn from_index(index: u8) -> Self {
			index
		}

		fn to_index(&self) -> u8 {
			*self
		}
	}

	impl MaskKeyByte for Mask4 {
		const FIRST: Self = Mask4(0);

		const LAST: Self = Mask4(3);

		fn mask_start(&self, byte: u8) -> u8 {
			let shift = self.0 * 2;
			byte & (0b11111111 >> shift)
		}

		fn mask_end_incl(&self, byte: u8) -> u8 {
			let shift = self.0 * 2;
			byte & (0b11111111 << (6 - shift) )
		}

		fn mask_end_excl(&self, byte: u8) -> u8 {
			if self.0 == 0 {
				byte
			} else {
				let shift = self.0 * 2;
				byte & (0b11111111 << (8 - shift) )
			}
		}

		fn index(&self, byte: u8) -> u8 {
			let shift = self.0 * 2;
			(byte & (0b11000000 >> shift)) >> (6 - shift)
		}

		fn set_index(&self, byte: u8, index: u8) -> u8 {
			let shift = self.0 * 2;
			(byte & !(0b11000000 >> shift)) | (index << (6 - shift))
		}

		fn cmp(&self, other: Self) -> Ordering {
			<u8 as core::cmp::Ord>::cmp(&self.0, &other.0)
		}

		fn from_index(index: u8) -> Self {
			Mask4(index)
		}

		fn to_index(&self) -> u8 {
			self.0
		}
	}
}

/// Definition of the tree node structure.
pub trait RadixConf {
	/// Prefix alignement and mask.
	type Alignment: PrefixKeyConf;

	/// Index for a given `NodeChildren`.
	type KeyIndex: NodeIndex;

	/// Maximum number of children per item.
	/// Corresponding node depth (not counting partial key)
	/// in bits is therefore Log2(CHILDREN_CAPACITY).
	const CHILDREN_CAPACITY: usize;

	/// Advance one item in depth.
	/// Return next mask and number of incremented bytes.
	fn advance(previous_mask: MaskFor<Self>) -> (MaskFor<Self>, usize);

	/// Advance with multiple depth steps.
	fn advance_by(mut previous_mask: MaskFor<Self>, nb: usize) -> (MaskFor<Self>, usize) {
		let mut bytes = 0;
		for _i in 0..nb {
			let (new_mask, b) = Self::advance(previous_mask);
			previous_mask = new_mask;
			bytes += b;
		}
		(previous_mask, bytes)
	}

	/// Get child node index for a given position (depth) of a key.
	fn index(key: &[u8], at: Position<Self::Alignment>) -> Option<Self::KeyIndex>;

	/// Set index at a given position in a key.
	fn set_index(key: &mut NodeKeyBuff, at: Position<Self::Alignment>, index: Self::KeyIndex);

	/// Get first non matching position from a delta resulting from a xor comparison.
	fn common_until_delta(delta: u8) -> MaskFor<Self>;
}

pub(crate) type MaskFor<N> = <<N as RadixConf>::Alignment as PrefixKeyConf>::Mask;

/// Different tree radix confs implementations.
pub mod impls {
	use super::*;

	pub struct Radix256Conf;
	pub struct Radix2Conf;
	pub struct Radix4Conf;
	pub struct Radix16Conf;

	impl RadixConf for Radix16Conf {
		type Alignment = bool;

		type KeyIndex = crate::children::Index16;

		const CHILDREN_CAPACITY: usize = 16;

		fn advance(previous_mask: MaskFor<Self>) -> (MaskFor<Self>, usize) {
			if previous_mask {
				(false, 1)
			} else {
				(true, 0)
			}
		}

		fn advance_by(previous_mask: MaskFor<Self>, nb: usize) -> (MaskFor<Self>, usize) {
			let inc = nb / 2;
			if (nb % 2) > 0 {
				if previous_mask {
					(!previous_mask, inc + 1)
				} else {
					(!previous_mask, inc)
				}
			} else {
				(previous_mask, inc)
			}
		}

		fn common_until_delta(delta: u8) -> MaskFor<Self> {
			delta < 16
		}

		fn index(key: &[u8], at: Position<Self::Alignment>) -> Option<Self::KeyIndex> {
			key.get(at.index).map(|byte| {
				Self::KeyIndex::from_usize(at.mask.index(*byte) as usize)
			})
		}

		fn set_index(key: &mut NodeKeyBuff, at: Position<Self::Alignment>, index: Self::KeyIndex) {
			if key.len() <= at.index {
				key.resize(at.index + 1, 0);
			}
			key.get_mut(at.index).map(|byte| {
				*byte = at.mask.set_index(*byte, index.to_usize() as u8)
			});
		}
	}

	impl RadixConf for Radix4Conf {
		type Alignment = Prefix4;

		type KeyIndex = crate::children::Index4;

		const CHILDREN_CAPACITY: usize = 4;

		// TODO put in MaskFor ? generally radix conf seems useless??
		fn advance(previous_mask: MaskFor<Self>) -> (MaskFor<Self>, usize) {
			if previous_mask != MaskFor::<Self>::LAST {
				(Mask4(previous_mask.0 + 1), 0)
			} else {
				(MaskFor::<Self>::FIRST, 1)
			}
		}

		fn advance_by(previous_mask: MaskFor<Self>, nb: usize) -> (MaskFor<Self>, usize) {
			let inc = nb / 4;
			let rem = (nb % 4) as u8;
			if previous_mask.0 + rem > 3 {
				(Mask4(previous_mask.0 + rem - 4), inc + 1)
			} else {
				(Mask4(previous_mask.0 + rem), inc)
			}
		}

		fn common_until_delta(delta: u8) -> MaskFor<Self> {
			Mask4((delta.leading_zeros() / 2)  as u8)
		}

		// TODO trait method? and from_index for index (uncheckd
		fn index(key: &[u8], at: Position<Self::Alignment>) -> Option<Self::KeyIndex> {
			key.get(at.index).map(|byte| {
				Self::KeyIndex::from_usize(at.mask.index(*byte) as usize)
			})
		}

		fn set_index(key: &mut NodeKeyBuff, at: Position<Self::Alignment>, index: Self::KeyIndex) {
			if key.len() <= at.index {
				key.resize(at.index + 1, 0);
			}
			key.get_mut(at.index).map(|byte| {
				*byte = at.mask.set_index(*byte, index.to_usize() as u8)
			});
		}
	}

	impl RadixConf for Radix256Conf {
		type Alignment = ();

		type KeyIndex = u8;

		const CHILDREN_CAPACITY: usize = 256;

		fn advance(_previous_mask: MaskFor<Self>) -> (MaskFor<Self>, usize) {
			((), 1)
		}

		fn advance_by(_previous_mask: MaskFor<Self>, nb: usize) -> (MaskFor<Self>, usize) {
			((), nb)
		}

		fn common_until_delta(_delta: u8) -> MaskFor<Self> {
			()
		}

		fn index(key: &[u8], at: Position<Self::Alignment>) -> Option<Self::KeyIndex> {
			key.get(at.index).map(|byte| {
				at.mask.index(*byte)
			})
		}

		fn set_index(key: &mut NodeKeyBuff, at: Position<Self::Alignment>, index: Self::KeyIndex) {
			if key.len() <= at.index {
				key.resize(at.index + 1, 0);
			}
			key.get_mut(at.index).map(|byte| {
				*byte = at.mask.set_index(*byte, index) // TODO no need to call function here (aligned)
			});
		}
	}

	impl RadixConf for Radix2Conf {
		type Alignment = u8;

		type KeyIndex = bool;

		const CHILDREN_CAPACITY: usize = 2;

		fn advance(previous_mask: MaskFor<Self>) -> (MaskFor<Self>, usize) {
			if previous_mask < 255 {
				unimplemented!()
			//	(previous_mask + 1, 0)
			} else {
				(0, 1)
			}
		}

		fn advance_by(_previous_mask: MaskFor<Self>, _nb: usize) -> (MaskFor<Self>, usize) {
			unimplemented!("TODO implement or default")
		}

		fn common_until_delta(delta: u8) -> MaskFor<Self> {
			delta.leading_zeros() as u8
		}

		fn index(key: &[u8], at: Position<Self::Alignment>) -> Option<Self::KeyIndex> {
			key.get(at.index).map(|byte| {
				at.mask.index(*byte) > 0
			})
		}

		fn set_index(key: &mut NodeKeyBuff, at: Position<Self::Alignment>, index: Self::KeyIndex) {
			if key.len() <= at.index {
				key.resize(at.index + 1, 0);
			}
			key.get_mut(at.index).map(|byte| {
				*byte = at.mask.set_index(*byte, if index {
					0
				} else {
					1
				})
			});
		}
	}
}


impl<D1, D2, P, L> PartialEq<PrefixKey<D2, P, L>> for PrefixKey<D1, P, L>
	where
		D1: Borrow<[u8]>,
		D2: Borrow<[u8]>,
		P: PrefixKeyConf,
		L: LimitPartial,
{
	fn eq(&self, other: &PrefixKey<D2, P, L>) -> bool {
		// !! this means either 255 or 0 mask
		// is forbidden!!
		// 0 should be forbidden, 255 when full byte
		// eg 1 byte slice is 255 and empty is always
		// same as a -1 byte so 255 mask
		let left = self.data.borrow();
		let right = other.data.borrow();
		left.len() == right.len()
			&& self.start == other.start
			&& self.end == other.end
			&& (left.len() == 0
				|| left.len() == 1 && self.unchecked_single_byte() == other.unchecked_single_byte()
				|| (self.unchecked_first_byte() == other.unchecked_first_byte()
					&& self.unchecked_last_byte_excl() == other.unchecked_last_byte_excl()
					&& left[1..left.len() - 1]
						== right[1..right.len() - 1]
			))
	}
}

impl<D, P, L> Eq for PrefixKey<D, P, L>
	where
		D: Borrow<[u8]>,
		P: PrefixKeyConf,
		L: LimitPartial,
{ }

/// Position in a key.
#[derive(Derivative)]
#[derivative(Clone)]
#[derivative(Copy)]
#[derivative(PartialEq)]
pub struct Position<P>
	where
		P: PrefixKeyConf,
{
	pub(crate) index: usize,
	pub(crate) mask: P::Mask,
}

impl<P> Position<P>
	where
		P: PrefixKeyConf,
{
	pub(crate) fn zero() -> Self {
		Position {
			index: 0,
			mask: P::Mask::FIRST,
		}
	}

	pub(crate) fn next<R: RadixConf<Alignment = P>>(&self) -> Self {
		let (mask, increment) = R::advance(self.mask);
		Position {
			index: self.index + increment,
			mask,
		}
	}

	pub(crate) fn next_by<R: RadixConf<Alignment = P>>(&self, nb: usize) -> Self {
		let (mask, increment) = R::advance_by(self.mask, nb);
		Position {
			index: self.index + increment,
			mask,
		}
	}

	pub(crate) fn index<R: RadixConf<Alignment = P>>(&self, key: &[u8]) -> Option<R::KeyIndex> {
		R::index(key, *self)
	}

	pub(crate) fn set_index<R: RadixConf<Alignment = P>>(&self, key: &mut NodeKeyBuff, index: R::KeyIndex) {
		R::set_index(key, *self, index)
	}

	pub(crate) fn cmp(&self, other: Position<P>) -> Ordering {
		match self.index.cmp(&other.index) {
			Ordering::Equal => {
				self.mask.cmp(other.mask)
			},
			r => r,
		}
	}
}
