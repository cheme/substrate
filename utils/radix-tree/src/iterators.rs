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

//! Different tree iterators and implementations.

use super::{Tree, TreeConf, Node, PositionFor, Descent, Key, KeyIndexFor};
pub use derivative::Derivative;
use alloc::vec::Vec;
use crate::radix::{Position, MaskFor, MaskKeyByte};
use crate::children::NodeIndex;

/// Stack of Node to reach a position.
struct NodeStack<'a, N: TreeConf> {
	// TODO use smallvec instead?
	stack: Vec<(PositionFor<N>, &'a Node<N>)>,
	// The key used with the stack.
	// key: Vec<u8>,
}

impl<'a, N: TreeConf> NodeStack<'a, N> {
	fn new() -> Self {
		NodeStack {
			stack: Vec::new(),
		}
	}
}

#[derive(Eq, PartialEq)]
enum IterState {
	Started,
	StartPrefix,
	Finished,
}

impl<'a, N: TreeConf> NodeStack<'a, N> {
	// TODO useless??
	fn descend(&self, key: &[u8], dest_position: PositionFor<N>) -> Descent<N::Radix> {
		if let Some(top) = self.stack.last() {
			top.1.descend(key, top.0, dest_position)
		} else {
			// using a random key index for root element
			Descent::Child(PositionFor::<N>::zero(), KeyIndexFor::<N>::zero())
		}
	}
}

/// Stack of Node to reach a position.
struct NodeStackNoCache<N: TreeConf> {
	// TODO consider &root to avoid root clone?
	// TODO use smallvec instead?
	stack: Vec<(PositionFor<N>, Node<N>)>,
}

impl<N: TreeConf> NodeStackNoCache<N> {
	fn new() -> Self {
		NodeStackNoCache {
			stack: Vec::new(),
		}
	}
}

impl<N: TreeConf> NodeStackNoCache<N> {
	// TODO useless??
	fn descend(&self, key: &[u8], dest_position: PositionFor<N>) -> Descent<N::Radix> {
		if let Some(top) = self.stack.last() {
			top.1.descend(key, top.0, dest_position)
		} else {
			// using a random key index for root element
			Descent::Child(PositionFor::<N>::zero(), KeyIndexFor::<N>::zero())
		}
	}
}

/// Stack of Node to reach a position.
struct NodeStackMut<N: TreeConf> {
	// TODO use smallvec instead?
	stack: Vec<(PositionFor<N>, *mut Node<N>)>,
	// The key used with the stack.
	// key: Vec<u8>,
}

impl<N: TreeConf> NodeStackMut<N> {
	fn new() -> Self {
		NodeStackMut {
			stack: Vec::new(),
		}
	}
}

impl<N: TreeConf> NodeStackMut<N> {
	fn descend(&self, key: &[u8], dest_position: PositionFor<N>) -> Descent<N::Radix> {
		if let Some(top) = self.stack.last() {
			unsafe {
				top.1.as_mut().unwrap().descend(key, top.0, dest_position)
			}
		} else {
			// using a random key index for root element
			Descent::Child(PositionFor::<N>::zero(), KeyIndexFor::<N>::zero())
		}
	}
}

/// Stack of Node to reach a position.
struct IterStack<'a, N: TreeConf> {
	// TODO use smallvec instead
	// The index is the current index where to descend into if going
	// downward, or where we descend from if going upward.
	stack: Vec<(PositionFor<N>, &'a Node<N>, KeyIndexFor<N>)>,
	// The key used with the stack.
	key: Key,
}

/// Stack of Node to reach a position.
struct IterStackNoCache<N: TreeConf> {
	// TODO use smallvec instead
	// The index is the current index where to descend into if going
	// downward, or where we descend from if going upward.
	stack: Vec<(PositionFor<N>, Node<N>, KeyIndexFor<N>)>,
	// The key used with the stack.
	key: Key,
}

/// Stack of Node to reach a position.
struct IterStackMut<N: TreeConf> {
	// TODO use smallvec instead
	// The index is the current index where to descend into if going
	// downward, or where we descend from if going upward.
	stack: Vec<(PositionFor<N>, *mut Node<N>, KeyIndexFor<N>)>,
	// The key used with the stack.
	key: Key,
}

impl<'a, N: TreeConf> IterStack<'a, N> {
	fn new() -> Self {
		IterStack {
			stack: Vec::new(),
			key: Default::default(),
		}
	}
}

