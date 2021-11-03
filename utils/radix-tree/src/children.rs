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

//! Children storage related traits and implementations.

use core::fmt::Debug;
use core::mem::replace;
use derivative::Derivative;
use crate::radix::RadixConf;

/// Children node index, depending on the radix use
/// different type can be use.
pub trait NodeIndex: Clone + Copy + Debug + PartialEq + PartialOrd {
	fn zero() -> Self;
	fn next(&self) -> Option<Self>;
	fn to_usize(self) -> usize;
	fn from_usize(position: usize) -> Self;
}

macro_rules! u8_index {
	($struct_name: ident, $size: expr) => {
		/// Radix $size node index.
		#[repr(transparent)]
		#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
		pub struct $struct_name(u8);

		impl NodeIndex for $struct_name {
			fn zero() -> Self {
				$struct_name(0)
			}
			fn next(&self) -> Option<Self> {
				if self.0 == $size - 1 {
					None
				} else {
					Some($struct_name(self.0 + 1))
				}
			}
			fn to_usize(self) -> usize {
				self.0 as usize
			}
			fn from_usize(position: usize) -> Self {
				$struct_name(position as u8)
			}
		}
	}
}

u8_index!(Index4, 4);
u8_index!(Index16, 16);

mod node_indexes_impls {
	use super::*;

	impl NodeIndex for bool {
		fn zero() -> Self {
			false
		}
		fn next(&self) -> Option<Self> {
			if *self {
				None
			} else {
				Some(true)
			}
		}
		fn to_usize(self) -> usize {
			self as usize
		}
		fn from_usize(position: usize) -> Self {
			position != 0
		}
	}

	impl NodeIndex for u8 {
		fn zero() -> Self {
			0
		}
		fn next(&self) -> Option<Self> {
			if *self == 255 {
				None
			} else {
				Some(*self + 1)
			}
		}
		fn to_usize(self) -> usize {
			self as usize
		}
		fn from_usize(position: usize) -> Self {
			position as u8
		}
	}
}

/// A children node container.
pub trait Children: Clone + Debug {
	/// if using backend and this is true,
	/// all value present are initiated explicitelly
	/// to unfetched (non stored value are therefore
	/// empty), otherwhise all values are consider
	/// unfetched.
	type Radix: RadixConf;
	type Node;

	/// Initialize size is size from backend.
	fn empty(initial_size: usize) -> Self;

	/// TODO associated value?
	fn need_init_unfetched() -> bool;

	/// Set or replace child.
	/// Warning this does not update the total
	/// stored number (update could just be
	/// setting value of an unfetched node).
	/// Please use `increase_number` when needed.
	fn set_child(
		&mut self,
		index: <Self::Radix as RadixConf>::KeyIndex,
		child: Self::Node,
	) -> Option<Self::Node>;

	/// Remove a child.
	/// Warning do not update number of child.
	fn remove_child(
		&mut self,
		index: <Self::Radix as RadixConf>::KeyIndex,
	) -> Option<Self::Node>;

	fn number_child(
		&self,
	) -> usize;

	fn get_child(
		&self,
		index: <Self::Radix as RadixConf>::KeyIndex,
	) -> Option<&Self::Node>;

	fn get_child_mut(
		&mut self,
		index: <Self::Radix as RadixConf>::KeyIndex,
	) -> Option<&mut Self::Node>;

	fn get_next_child_index(
		&self,
		mut index: Option<<Self::Radix as RadixConf>::KeyIndex>,
	) -> Option<<Self::Radix as RadixConf>::KeyIndex> {
		while let Some(i) = match index {
			Some(i) => i.next(),
			None => Some(<Self::Radix as RadixConf>::KeyIndex::zero()),
		} {
			if self.get_child(i).is_some() {
				return Some(i);
			}
			index = Some(i);
		}
		None
	}
}

#[derive(Derivative)]
#[derivative(Clone)]
#[derivative(Debug)]
struct Children2<N> (
	Option<(Option<N>, Option<N>)>
);