impl<N: TreeConf> IterStackNoCache<N> {
	fn new() -> Self {
		IterStackNoCache {
			stack: Vec::new(),
			key: Default::default(),
		}
	}
}

impl<N: TreeConf> IterStackMut<N> {
	fn new() -> Self {
		IterStackMut {
			stack: Vec::new(),
			key: Default::default(),
		}
	}
}

impl<N: TreeConf> Tree<N> {
	/// Seek iteration following a giving key.
	pub fn seek_iter<'a>(&'a self, key: &'a [u8]) -> SeekIter<'a, N> {
		let dest_position = Position {
			index: key.len(),
			mask: MaskFor::<N::Radix>::FIRST,
		};
		self.seek_iter_at(key, dest_position)
	}

	/// Seek iteration following a giving node key, a postion on the key is used for unalingned
	/// radix.
	pub fn seek_iter_at<'a>(&'a self, key: &'a [u8], dest_position: PositionFor<N>) -> SeekIter<'a, N> {
		let stack = NodeStack::new();
		let reach_dest = false;
		let next = stack.descend(key, dest_position);
		SeekIter {
			tree: self,
			dest: key,
			dest_position,
			stack,
			reach_dest,
			next,
		}
	}

	/// Seek iteration following a giving key.
	pub fn seek_iter_no_cache<'a>(&'a self, key: &'a [u8]) -> SeekIterNoCache<'a, N> {
		let dest_position = Position {
			index: key.len(),
			mask: MaskFor::<N::Radix>::FIRST,
		};
		self.seek_iter_at_no_cache(key, dest_position)
	}

	/// Seek iteration following a giving node key, a postion on the key is used for unalingned
	/// radix.
	pub fn seek_iter_at_no_cache<'a>(&'a self, key: &'a [u8], dest_position: PositionFor<N>) -> SeekIterNoCache<'a, N> {
		let stack = NodeStackNoCache::new();
		let reach_dest = false;
		let next = stack.descend(key, dest_position);
		SeekIterNoCache {
			tree: self,
			dest: key,
			dest_position,
			stack,
			reach_dest,
			next,
		}
	}

	/// Seek nodes iterator over a given key with mutable access.
	pub fn seek_iter_mut<'a>(&'a mut self, key: &'a [u8]) -> SeekIterMut<'a, N> {
		let dest_position = Position {
			index: key.len(),
			mask: MaskFor::<N::Radix>::FIRST,
		};
		self.seek_iter_at_mut(key, dest_position)
	}

	/// Variant of `seek_iter_mut` for key using a unaligned radix.
	pub fn seek_iter_at_mut<'a>(&'a mut self, key: &'a [u8], dest_position: PositionFor<N>) -> SeekIterMut<'a, N> {
		let stack = NodeStackMut::new();
		let reach_dest = false;
		let next = stack.descend(key, dest_position);
		SeekIterMut {
			tree: self,
			dest: key,
			dest_position,
			stack,
			reach_dest,
			next,
		}
	}
}

macro_rules! no_ref {($seek_iter: expr) => {
	$seek_iter
}}
macro_rules! use_ref {($seek_iter: expr) => {
	&$seek_iter
}}

macro_rules! iter_impl {
	(
		$seek_iter: ident,
		$iter: ident,
		$item: ident,
		$next_node: ident,
		$value_iter: ident,
		$seek_value_iter: ident,
		$iter_stack: ident,
		$from_stack: ident,
		$from_stack2: ident,
		$use_ref: ident,
		$to_stack: ident,
		$to_stack2: ident,
		$as_ref: ident,
		$get_child: ident,
	) => {

impl<'a, N: TreeConf> $seek_iter<'a, N> {

	/// With middle we need to check the position and next element
	/// and act on stack to switch to iter.
	/// Return true if we need to continue iteration.
	fn middle_seek_to_iter(
		next: &Descent<N::Radix>,
		stack: &mut Vec<$item<'a, N>>,
		dest_key: &[u8],
		dest_position: PositionFor<N>,
	) -> bool {
		match next {
			Descent::Middle(position, _index) => {
				if position == &dest_position {
					// we are on the partial key, just remove stacked node.
					stack.pop();
				} else {
					let key_index = position.index::<N::Radix>(dest_key)
						.expect("Middle variant");
					debug_assert!(stack.len() > 0);
					let node_position = if stack.len() > 1 {
						stack[stack.len() - 2].0.next::<N::Radix>()
					} else {
						PositionFor::<N>::zero()
					};
					let index = $from_stack($use_ref!(stack[stack.len() - 1].1)).partial_index(node_position, *position)
						.expect("Middle variant");
					debug_assert!(index != key_index);
					if index > key_index {
						// need to return self
						stack.pop();
					} else {
						// return next from parent TODO duplicated with iter code
						let mut do_pop = true;
						while do_pop {
							stack.pop();
							if let Some(last) = stack.last_mut() {
								if let Some(next) = $from_stack($use_ref!(last.1)).get_next_child_index(Some(last.2)) {
									last.2 = next;
								} else {
									continue;
								}
							} else {
								// last pop, do not iterate
								return false;
							}
							do_pop = false;
						}
					}
				}
			},
			_ => (),
		}
		true
	}

	/// Node iterator from a seek iterator.
	/// This allow doing seek first then iterationg nodes
	/// with the same context.
	pub fn iter(mut self) -> $iter<'a, N> {
		let dest = self.dest;
		// corner case where seek iter skip a stack (alloc)
		if self.stack.stack.len() == 0 && self.dest.len() > 0 {
			if let Some(node) = self.tree.tree.$as_ref() {
				let zero = PositionFor::<N>::zero();
				self.stack.stack.push((zero, $to_stack(node)));
			}
		}

		let mut stack = self.stack.stack.into_iter().map(|(pos, node)| {
			let node_depth = $from_stack($use_ref!(node)).depth();
			let pos = pos.next_by::<N::Radix>(node_depth);
			let key = pos.index::<N::Radix>(dest)
				// out of dest we use the first child
				.unwrap_or_else(|| KeyIndexFor::<N>::zero());
			(pos, node, key)
		}).collect();

		let state = if Self::middle_seek_to_iter(
			&self.next,
			&mut stack,
			self.dest,
			self.dest_position,
		) {
			IterState::Started
		} else {
			IterState::Finished
		};

		$iter {
			tree: self.tree,
			stack: $iter_stack {
				stack,
				key: self.dest.into(),
			},
			state,
		}
	}

	/// Node iterator from a seek iterator.
	/// This differs from `iter` because the iteration
	/// will only happen on nodes starting with the prefix
	/// of the current node for the seek iterator.
	pub fn iter_prefix(mut self) -> $iter<'a, N> {
		let dest = self.dest;
		// corner case where seek iter skip a stack (alloc)
		if self.stack.stack.len() == 0 && self.dest.len() > 0 {
			if let Some(node) = self.tree.tree.$as_ref() {
				let zero = PositionFor::<N>::zero();
				self.stack.stack.push((zero, $to_stack(node)));
			}
		}

		let mut state = IterState::Started;
		match self.next {
			Descent::Middle(position, _index) => {
				if position == self.dest_position {
					self.stack.stack.pop();
					state = IterState::StartPrefix;
				} else {
					// nothing match prefix.
					state = IterState::Finished;
				}
			},
			Descent::Child(position, _index) => {
				// using index since we see a aligned key as dest_position
				if position.index < self.dest_position.index {
					// no child with prefix
					state = IterState::Finished;
				}
			},
			_ => (),
		}

		let stack = if state != IterState::Finished {
			self.stack.stack.pop().map(|(pos, node)| {
				let node_depth = $from_stack($use_ref!(node)).depth();
				let pos = pos.next_by::<N::Radix>(node_depth);
				let key = pos.index::<N::Radix>(dest)
					.unwrap_or_else(|| KeyIndexFor::<N>::zero());
				(pos, node, key)
			}).into_iter().collect()
		} else {
			Default::default()
		};

		$iter {
			tree: self.tree,
			stack: $iter_stack {
				stack,
				key: self.dest.into(),
			},
			state,
		}
	}

	/// Get iterator only on value from respective node iterator.
	pub fn value_iter(self) -> $seek_value_iter<'a, N> {
		$seek_value_iter(self)
	}

	fn next_node(&mut self) -> Option<$next_node<'a, N>> {
		if self.reach_dest {
			return None;
		}
		match self.next {
			Descent::Child(position, index) => {
				if let Some(parent) = self.stack.stack.last() {
					if let Some(child) = $from_stack($use_ref!(parent.1)).$get_child(index, &self.tree.backend) {
						let position = position.next::<N::Radix>();
						match child.descend(
							&self.dest,
							position,
							self.dest_position,
						) {
							next@Descent::Middle(..) => {
								self.next = next;
								self.reach_dest = true;
								// need to stack in order to convert to iter later.
								self.stack.stack.push((position, $to_stack2(child)));
								return None;
							},
							next@Descent::Match(..) => {
								self.next = next;
								self.reach_dest = true;
							},
							next@Descent::Child(..) => {
								self.next = next;
							},
						}
						self.stack.stack.push((position, $to_stack2(child)));
					} else {
						self.reach_dest = true;
						return None;
					}
				} else {
					// empty tree
					//		// TODO put ref in stack.
					if let Some(node) = self.tree.tree.$as_ref() {
						let zero = PositionFor::<N>::zero();
						match node.descend(
							&self.dest,
							zero,
							self.dest_position,
						) {
							next@Descent::Middle(..) => {
								self.next = next;
								return None;
							},
							Descent::Match(..) => {
								self.reach_dest = true;
							},
							next@Descent::Child(..) => {
								self.next = next;
							},
						}
						self.stack.stack.push((zero, $to_stack(node)));
					} else {
						self.reach_dest = true;
					}
				}
			},
			Descent::Middle(_position, _index) => {
				unreachable!();
			},
			Descent::Match(_position) => {
				unreachable!();
			},
		}
		self.stack.stack.last().map(|last| {
			(last.0, $from_stack2($use_ref!(last.1)))
		})
	}
}