impl<N: Debug + Clone> Children for Children2<N> {
	type Radix = crate::radix::impls::Radix2Conf;

	type Node = N;


	fn empty(_initial_size: usize) -> Self {
		Children2(None)
	}

	fn need_init_unfetched() -> bool {
		true // to have working nb_children
	}

	fn set_child(
		&mut self,
		index: <Self::Radix as RadixConf>::KeyIndex,
		child: N,
	) -> Option<N> {
		if self.0.is_none() {
			self.0 = Some((None, None));
		}
		let children = self.0.as_mut()
			.expect("Lazy init above");
		if index {
			replace(&mut children.0, Some(child))
		} else {
			replace(&mut children.1, Some(child))
		}
	}

	fn remove_child(
		&mut self,
		index: <Self::Radix as RadixConf>::KeyIndex,
	) -> Option<N> {
		if let Some(children) = self.0.as_mut() {
			if index {
				replace(&mut children.0, None)
			} else {
				replace(&mut children.1, None)
			}
		} else {
			None
		}
	}

	fn number_child(
		&self,
	) -> usize {
		match self.0.as_ref() {
			Some(b) => match &b {
				(Some(_), Some(_)) => 2,
				(None, Some(_)) => 1,
				(Some(_), None) => 1,
				(None, None) => 0,
			},
			None => 0,
		}
	}

	fn get_child(
		&self,
		index: <Self::Radix as RadixConf>::KeyIndex,
	) -> Option<&N> {
		self.0.as_ref().and_then(|b| if index {
			b.0.as_ref()
		} else {
			b.1.as_ref()
		})
	}

	fn get_child_mut(
		&mut self,
		index: <Self::Radix as RadixConf>::KeyIndex,
	) -> Option<&mut N> {
		self.0.as_mut().and_then(|b| if index {
			b.0.as_mut()
		} else {
			b.1.as_mut()
		})
	}
}

#[derive(Derivative)]
#[derivative(Clone)]
pub struct Children256<N> (
	// 256 array is to big but ok for initial implementation
	Option<[Option<N>; 256]>,
	u16, // u16 as could overflow u8 with size 256
);

/// 48 children for art tree.
#[derive(Derivative)]
#[derivative(Clone)]
pub struct ART48<N> (
	([u8; 256], [Option<N>; 48]),
	u8,
);

/// 16 children for art tree.
#[derive(Derivative)]
#[derivative(Clone)]
pub struct ART16<N> (
	([u8; 16], [Option<N>; 16]),
	u8,
);

/// 4 children for art tree.
#[derive(Derivative)]
#[derivative(Clone)]
pub struct ART4<N> (
	([u8; 4], [Option<N>; 4]),
	u8,
);

/// Adaptative only between 48 and 256.
#[derive(Derivative)]
#[derivative(Clone)]
pub enum ART48_256<N> {
	ART4(ART4<N>),
	ART16(ART16<N>),
	ART48(ART48<N>),
	ART256(Children256<N>),
}


macro_rules! exp2_array {
	(@6, [$($inpp:expr),*]) => { exp2_array!(@5, [$($inpp,)* $($inpp),*]) };
	(@5, [$($inpp:expr),*]) => { exp2_array!(@4, [$($inpp,)* $($inpp),*]) };
	(@4, [$($inpp:expr),*]) => { exp2_array!(@3, [$($inpp,)* $($inpp),*]) };
	(@3, [$($inpp:expr),*]) => { exp2_array!(@2, [$($inpp,)* $($inpp),*]) };
	(@2, [$($inpp:expr),*]) => { exp2_array!(@1, [$($inpp,)* $($inpp),*]) };
	(@1, [$($inpp:expr),*]) => { [$($inpp,)* $($inpp),*] };
}

#[inline]
const fn empty_4_children<N>() -> [Option<N>; 4] {
	[
		None, None, None, None,
	]
}

#[inline]
const fn empty_16_children<N>() -> [Option<N>; 16] {
	exp2_array!(@3, [None, None])
}

#[inline]
const fn empty_48_children<N>() -> [Option<N>; 48] {
	exp2_array!(@4, [None, None, None])
}

#[inline]
const fn empty_256_children<N>() -> [Option<N>; 256] {
	exp2_array!(@6, [None, None, None, None])
}

impl<N: Debug> Debug for Children256<N> {
	fn fmt(&self, f: &mut core::fmt::Formatter) -> core::result::Result<(), core::fmt::Error> {
		if let Some(children) = self.0.as_ref() {
			children[..].fmt(f)
		} else {
			let empty: &[N] = &[]; 
			empty.fmt(f)
		}
	}
}

impl<N: Debug> Debug for ART48<N> {
	fn fmt(&self, f: &mut core::fmt::Formatter) -> core::result::Result<(), core::fmt::Error> {
		"unimplemented children 48".fmt(f)
	}
}

impl<N: Debug> Debug for ART16<N> {
	fn fmt(&self, f: &mut core::fmt::Formatter) -> core::result::Result<(), core::fmt::Error> {
		"unimplemented children 16".fmt(f)
	}
}

impl<N: Debug> Debug for ART4<N> {
	fn fmt(&self, f: &mut core::fmt::Formatter) -> core::result::Result<(), core::fmt::Error> {
		"unimplemented children 4".fmt(f)
	}
}

impl<N: Debug> Debug for ART48_256<N> {
	fn fmt(&self, f: &mut core::fmt::Formatter) -> core::result::Result<(), core::fmt::Error> {
		"unimplemented art children format".fmt(f)
	}
}

impl<N: Debug + Clone> Children for Children256<N> {
	type Radix = crate::radix::impls::Radix256Conf;

	type Node = N;

	fn empty(initial_size: usize) -> Self {
		Children256(None, initial_size as u16)
	}

	fn need_init_unfetched() -> bool {
		false
	}

	fn set_child(
		&mut self,
		index: <Self::Radix as RadixConf>::KeyIndex,
		child: N,
	) -> Option<N> {
		if self.0.is_none() {
			self.0 = Some(empty_256_children());
		}
		let children = self.0.as_mut()
			.expect("Lazy init above");
		let r = children[index as usize].take();
		children[index as usize] = Some(child);
		if r.is_none() {
			self.1 += 1;
		}
		r
	}

	fn remove_child(
		&mut self,
		index: <Self::Radix as RadixConf>::KeyIndex,
	) -> Option<N> {
		if let Some(children) = self.0.as_mut() {
			let r = replace(&mut children[index as usize], None);
			if r.is_some() {
				self.1 -= 1;
			}
			r
		} else {
			None
		}
	}

	fn number_child(
		&self,
	) -> usize {
		self.1 as usize
	}

	fn get_child(
		&self,
		index: <Self::Radix as RadixConf>::KeyIndex,
	) -> Option<&N> {
		self.0.as_ref().and_then(|b| b[index as usize].as_ref())
	}

	fn get_child_mut(
		&mut self,
		index: <Self::Radix as RadixConf>::KeyIndex,
	) -> Option<&mut N> {
		self.0.as_mut().and_then(|b| b[index as usize].as_mut())
	}
}