impl<'a, N: TreeConf> $iter<'a, N> {
	/// Get value iterator from this node iterator.
	pub fn value_iter(self) -> $value_iter<'a, N> {
		$value_iter(self)
	}

	fn next_node(&mut self) -> Option<$next_node<'a, N>> {
		if self.state == IterState::Finished {
			return None;
		}
		let is_first_prefix = if self.state == IterState::StartPrefix {
			self.state = IterState::Started;
			true
		} else {
			false
		};
		let mut do_pop = false;
		loop {
			if do_pop {
				self.stack.stack.pop();
				if let Some(last) = self.stack.stack.last_mut() {
					// move cursor to next
					if let Some(next) = $from_stack($use_ref!(last.1)).get_next_child_index(Some(last.2)) {
						last.2 = next;
					} else {
						// try descend in next from parent
						continue;
					}
				} else {
					// last pop
					self.state = IterState::Finished;
					break;
				}
				do_pop = false;
			}
			if let Some(last) = self.stack.stack.last_mut() {
				let last_1 = $from_stack($use_ref!(last.1));
				// try descend
				if let Some(child) = last_1.$get_child(last.2, &self.tree.backend) {
					//let position = last.0.next::<N::Radix>();
					let position = last.0;
					position.set_index::<N::Radix>(&mut self.stack.key, last.2);
					let position = position.next::<N::Radix>();
					child.new_end(&mut self.stack.key, position);
					let position = position.next_by::<N::Radix>(child.depth());

					let first_key = child.get_next_child_index(None)
						.unwrap_or(KeyIndexFor::<N>::zero()); // TODO end iter instead of 0 useless query
					if is_first_prefix {
						self.stack.stack.pop(); // we don't want to include siblings in iteration.
					}
					self.stack.stack.push((position, child, first_key));
					break;
				}

				debug_assert!(!is_first_prefix); // prefix state only added after popping stack.

				// try descend in next
				if let Some(next) = $from_stack($use_ref!(last.1)).get_next_child_index(Some(last.2)) {
					last.2 = next;
				} else {
					// try descend in next from parent
					do_pop = true;
				}
			} else {
				// empty, this is start iteration
				if let Some(node) = self.tree.tree.$as_ref() {
					let zero = PositionFor::<N>::zero();
					let first_key = node.get_next_child_index(None)
						.unwrap_or(KeyIndexFor::<N>::zero());
					node.new_end(&mut self.stack.key, zero);
					let zero = zero.next_by::<N::Radix>(node.depth()); // could just use node prefix end and index
					self.stack.stack.push((zero, $to_stack(node), first_key));
				} else {
					self.state = IterState::Finished;
				}
				break;
			}
		}

		self.stack.stack.last_mut().map(|(p, n, _i)| (*p, $from_stack2($use_ref!(*n))))
	}
}


}}

type SeekIterItem<'a, N> = (PositionFor<N>, &'a Node<N>, KeyIndexFor<N>);
type NextNode<'a, N> = (PositionFor<N>, &'a Node<N>);
type SeekIterItemNoCache<'a, N> = (PositionFor<N>, Node<N>, KeyIndexFor<N>);
type NextNodeNoCache<'a, N> = (PositionFor<N>, Node<N>);
type SeekIterItemMut<'a, N> = (PositionFor<N>, *mut Node<N>, KeyIndexFor<N>);
type NextNodeMut<'a, N> = (PositionFor<N>, &'a mut Node<N>);
fn from_stack<'a, N: TreeConf>(s: &'a Node<N>) -> &'a Node<N> {
	s
}
fn to_stack<'a, N: TreeConf>(s: &'a Node<N>) -> &'a Node<N> {
	s
}
fn from_stack_no_cache<'a, N: TreeConf>(s: &Node<N>) -> &Node<N> {
	s
}
fn from_stack_no_cache_clone<'a, N: TreeConf>(s: &Node<N>) -> Node<N> {
	s.clone()
}
fn to_stack_no_cache<'a, N: TreeConf>(s: &'a Node<N>) -> Node<N> { // TODO try using directly node as param (using different as_ref function)
	s.clone()
}
fn to_stack_no_cache2<N: TreeConf>(s: Node<N>) -> Node<N> { // TODO try using directly node as param (using different as_ref function)
	s
}
fn unsafe_from_stack_mut<'a, N: TreeConf>(s: *mut Node<N>) -> &'a mut Node<N> {
	unsafe { s.as_mut().unwrap() }
}
fn to_stack_mut<'a, N: TreeConf>(s: &'a mut Node<N>) -> *mut Node<N> {
	s as *mut _
}

iter_impl!(
	SeekIter,
	Iter,
	SeekIterItem,
	NextNode,
	ValueIter,
	SeekValueIter,
	IterStack,
	from_stack,
	from_stack,
	no_ref,
	to_stack,
	to_stack,
	as_ref,
	get_child,
);

iter_impl!(
	SeekIterNoCache,
	IterNoCache,
	SeekIterItemNoCache,
	NextNodeNoCache,
	ValueIterNoCache,
	SeekValueIterNoCache,
	IterStackNoCache,
	from_stack_no_cache,
	from_stack_no_cache_clone,
	use_ref,
	to_stack_no_cache,
	to_stack_no_cache2,
	as_ref,
	get_child_no_cache,
);

iter_impl!(
	SeekIterMut,
	IterMut,
	SeekIterItemMut,
	NextNodeMut,
	ValueIterMut,
	SeekValueIterMut,
	IterStackMut,
	unsafe_from_stack_mut,
	unsafe_from_stack_mut,
	no_ref,
	to_stack_mut,
	to_stack_mut,
	as_mut,
	get_child_mut_deref,
);

/// Iterator on nodes that follows a given key (all nodes seeked
/// on the key path).
pub struct SeekIter<'a, N: TreeConf> {
	tree: &'a Tree<N>,
	dest: &'a [u8],
	dest_position: PositionFor<N>,
	// TODO seekiter could be lighter and avoid using stack,
	// just keep latest: a stack trait could be use.
	stack: NodeStack<'a, N>,
	// state for next iter, we calculate it before
	// just to store position with node prefix.
	// TODO this is actually always a child variant
	next: Descent<N::Radix>,
	// part of state for next iter, not an item of descent
	// to avoid
	reach_dest: bool,
}

/// Iterator on values seeked when fetching a given key.
pub struct SeekValueIter<'a, N: TreeConf>(SeekIter<'a, N>);

impl<'a, N: TreeConf> SeekValueIter<'a, N> {
	/// Get back node iterator.
	pub fn node_iter(self) -> SeekIter<'a, N> {
		self.0
	}
}

impl<'a, N: TreeConf> Iterator for SeekIter<'a, N> {
	type Item = (&'a [u8], PositionFor<N>, &'a Node<N>);

	fn next(&mut self) -> Option<Self::Item> {
		self.next_node().map(|(pos, node)| (self.dest, pos, node))
	}
}

impl<'a, N: TreeConf> Iterator for SeekValueIter<'a, N> {
	type Item = (&'a [u8], &'a N::Value);

	fn next(&mut self) -> Option<Self::Item> {
		loop {
			if let Some((key, pos, node)) = self.0.next() {
				if let Some(v) = node.value(&self.0.tree.backend) {
					let pos = pos.next_by::<N::Radix>(node.depth());
					return Some((&key[..pos.index], v))
				}
			} else {
				return None;
			}
		}
	}
}