macro_rules! impl_children {
	($struct_name: ident, $radix_conf: ident, $empty: ident, $size: expr) => {

#[derive(Derivative)]
#[derivative(Clone)]
pub struct $struct_name<N> (
	[Option<N>; $size],
	u8,
);

impl<N: Debug> Debug for $struct_name<N> {
	fn fmt(&self, f: &mut core::fmt::Formatter) -> core::result::Result<(), core::fmt::Error> {
		self.0[..].fmt(f)
	}
}

impl<N: Debug + Clone> Children for $struct_name<N> {
	type Radix = crate::radix::impls::$radix_conf;

	type Node = N;

	fn empty(initial_size: usize) -> Self {
		$struct_name($empty(), initial_size as u8)
	}

	fn need_init_unfetched() -> bool {
		false
	}

	fn set_child(
		&mut self,
		index: <Self::Radix as RadixConf>::KeyIndex,
		child: N,
	) -> Option<N> {
		let children = &mut self.0;
		let result = replace(&mut children[index.to_usize()], Some(child));
		if result.is_none() {
			self.1 += 1;
		}
		result
	}

	fn remove_child(
		&mut self,
		index: <Self::Radix as RadixConf>::KeyIndex,
	) -> Option<N> {
		let children = &mut self.0.as_mut();
		let r = replace(&mut children[index.to_usize()], None);
		if r.is_some() {
			self.1 -= 1;
		}
		r
	}

	fn number_child(
		&self,
	) -> usize {
		self.1 as usize
	}

	fn get_child(
		&self,
		index: <Self::Radix as RadixConf>::KeyIndex,
	) -> Option<&N> {
		self.0[index.to_usize()].as_ref()
	}

	fn get_child_mut(
		&mut self,
		index: <Self::Radix as RadixConf>::KeyIndex,
	) -> Option<&mut N> {
		self.0[index.to_usize()].as_mut()
	}
}

}}

impl_children!(Children4, Radix4Conf, empty_4_children, 4);
impl_children!(Children16, Radix16Conf, empty_16_children, 16);

impl<N: Debug + Clone> Children256<N> {
	fn need_reduce(
		&self,
	) -> bool {
		self.1 <= REM_TRESHOLD48 as u16
	}

	fn reduce_node(&mut self) -> ART48<N> {
		debug_assert!(self.1 <= 48);
		let mut result = ART48::empty(0);
		for i in 0..=255 {
			if let Some(child) = self.remove_child(i) {
				result.set_child(i, child);
			}
		}
		result
	}
}

const UNSET48: u8 = 49u8;

// we cannot really change this or at least I see no use
// in anticipating a node switch when growing
const ADD_TRESHOLD48: u8 = 48u8;
const ADD_TRESHOLD16: u8 = 16u8;
const ADD_TRESHOLD4: u8 = 4u8;

// using smaller that 48 to avoid add-remove-add-remove...
// situation
const REM_TRESHOLD48: u8 = 40u8;
const REM_TRESHOLD16: u8 = 16u8;
const REM_TRESHOLD4: u8 = 4u8;

use crate::radix::impls::Radix256Conf;

impl<N: Debug + Clone> ART48<N> {
	fn empty(initial_size: usize) -> Self {
		ART48(([UNSET48; 256], empty_48_children()), initial_size as u8)
	}

	fn need_reduce(
		&self,
	) -> bool {
		self.1 <= REM_TRESHOLD16
	}

	fn reduce_node(&mut self) -> ART16<N> {
		debug_assert!(self.1 <= 16);
		let mut result = ART16::empty(0);
		let (indexes, values) = &mut self.0;
		for i in 0..=255 {
			let index = indexes[i as usize];
			if index != UNSET48 {
				if let Some(value) = values[index as usize].take() {
					result.set_child(i, value);
				}
			}
		}
		result
	}

	fn grow_node(&mut self) -> Children256<N> {
		if self.1 == 0 {
			return Children256::empty(0);
		}
		let mut result = Children256::empty(0);
		let (indexes, values) = &mut self.0;
		for i in 0..=255 {
			let ix = indexes[i];
			if ix != UNSET48 {
				let value = values[ix as usize].take()
					.expect("Not unset");
				result.set_child(i as u8, value);
			}
		}
		result
	}

	fn can_set_child(
		&mut self,
		index: <Radix256Conf as RadixConf>::KeyIndex,
	) -> bool {
		if self.1 < ADD_TRESHOLD48 {
			return true;
		}
		let (indexes, _values) = &self.0;
		let is_new = indexes[index as usize] == UNSET48;
		if is_new && self.1 >= ADD_TRESHOLD48 {
			return false;
		}
		true
	}