pub struct SeekIterNoCache<'a, N: TreeConf> {
	tree: &'a Tree<N>,
	dest: &'a [u8],
	dest_position: PositionFor<N>,
	stack: NodeStackNoCache<N>,
	next: Descent<N::Radix>,
	reach_dest: bool,
}

/// Iterator on values seeked when fetching a given key.
pub struct SeekValueIterNoCache<'a, N: TreeConf>(SeekIterNoCache<'a, N>);

impl<'a, N: TreeConf> SeekValueIterNoCache<'a, N> {
	/// Get back node iterator.
	pub fn node_iter(self) -> SeekIterNoCache<'a, N> {
		self.0
	}
}

impl<'a, N: TreeConf> Iterator for SeekIterNoCache<'a, N> {
	type Item = (&'a [u8], PositionFor<N>, Node<N>);

	fn next(&mut self) -> Option<Self::Item> {
		self.next_node().map(|(pos, node)| (self.dest, pos, node))
	}
}

impl<'a, N: TreeConf> Iterator for SeekValueIterNoCache<'a, N> {
	type Item = (&'a [u8], N::Value);

	fn next(&mut self) -> Option<Self::Item> {
		loop {
			if let Some((key, pos, node)) = self.0.next() {
				if let Some(v) = node.value_no_cache(&self.0.tree.backend) {
					let pos = pos.next_by::<N::Radix>(node.depth());
					return Some((&key[..pos.index], v))
				}
			} else {
				return None;
			}
		}
	}
}

/// Mutable variant of seek iterator.
pub struct SeekIterMut<'a, N: TreeConf> {
	tree: &'a mut Tree<N>,
	dest: &'a [u8],
	dest_position: PositionFor<N>,
	// Here NodeStackMut will be used through unsafe
	// calls, but it should always be 'a with
	// content comming only form tree field.
	stack: NodeStackMut<N>,
	reach_dest: bool,
	next: Descent<N::Radix>,
}

pub struct SeekValueIterMut<'a, N: TreeConf>(SeekIterMut<'a, N>);

impl<'a, N: TreeConf> SeekValueIterMut<'a, N> {
	/// Get back node iterator.
	pub fn node_iter(self) -> SeekIterMut<'a, N> {
		self.0
	}
}

impl<'a, N: TreeConf> Iterator for SeekIterMut<'a, N> {
	type Item = (&'a [u8], PositionFor<N>, &'a mut Node<N>);

	fn next(&mut self) -> Option<Self::Item> {
		self.next_node().map(|(pos, node)| (self.dest, pos, node))
	}
}

impl<'a, N: TreeConf> Iterator for SeekValueIterMut<'a, N> {
	type Item = (&'a [u8], &'a mut N::Value);

	fn next(&mut self) -> Option<Self::Item> {
		loop {
			if let Some((key, pos, node)) = self.0.next() {
				let depth = node.depth();
				if let Some(v) = node.value_mut(&self.0.tree.backend) {
					let pos = pos.next_by::<N::Radix>(depth);
					return Some((&key[..pos.index], v))
				}
			} else {
				return None;
			}
		}
	}
}

/// Iterator over the nodes of a tree.
pub struct Iter<'a, N: TreeConf> {
	tree: &'a Tree<N>,
	stack: IterStack<'a, N>,
	state: IterState,
}

/// Iterator over the nodes of a tree.
pub struct IterNoCache<'a, N: TreeConf> {
	tree: &'a Tree<N>,
	stack: IterStackNoCache<N>,
	state: IterState,
}

/// Mutable aterator over the nodes of a tree.
pub struct IterMut<'a, N: TreeConf> {
	tree: &'a mut Tree<N>,
	stack: IterStackMut<N>,
	state: IterState,
}

/// Iterator over the values of a tree.
pub struct ValueIter<'a, N: TreeConf>(Iter<'a, N>);

/// Iterator over the values of a tree.
pub struct ValueIterNoCache<'a, N: TreeConf>(IterNoCache<'a, N>);

/// Mutable iterator over the values of a tree.
pub struct ValueIterMut<'a, N: TreeConf>(IterMut<'a, N>);