	fn set_child(
		&mut self,
		index: <Radix256Conf as RadixConf>::KeyIndex,
		child: N,
	) -> Option<Option<N>> {
		let (indexes, values) = &mut self.0;
		let is_new = indexes[index as usize] == UNSET48;
		if is_new && self.1 >= ADD_TRESHOLD48 {
			return None;
		}
		let result = if is_new {
			indexes[index as usize] = self.1;
			values[self.1 as usize] = Some(child);
			self.1 += 1;
			None
		} else {
			let ix = indexes[index as usize];
			replace(&mut values[ix as usize], Some(child))
		};
		Some(result)
	}

	fn remove_child(
		&mut self,
		index: <Radix256Conf as RadixConf>::KeyIndex,
	) -> Option<N> {
		if self.1 == 0 {
			return None;
		}
		let (indexes, values) = &mut self.0;
		if indexes[index as usize] != UNSET48 {
			let old_index = indexes[index as usize];
			let result = replace(&mut values[old_index as usize], None);
			debug_assert!(result.is_some());
			self.1 -= 1;
			indexes[index as usize] = UNSET48;
			if old_index != self.1 {
				// slow removal implementation (may do something here with u128 bit ops.
				let mut found = None;
				for (ix, value_ix) in indexes.iter().enumerate() {
					if *value_ix == self.1 {
						found = Some(ix);
						break;
					}
				}
				if let Some(ix) = found {
					let v = values[indexes[ix] as usize].take();
					values[old_index as usize] = v;
					indexes[ix] = old_index;
				}
			}
			result
		} else {
			None
		}
	}

	fn number_child(
		&self,
	) -> usize {
		self.1 as usize
	}

	fn get_child(
		&self,
		index: <Radix256Conf as RadixConf>::KeyIndex,
	) -> Option<&N> {
		if self.1 == 0 {
			return None;
		}
		let (indexes, values) = &self.0;
		let index = indexes[index as usize];
		if index == UNSET48 {
			return None;
		}
		values[index as usize].as_ref()
	}

	fn get_child_mut(
		&mut self,
		index: <Radix256Conf as RadixConf>::KeyIndex,
	) -> Option<&mut N> {
		if self.1 == 0 {
			return None;
		}
		let (indexes, values) = &mut self.0;
		let index = indexes[index as usize];
		if index == UNSET48 {
			return None;
		}
		values[index as usize].as_mut()
	}

	fn get_next_child_index(
		&self,
		mut index: Option<<Radix256Conf as RadixConf>::KeyIndex>,
	) -> Option<<Radix256Conf as RadixConf>::KeyIndex> {
		while let Some(i) = match index {
			Some(i) => i.next(),
			None => Some(<Radix256Conf as RadixConf>::KeyIndex::zero()),
		} {
			if self.get_child(i).is_some() {
				return Some(i);
			}
			index = Some(i);
		}
		None
	}
}

impl<N: Debug + Clone> ART16<N> {
	fn empty(initial: usize) -> Self {
		ART16(([0u8; 16], empty_16_children()), initial as u8)
	}

	fn grow_node(&mut self) -> ART48<N> {
		if self.1 == 0 {
			return ART48::empty(0);
		}
		let mut result = ART48::empty(0);
		let (indexes, values) = &mut self.0;
		for i in 0..self.1 {
			let ix = indexes[i as usize];
			let value = values[i as usize].take()
				.expect("Restricted by size");
			result.set_child(ix, value);
		}
		result
	}

	fn need_reduce(
		&self,
	) -> bool {
		self.1 <= REM_TRESHOLD4
	}

	fn reduce_node(&mut self) -> ART4<N> {
		debug_assert!(self.1 <= 4);
		let mut result = ART4::empty(0);
		let (indexes, values) = &mut self.0;
		for i in 0..self.1 {
			let index = indexes[i as usize];
			if let Some(value) = values[i as usize].take() {
				result.set_child(index, value);
			}
		}
		result
	}

	// Returns either the old value or the value to set after growth.
	fn set_child(
		&mut self,
		index: <Radix256Conf as RadixConf>::KeyIndex,
		child: N,
	) -> (Option<Option<N>>, Option<N>) {
		let (indexes, values) = &mut self.0;
		let mut existing_index = None;
		// TODO something to do with bit expr
		for i in 0..self.1 {
			if indexes[i as usize] == index {
				existing_index = Some(i);
			}
		}
		if existing_index.is_none() && self.1 >= ADD_TRESHOLD16 {
			return (None, Some(child));
		}
		let result = if let Some(i) = existing_index {
			indexes[i as usize] = index;
			replace(&mut values[i as usize], Some(child))
		} else {
			indexes[self.1 as usize] = index;
			values[self.1 as usize] = Some(child);
			self.1 += 1;
			None
		};
		(Some(result), None)
	}

	fn remove_child(
		&mut self,
		index: <Radix256Conf as RadixConf>::KeyIndex,
	) -> Option<N> {
		if self.1 == 0 {
			return None;
		}
		let (indexes, values) = &mut self.0;
		let mut existing_index = None;
		// TODO something to do with bit expr
		for i in 0..self.1 {
			if indexes[i as usize] == index {
				existing_index = Some(i);
			}
		}
		if let Some(ix) = existing_index {
			self.1 -= 1;
			if ix == self.1 {
				replace(&mut values[ix as usize], None)
			} else {
				let result = replace(&mut values[ix as usize], None);
				values[ix as usize] = values[self.1 as usize].take();
				indexes[ix as usize] = indexes[self.1 as usize];
				result
			}
		} else {
			None
		}
	}

	fn number_child(
		&self,
	) -> usize {
		self.1 as usize
	}

	fn get_child(
		&self,
		index: <Radix256Conf as RadixConf>::KeyIndex,
	) -> Option<&N> {
		if self.1 == 0 {
			return None;
		}
		let (indexes, values) = &self.0;
		for i in 0..self.1 {
			if indexes[i as usize] == index {
				return values[i as usize].as_ref()
			}
		}
		None
	}

	fn get_child_mut(
		&mut self,
		index: <Radix256Conf as RadixConf>::KeyIndex,
	) -> Option<&mut N> {
		if self.1 == 0 {
			return None;
		}
		let (indexes, values) = &mut self.0;
		for i in 0..self.1 {
			if indexes[i as usize] == index {
				return values[i as usize].as_mut()
			}
		}
		None
	}

	fn get_next_child_index(
		&self,
		index: Option<<Radix256Conf as RadixConf>::KeyIndex>,
	) -> Option<<Radix256Conf as RadixConf>::KeyIndex> {
		let mut result = None;
		if let Some(from) = match index {
			Some(i) => i.next(),
			None => Some(<Radix256Conf as RadixConf>::KeyIndex::zero()),
		} {
			for i in 0..self.1 {
				let ix = self.0.0[i as usize];
				if ix >= from {
					if result.map(|r| r > ix).unwrap_or(true) {
						result = Some(ix);
					}
				}
			}
		}
		result
	}
}

impl<N: Debug + Clone> ART4<N> {
	fn empty(initial: usize) -> Self {
		ART4(([0u8; 4], empty_4_children()), initial as u8)
	}

	fn grow_node(&mut self) -> ART16<N> {
		if self.1 == 0 {
			return ART16::empty(0);
		}
		let mut result = ART16::empty(0);
		let (indexes, values) = &mut self.0;
		for i in 0..self.1 {
			let ix = indexes[i as usize];
			let value = values[i as usize].take()
				.expect("Restricted by size");
			result.set_child(ix, value);
		}
		result
	}