impl<N: TreeConf> Tree<N> {
	/// Get node iterator for this tree.
	pub fn iter<'a>(&'a self) -> Iter<'a, N> {
		Iter {
			tree: self,
			stack: IterStack::new(),
			state: IterState::Started,
		}
	}

	/// Get node iterator for this tree.
	pub fn iter_no_cache<'a>(&'a self) -> IterNoCache<'a, N> {
		IterNoCache {
			tree: self,
			stack: IterStackNoCache::new(),
			state: IterState::Started,
		}
	}

	/// Get mutable node iterator for this tree.
	pub fn iter_mut<'a>(&'a mut self) -> IterMut<'a, N> {
		IterMut {
			tree: self,
			stack: IterStackMut::new(),
			state: IterState::Started,
		}
	}
}

impl<'a, N: TreeConf> Iterator for Iter<'a, N> {
	// TODO key as slice, but usual lifetime issue.
	// TODO at leas use a stack type for key (smallvec).
	type Item = (Key, PositionFor<N>, &'a Node<N>);

	fn next(&mut self) -> Option<Self::Item> {
		self.next_node().map(|(p, n)| (self.stack.key.clone(), p, n))
	}
}

impl<'a, N: TreeConf> Iterator for IterNoCache<'a, N> {
	// TODO key as slice, but usual lifetime issue.
	// TODO at leas use a stack type for key (smallvec).
	type Item = (Key, PositionFor<N>, Node<N>);

	fn next(&mut self) -> Option<Self::Item> {
		self.next_node().map(|(p, n)| (self.stack.key.clone(), p, n))
	}
}

impl<'a, N: TreeConf> Iterator for IterMut<'a, N> {
	// TODO key as slice, but usual lifetime issue.
	// TODO at leas use a stack type for key (smallvec).
	type Item = (Key, PositionFor<N>, &'a mut Node<N>);

	fn next(&mut self) -> Option<Self::Item> {
		self.next_node().map(|(p, n)| (self.stack.key.clone(), p, n))
	}
}

impl<'a, N: TreeConf> Iterator for ValueIter<'a, N> {
	// TODO key as slice, but usual lifetime issue.
	// TODO at leas use a stack type for key (smallvec).
	type Item = (Key, &'a N::Value);

	fn next(&mut self) -> Option<Self::Item> {
		loop {
			if let Some((mut key, pos, node)) = self.0.next() {
				if let Some(v) = node.value(&self.0.tree.backend) {
					key.truncate(pos.index);
					return Some((key, v))
				}
			} else {
				return None;
			}
		}
	}
}

impl<'a, N: TreeConf> ValueIter<'a, N> {
	/// Get back node iterator.
	pub fn node_iter(self) -> Iter<'a, N> {
		self.0
	}
}

impl<'a, N: TreeConf> Iterator for ValueIterNoCache<'a, N> {
	// TODO key as slice, but usual lifetime issue.
	// TODO at leas use a stack type for key (smallvec).
	type Item = (Key, N::Value);

	fn next(&mut self) -> Option<Self::Item> {
		loop {
			if let Some((mut key, pos, node)) = self.0.next() {
				if let Some(v) = node.value_no_cache(&self.0.tree.backend) {
					key.truncate(pos.index);
					return Some((key, v))
				}
			} else {
				return None;
			}
		}
	}
}

impl<'a, N: TreeConf> ValueIterNoCache<'a, N> {
	/// Get back node iterator.
	pub fn node_iter(self) -> IterNoCache<'a, N> {
		self.0
	}
}


impl<'a, N: TreeConf> Iterator for ValueIterMut<'a, N> {
	// TODO key as slice, but usual lifetime issue (key being
	// build from multiple prefixes).
	// TODO return stack of keys that would require merge?
	// TODO at leas use a stack type for key (smallvec).
	type Item = (Key, &'a mut N::Value);

	fn next(&mut self) -> Option<Self::Item> {
		loop {
			if let Some((mut key, pos, node)) = self.0.next() {
				if let Some(v) = node.value_mut(&self.0.tree.backend) {
					key.truncate(pos.index);
					return Some((key, v))
				}
			} else {
				return None;
			}
		}
	}
}

/// Owned tree iterator.
///
/// Should be use when we want to keep iterator context
/// but don't want to manage associated lifetime.
///
/// This is an unsafe wrapper over `ValueIterMut`
/// (we use mutable version for backend supports).
///
/// Owned iterator do not modify the inner tree,
/// and iterated content is always cloned.
pub struct OwnedIter<N: TreeConf + 'static> {
	inner: Tree<N>,
	iter: ValueIterNoCache<'static, N>,
}

/// Seek variant of OwnedIter.
pub struct OwnedIterSeek<N: TreeConf + 'static> {
	inner: Tree<N>,
	prefix: Key,
	iter: SeekValueIterNoCache<'static, N>,
}

impl<N: TreeConf + 'static> OwnedIter<N> {
	/// After iteration, we can use back the
	/// tree content back.
	pub fn extract_inner(self) -> Tree<N> {
		let OwnedIter {
			inner,
			iter,
		} = self;
		drop(iter);
		inner
	}
}

impl<N: TreeConf + 'static> OwnedIterSeek<N> {
	/// After iteration, we can use back the
	/// tree content back.
	pub fn extract_inner(self) -> Tree<N> {
		let OwnedIterSeek {
			inner,
			iter,
			prefix,
		} = self;
		drop(iter);
		drop(prefix);
		inner
	}
}

impl<N: TreeConf> Tree<N> {
	/// Use tree as a value iterator.
	pub fn owned_iter(self) -> OwnedIter<N> {
		let self_ptr = &self as *const Self;
		let unsafe_ptr: &'static Self = unsafe { self_ptr.as_ref().unwrap() };
		let iter = unsafe_ptr.iter_no_cache().value_iter();
		OwnedIter {
			inner: self,
			iter,
		}
	}

	/// Use tree as a value iterator for keys starting with a given prefix.
	pub fn owned_prefix_iter(self, prefix: &[u8]) -> OwnedIter<N> {
		let self_ptr = &self as *const Self;
		let unsafe_ptr: &'static Self = unsafe { self_ptr.as_ref().unwrap() };
		let static_prefix = prefix as *const [u8];
		let static_prefix: &'static [u8] = unsafe { static_prefix.as_ref().unwrap() };
		let mut seek_iter = unsafe_ptr.seek_iter_no_cache(static_prefix);
		while seek_iter.next().is_some() { }
		let iter = seek_iter.iter_prefix().value_iter();
		OwnedIter {
			inner: self,
			iter,
		}
	}

	/// Use tree as a value iterator for keys starting with a given key.
	pub fn owned_iter_from(self, start_key: &[u8]) -> OwnedIter<N> {
		let self_ptr = &self as *const Self;
		let unsafe_ptr: &'static Self = unsafe { self_ptr.as_ref().unwrap() };
		let static_prefix = start_key as *const [u8];
		let static_prefix: &'static [u8] = unsafe { static_prefix.as_ref().unwrap() };
		let mut seek_iter = unsafe_ptr.seek_iter_no_cache(static_prefix);
		while seek_iter.next().is_some() { }
		let iter = seek_iter.iter().value_iter();
		OwnedIter {
			inner: self,
			iter,
		}
	}

	/// Use tree as a seek value iterator.
	pub fn owned_iter_seek(self, prefix: Key) -> OwnedIterSeek<N> {
		let self_ptr = &self as *const Self;
		let unsafe_ptr: &'static Self = unsafe { self_ptr.as_ref().unwrap() };
		let prefix_ptr = prefix.as_slice() as *const [u8];
		let unsafe_prefix_ptr: &'static [u8] = unsafe { prefix_ptr.as_ref().unwrap() };
		let iter = unsafe_ptr.seek_iter_no_cache(unsafe_prefix_ptr).value_iter();
		OwnedIterSeek {
			inner: self,
			prefix,
			iter,
		}
	}
}

impl<N: TreeConf + 'static> Iterator for OwnedIter<N> {
	type Item = (Key, N::Value);

	fn next(&mut self) -> Option<Self::Item> {
		self.iter.next()
	}
}

impl<N: TreeConf + 'static> Iterator for OwnedIterSeek<N> {
	type Item = (Key, N::Value);

	fn next(&mut self) -> Option<Self::Item> {
		self.iter.next().map(|(key, value)| (key.to_vec(), value))
	}
}

impl<N: TreeConf> OwnedIterSeek<N> {
	/// Node iterator from a seek iterator.
	/// This allow doing seek first then iterationg nodes
	/// with the same context.
	pub fn iter(self) -> OwnedIter<N> {
		OwnedIter {
			inner: self.inner,
			iter: self.iter.0.iter().value_iter(),
		}
	}

	/// Node iterator from a seek iterator.
	/// This differs from `iter` because the iteration
	/// will only happen on nodes starting with the prefix
	/// of the current node for the seek iterator.
	pub fn iter_prefix(self) -> OwnedIter<N> {
		OwnedIter {
			inner: self.inner,
			iter: self.iter.0.iter_prefix().value_iter(),
		}
	}
}