	// Returns either the old value or the value to set after growth.
	fn set_child(
		&mut self,
		index: <Radix256Conf as RadixConf>::KeyIndex,
		child: N,
	) -> (Option<Option<N>>, Option<N>) {
		let (indexes, values) = &mut self.0;
		let mut existing_index = None;
		// TODO something to do with bit expr
		for i in 0..self.1 {
			if indexes[i as usize] == index {
				existing_index = Some(i);
				break;
			}
		}
		if existing_index.is_none() && self.1 >= ADD_TRESHOLD4 {
			return (None, Some(child));
		}
		let result = if let Some(i) = existing_index {
			indexes[i as usize] = index;
			replace(&mut values[i as usize], Some(child))
		} else {
			indexes[self.1 as usize] = index;
			values[self.1 as usize] = Some(child);
			self.1 += 1;
			None
		};
		(Some(result), None)
	}

	fn remove_child(
		&mut self,
		index: <Radix256Conf as RadixConf>::KeyIndex,
	) -> Option<N> {
		if self.1 == 0 {
			return None;
		}
		let (indexes, values) = &mut self.0;
		let mut existing_index = None;
		// TODO something to do with bit expr
		for i in 0..self.1 {
			if indexes[i as usize] == index {
				existing_index = Some(i);
			}
		}
		if let Some(ix) = existing_index {
			self.1 -= 1;
			if ix == self.1 {
				replace(&mut values[ix as usize], None)
			} else {
				let result = replace(&mut values[ix as usize], None);
				values[ix as usize] = values[self.1 as usize].take();
				indexes[ix as usize] = indexes[self.1 as usize];
				result
			}
		} else {
			None
		}
	}

	fn number_child(
		&self,
	) -> usize {
		self.1 as usize
	}

	fn get_child(
		&self,
		index: <Radix256Conf as RadixConf>::KeyIndex,
	) -> Option<&N> {
		if self.1 == 0 {
			return None;
		}
		let (indexes, values) = &self.0;
		for i in 0..self.1 {
			if indexes[i as usize] == index {
				return values[i as usize].as_ref()
			}
		}
		None
	}

	fn get_child_mut(
		&mut self,
		index: <Radix256Conf as RadixConf>::KeyIndex,
	) -> Option<&mut N> {
		if self.1 == 0 {
			return None;
		}
		let (indexes, values) = &mut self.0;
		for i in 0..self.1 {
			if indexes[i as usize] == index {
				return values[i as usize].as_mut()
			}
		}
		None
	}

	fn get_next_child_index(
		&self,
		index: Option<<Radix256Conf as RadixConf>::KeyIndex>,
	) -> Option<<Radix256Conf as RadixConf>::KeyIndex> {
		let mut result = None;
		if let Some(from) = match index {
			Some(i) => i.next(),
			None => Some(<Radix256Conf as RadixConf>::KeyIndex::zero()),
		} {
			for i in 0..self.1 {
				let ix = self.0.0[i as usize];
				if ix >= from {
					if result.map(|r| r > ix).unwrap_or(true) {
						result = Some(ix);
					}
				}
			}
		}
		result
	}
}

impl<N> ART48_256<N> {
	pub fn len(&self) -> usize {
		match self {
			ART48_256::ART4(inner) => inner.1 as usize,
			ART48_256::ART16(inner) => inner.1 as usize,
			ART48_256::ART48(inner) => inner.1 as usize,
			ART48_256::ART256(inner) => inner.1 as usize,
		}
	}
}

impl<N: Debug + Clone> Children for ART48_256<N> {
	type Radix = Radix256Conf;
	type Node = N;

	fn empty(capacity: usize) -> Self {
		if capacity <= ADD_TRESHOLD4 as usize {
			ART48_256::ART4(ART4::empty(capacity))
		} else if capacity <= ADD_TRESHOLD16 as usize {
			ART48_256::ART16(ART16::empty(capacity))
		} else if capacity <= ADD_TRESHOLD48 as usize {
			ART48_256::ART48(ART48::empty(capacity))
		} else {
			ART48_256::ART256(Children256::empty(capacity))
		}
	}

	fn need_init_unfetched() -> bool {
		true
	}

	fn set_child(
		&mut self,
		index: <Self::Radix as RadixConf>::KeyIndex,
		mut child: N,
	) -> Option<N> {
		let mut new_256 = match self {
			ART48_256::ART4(inner) => {
				match inner.set_child(index, child) {
					(Some(result), None) => return result,
					(None, Some(value)) => {
						child = value;
						ART48_256::ART16(inner.grow_node())
					},
					_ => unreachable!(),
				}
			},
			ART48_256::ART16(inner) => {
				match inner.set_child(index, child) {
					(Some(result), None) => return result,
					(None, Some(value)) => {
						child = value;
						ART48_256::ART48(inner.grow_node())
					},
					_ => unreachable!(),
				}
			},
			ART48_256::ART48(inner) => {
				if inner.can_set_child(index) {
					return inner.set_child(index, child)
						.expect("checked above");
				} else {
					ART48_256::ART256(inner.grow_node())
				}
			},
			ART48_256::ART256(inner) => {
				return inner.set_child(index, child);
			},
		};
		let result = new_256.set_child(index, child);
		*self = new_256;
		result
	}

	fn remove_child(
		&mut self,
		index: <Self::Radix as RadixConf>::KeyIndex,
	) -> Option<N> {
		let (result, do_reduce) = match self {
			ART48_256::ART256(inner) => {
				let result = inner.remove_child(index);
				// Note that  this is not compatible with a backend
				// usage (would need to resolve from backend), but backend
				// do no node removal until commit. TODO consider consuming
				// commit or remove this piece of code when use with backend
				if result.is_some() && inner.need_reduce() {
					(result, Some(ART48_256::ART48(inner.reduce_node())))
				} else {
					(result, None)
				}
			},
			ART48_256::ART48(inner) => {
				let result = inner.remove_child(index);
				if result.is_some() && inner.need_reduce() {
					(result, Some(ART48_256::ART16(inner.reduce_node())))
				} else {
					(result, None)
				}
			},
			ART48_256::ART16(inner) => {
				let result = inner.remove_child(index);
				if result.is_some() && inner.need_reduce() {
					(result, Some(ART48_256::ART4(inner.reduce_node())))
				} else {
					(result, None)
				}
			},
			ART48_256::ART4(inner) => {
				(inner.remove_child(index), None)
			},
		};
		if let Some(do_reduce) = do_reduce {
			*self = do_reduce;
		}
		result
	}

	fn number_child(
		&self,
	) -> usize {
		match self {
			ART48_256::ART256(inner) => inner.number_child(),
			ART48_256::ART48(inner) => inner.number_child(),
			ART48_256::ART16(inner) => inner.number_child(),
			ART48_256::ART4(inner) => inner.number_child(),
		}
	}

	fn get_child(
		&self,
		index: <Self::Radix as RadixConf>::KeyIndex,
	) -> Option<&N> {
		match self {
			ART48_256::ART256(inner) => inner.get_child(index),
			ART48_256::ART48(inner) => inner.get_child(index),
			ART48_256::ART16(inner) => inner.get_child(index),
			ART48_256::ART4(inner) => inner.get_child(index),
		}
	}

	fn get_child_mut(
		&mut self,
		index: <Self::Radix as RadixConf>::KeyIndex,
	) -> Option<&mut N> {
		match self {
			ART48_256::ART256(inner) => inner.get_child_mut(index),
			ART48_256::ART48(inner) => inner.get_child_mut(index),
			ART48_256::ART16(inner) => inner.get_child_mut(index),
			ART48_256::ART4(inner) => inner.get_child_mut(index),
		}
	}

	fn get_next_child_index(
		&self,
		index: Option<<Self::Radix as RadixConf>::KeyIndex>,
	) -> Option<<Self::Radix as RadixConf>::KeyIndex> {
		match self {
			ART48_256::ART256(inner) => inner.get_next_child_index(index),
			ART48_256::ART48(inner) => inner.get_next_child_index(index),
			ART48_256::ART16(inner) => inner.get_next_child_index(index),
			ART48_256::ART4(inner) => inner.get_next_child_index(index),
		}
	}
}
