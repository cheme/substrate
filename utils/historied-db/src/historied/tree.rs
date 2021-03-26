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

//! Tree historied data historied db implementations.

// TODO remove "previous code" expect.

use super::{HistoriedValue, Data, DataMut, DataRef, DataRefMut,
	DataSlices, DataSliceRanges, UpdateResult, Value, ValueRef,
	DataBasis, IndexedDataBasis,
	aggregate::{Sum as DataSum, SummableValue}};
#[cfg(feature = "indexed-access")]
use super::IndexedData;
use crate::backend::{LinearStorage, LinearStorageRange, LinearStorageSlice, LinearStorageMem};
use crate::historied::linear::{Linear, LinearState, LinearGC, aggregate::Sum as LinearSum};
use crate::management::tree::{ForkPlan, TreeStateGc, DeltaTreeStateGc, MultipleGc, MultipleMigrate};
use sp_std::vec::Vec;
use sp_std::marker::PhantomData;
use crate::Latest;
use crate::{Context, ContextBuilder, InitFrom, DecodeWithContext, Trigger};
use codec::{Encode, Input};
use derivative::Derivative;
use core::default::Default;

// TODO for not in memory we need some direct or indexed api, returning value
// and the info if there can be lower value index (not just a direct index).
// -> then similar to those reverse iteration with possible early exit.
// -> Also need to attach some location index (see enumerate use here)

// strategy such as in linear are getting too complex for tree, just using
// macros to remove duplicated code.

// Common code to get from tree.
// Lookup first linear storage in parallel (until incorrect state ordering).
// Call second linear historied value afterward.
macro_rules! tree_get {
	($fn_name: ident, $return_type: ty, $apply_on_branch: ident, $value_query: expr, $post_process: expr, $b: lifetime) => {
	fn $fn_name<$b>(&'a self, at: &<Self as DataBasis>::S) -> Option<$return_type> {
		// note that we expect branch index to be linearily set
		// along a branch (no state containing unordered branch_index
		// and no history containing unorderd branch_index).
		let mut next_branch_index = self.branches.last();
		let mut final_result = None;
		for (state_branch_range, state_branch_index) in at.iter() {
			while let Some(branch_ix) = next_branch_index {
				let branch_index = &self.branches.get_state(branch_ix);
				if branch_index < &state_branch_index {
					break;
				} else if branch_index == &state_branch_index {
					// TODO add a lower bound check (maybe debug_assert it only).
					let mut upper_bound = state_branch_range.end.clone();
					upper_bound -= BI::one();
					self.branches.$apply_on_branch(branch_ix, |value| {
						let branch = value.value;
						if let Some(result) = $value_query(branch, &upper_bound) {
							final_result = Some($post_process(result, branch, branch_ix));
						}
					});
					if final_result.is_some() {
						return final_result;
					}
				}
				next_branch_index = self.branches.previous_index(branch_ix);
			}
		}

		// composite part.
		while let Some(branch_ix) = next_branch_index {
			let branch_index = &self.branches.get_state(branch_ix);
			if branch_index <= &at.composite_treshold.0 {
				self.branches.$apply_on_branch(branch_ix, |value| {
					let branch = value.value;
					if let Some(result) = $value_query(branch, &at.composite_treshold.1) {
						final_result = Some($post_process(result, branch, branch_ix));
					}
				});
				if final_result.is_some() {
					return final_result;
				}
			}
			next_branch_index = self.branches.previous_index(branch_ix);
		}
	
		final_result
	}
	}
}

#[derive(Derivative, Debug, Clone, Encode)]
#[derivative(PartialEq(bound="D: PartialEq"))]
pub struct Tree<I, BI, V, D: Context, BD: Context> {
	branches: D,
	#[codec(skip)]
	#[derivative(PartialEq="ignore" )]
	init: D::Context,
	#[codec(skip)]
	#[derivative(PartialEq="ignore" )]
	init_child: BD::Context,
	#[codec(skip)]
	_ph: PhantomData<(I, BI, V, BD)>,
}

impl<I, BI, V, D, BD> DecodeWithContext for Tree<I, BI, V, D, BD>
	where
		D: DecodeWithContext,
		BD: DecodeWithContext,
{
	fn decode_with_context<IN: Input>(input: &mut IN, init: &Self::Context) -> Option<Self> {
		D::decode_with_context(input, &init.0).map(|branches|
			Tree {
				branches,
				init: init.0.clone(),
				init_child: init.1.clone(),
				_ph: PhantomData,
			}
		)
	}
}

impl<I, BI, V, D: Context, BD: Context> Context for Tree<I, BI, V, D, BD> {
	type Context = (D::Context, BD::Context);
}

impl<I, BI, V, D: Context + Trigger, BD: Context> Trigger for Tree<I, BI, V, D, BD> {
	const TRIGGER: bool = <D as Trigger>::TRIGGER;

	fn trigger_flush(&mut self) {
		if Self::TRIGGER {
			self.branches.trigger_flush();
		}
	}
}

impl<I, BI, V, D: InitFrom, BD: InitFrom> InitFrom for Tree<I, BI, V, D, BD> {
	fn init_from(init: Self::Context) -> Self {
		Tree {
			branches: D::init_from(init.0.clone()),
			init: init.0,
			init_child: init.1,
			_ph: PhantomData,
		}
	}
}

type Branch<I, BI, V, BD> = HistoriedValue<Linear<V, BI, BD>, I>;

impl<I, BI, V, BD> Branch<I, BI, V, BD>
	where
		I: Clone + Encode,
		BI: LinearState,
		V: Value + Clone + Eq,
		BD: LinearStorage<V::Storage, BI>,
{
	pub fn new(value: V, state: &(I, BI), init: &BD::Context) -> Self {
		let (branch_index, index) = state.clone();
		let index = Latest::unchecked_latest(index); // TODO cast ptr?
		let init = if BD::Context::USE_INDEXES {
			let index = state.0.encode(); // TODO force compact encode?
			// parent index set at build.
			init.with_indexes(&[], index.as_slice())
		} else {
			init.clone()
		};
		let history = Linear::new(value, &index, init);
		Branch {
			state: branch_index,
			value: history,
		}
	}
}

impl<I, BI, V, D, BD> DataBasis for Tree<I, BI, V, D, BD>
	where
		I: Default + Ord + Clone,
		BI: LinearState,
		V: Value + Clone,
		D: LinearStorage<Linear<V, BI, BD>, I>, // TODO rewrite to be linear storage of BD only.
		BD: LinearStorage<V::Storage, BI>,
{
	type S = ForkPlan<I, BI>;
	type Index = (I, BI);

	fn contains(&self, at: &Self::S) -> bool {
		self.get(at).is_some() // TODO avoid clone??
	}

	fn is_empty(&self) -> bool {
		// This implies empty branch get clean correctly.
		self.branches.len() == 0
	}
}

impl<I, BI, V, D, BD> IndexedDataBasis for Tree<I, BI, V, D, BD>
	where
		I: Default + Ord + Clone,
		BI: LinearState,
		V: Value + Clone,
		D: LinearStorage<Linear<V, BI, BD>, I>, // TODO rewrite to be linear storage of BD only.
		BD: LinearStorage<V::Storage, BI>,
{
	type I = (D::Index, BD::Index);
	// Not really used, but it would make sense to implement variants with get_ref.
	tree_get!(index, Self::I, apply_on, |b: &Linear<V, BI, BD>, ix| b.index(ix), |r, _, ix| (ix, r), 'a);
}

#[cfg(feature = "indexed-access")]
impl<I, BI, V, D, BD> IndexedData<V> for Tree<I, BI, V, D, BD>
	where
		I: Default + Ord + Clone,
		BI: LinearState,
		V: Value + Clone,
		D: LinearStorage<Linear<V, BI, BD>, I>, // TODO rewrite to be linear storage of BD only.
		BD: LinearStorage<V::Storage, BI>,
{
	fn get_by_internal_index(&self, at: Self::I) -> V {
		let mut result = None;
		self.branches.apply_on(at.0, |branch| {
			result = Some(branch.value.get_by_internal_index(at.1));
		});
		result.expect("apply_on panic on missing indexes")
	}
}

impl<I, BI, V, D, BD> Data<V> for Tree<I, BI, V, D, BD>
	where
		I: Default + Ord + Clone,
		BI: LinearState,
		V: Value + Clone,
		D: LinearStorage<Linear<V, BI, BD>, I>, // TODO rewrite to be linear storage of BD only.
		BD: LinearStorage<V::Storage, BI>,
{
	tree_get!(get, V, apply_on, |b: &Linear<V, BI, BD>, ix| b.get(ix), |r, _, _| r, 'a);
}

impl<I, BI, V, D, BD> DataRef<V> for Tree<I, BI, V, D, BD>
	where
		I: Default + Ord + Clone,
		BI: LinearState,
		V: ValueRef + Clone,
		D: for<'a> LinearStorageMem<'a, Linear<V, BI, BD>, I>,
		BD: for<'a> LinearStorageMem<'a, V::Storage, BI>,
{
	tree_get!(get_ref, &'a V, apply_on_ref, |b: &'a Linear<V, BI, BD>, ix| b.get_ref(ix), |r, _, _| r, 'a);
}

impl<I, BI, V, D, BD> DataMut<V> for Tree<I, BI, V, D, BD>
	where
		I: Default + Ord + Clone + Encode,
		BI: LinearState,
		V: Value + Clone + Eq,
		D: LinearStorage<Linear<V, BI, BD>, I>,
		BD: LinearStorage<V::Storage, BI> + Trigger,
{
	type SE = Latest<(I, BI)>;
	type GC = MultipleGc<I, BI>;
	type Migrate = MultipleMigrate<I, BI>;

	fn new(value: V, at: &Self::SE, init: Self::Context) -> Self {
		let mut v = D::init_from(init.0.clone());
		v.push(Branch::new(value, at.latest(), &init.1));
		Tree {
			branches: v,
			init: init.0,
			init_child: init.1,
			_ph: PhantomData,
		}
	}

	fn set(&mut self, value: V, at: &Self::SE) -> UpdateResult<()> {
		// Warn dup code, can be merge if change set to return previ value: with
		// ref refact will be costless
		let (branch_index, index) = at.latest();
		let mut insert_at = None;
		for branch_ix in self.branches.rev_index_iter() {
			let iter_branch_index = self.branches.get_state(branch_ix);
			if &iter_branch_index == branch_index {
				let index = Latest::unchecked_latest(index.clone());
				let mut result = UpdateResult::Unchanged;
				self.branches.apply_on_mut(branch_ix, |branch| {
					result = branch.value.set(value, &index);
					matches!(result, UpdateResult::Changed(_))
				});
				return match result {
					UpdateResult::Changed(_) => {
						UpdateResult::Changed(())
					},
					UpdateResult::Cleared(_) => {
						self.remove_branch(branch_ix);
						if self.branches.len() == 0 {
							UpdateResult::Cleared(())
						} else {
							UpdateResult::Changed(())
						}
					},
					UpdateResult::Unchanged => UpdateResult::Unchanged,
				}
			}
			if &iter_branch_index < branch_index {
				break;
			}
			insert_at = Some(branch_ix);
		}
		let branch = Branch::new(value, at.latest(), &self.init_child);
		if let Some(index) = insert_at {
			self.branches.insert(index, branch);
		} else {
			self.branches.push(branch);
		}
		UpdateResult::Changed(())
	}

	fn discard(&mut self, at: &Self::SE) -> UpdateResult<Option<V>> {
		let (branch_index, index) = at.latest();
		for branch_ix in self.branches.rev_index_iter() {
			let iter_branch_index = self.branches.get_state(branch_ix);
			if &iter_branch_index == branch_index {
				let index = Latest::unchecked_latest(index.clone());
				let mut result = UpdateResult::Unchanged;
				self.branches.apply_on_mut(branch_ix, |branch| {
					result = branch.value.discard(&index);
					matches!(result, UpdateResult::Changed(_))
				});
				return match result {
					UpdateResult::Cleared(v) => {
						self.remove_branch(branch_ix);
						if self.branches.len() == 0 {
							UpdateResult::Cleared(v)
						} else {
							UpdateResult::Changed(v)
						}
					},
					result => result,
				};
			}
			if &iter_branch_index < branch_index {
				break;
			}
		}

		UpdateResult::Unchanged
	}

	fn gc(&mut self, gc: &Self::GC) -> UpdateResult<()> {
		match gc {
			MultipleGc::Journaled(gc) => self.journaled_gc(gc),
			MultipleGc::State(gc) => self.state_gc(gc),
		}
	}

	fn is_in_migrate((index, linear_index) : &Self::Index, gc: &Self::Migrate) -> bool {
		match gc {
			MultipleMigrate::Noops => (),
			MultipleMigrate::JournalGc(gc) => {
				if let Some(new_start) = gc.pruning_treshold.as_ref() {
					if linear_index <= &new_start {
						return true;
					}
				}
				if let Some(br) = gc.storage.get(&index) {
					return if let Some(bi) = br.0.as_ref() {
						bi <= linear_index
					} else {
						true
					};
				}
			},
			MultipleMigrate::Rewrite(_gc) => {
				unimplemented!()
			},
		}
		false
	}

	fn migrate(&mut self, mig: &Self::Migrate) -> UpdateResult<()> {
		let mut result = UpdateResult::Unchanged;

		match mig {
			MultipleMigrate::JournalGc(gc) => {
				result = self.journaled_gc(gc);
				if let UpdateResult::Cleared(()) = result {
					return UpdateResult::Cleared(());
				}
				let mut new_branch: Option<Branch<I, BI, V, BD>> = None;
				let mut i = 0;
				// merge all less than composite treshold in composite treshold index branch.
				loop {
					if let Some(index) = self.branches.lookup(i) {
						// TODO this does a copy of the full branch: should be rewrite
						// to use apply_on_mut.
						let mut branch = self.branches.get(index);
						if branch.state <= gc.composite_treshold.0 {
							if let Some(new_branch) = new_branch.as_mut() {
								for i in 0.. {
									if let Some(h) = branch.value.storage().lookup(i) {
										let h = branch.value.storage().get(h);
										new_branch.value.storage_mut().push(h);
									} else {
										break;
									}
								}
							} else {
								branch.state = gc.composite_treshold.0.clone();
								new_branch = Some(branch);
							}
						} else {
							break;
						}
					} else {
						break;
					}
					i += 1;
				}
				if let Some(new_branch) = new_branch {
					if i == self.branches.len() {
						self.branches.clear();
						self.branches.push(new_branch);
					} else {
						self.truncate_branches_until(i);
						self.branches.insert_lookup(0, new_branch);
					}
				}
			},
			MultipleMigrate::Rewrite(_gc) => unimplemented!(),
			MultipleMigrate::Noops => (),
		}

		if let UpdateResult::Changed(()) = result {
			if self.branches.len() == 0 {
				result = UpdateResult::Cleared(());
			}
		}
		result
	}
}

impl<I, BI, V, D, BD> Tree<I, BI, V, D, BD>
	where
		I: Default + Ord + Clone,
		BI: LinearState,
		V: Value + Clone + Eq,
		D: LinearStorage<Linear<V, BI, BD>, I>,
		BD: LinearStorage<V::Storage, BI> + Trigger,
{
	fn state_gc(&mut self, gc: &TreeStateGc<I, BI>) -> UpdateResult<()> {
		let mut result = UpdateResult::Unchanged;
		let start_history = &gc.pruning_treshold;
		let mut gc_iter = gc.storage.iter().rev();
		let mut next_branch_index = self.branches.last();
	
		let mut o_gc = gc_iter.next();
		let mut o_branch = next_branch_index.map(|i| (i, self.branches.get_state(i)));
		while let (Some(gc), Some((index, branch_index))) = (o_gc.as_ref(), o_branch.as_ref()) {
			let index = *index;
			next_branch_index = self.branches.previous_index(index);
			if gc.0 == branch_index {
				let start = gc.1.range().start.clone();
				let end = gc.1.range().end.clone();
				let start = start_history.as_ref().and_then(|start_history| if &start < start_history {
					Some(start_history.clone())
				} else {
					None
				}).unwrap_or(start);
				let mut gc = LinearGC {
					new_start: Some(start),
					new_end:  Some(end),
				};

				self.branches.apply_on_mut(index, |branch| {
					match branch.value.gc(&mut gc) {
						UpdateResult::Unchanged => false,
						UpdateResult::Changed(_) => {
							result = UpdateResult::Changed(());
							true
						},
						UpdateResult::Cleared(_) => true,
					}
				});
				if matches!(result, UpdateResult::Cleared(_)) {
					self.remove_branch(index);
					result = UpdateResult::Changed(());
				}

				o_gc = gc_iter.next();

				o_branch = next_branch_index.map(|i| (i, self.branches.get_state(i)));
			} else if gc.0 < &branch_index {
				self.remove_branch(index);
				result = UpdateResult::Changed(());
				o_branch = next_branch_index.map(|i| (i, self.branches.get_state(i)));
			} else {
				o_gc = gc_iter.next();
			}
		}

		if let UpdateResult::Changed(()) = result {
			if self.branches.len() == 0 {
				result = UpdateResult::Cleared(());
			}
		}

		result
	}

	fn journaled_gc(&mut self, gc: &DeltaTreeStateGc<I, BI>) -> UpdateResult<()> {
		// for all branch check if in deleted.
		// Also apply new start on all.
		let mut result = UpdateResult::Unchanged;
		let start_history = gc.pruning_treshold.as_ref();
		let mut first_new_start = false;
		let mut next_branch_index = self.branches.last();
		while let Some(branch_ix) = next_branch_index {
			// TODO this involve a full branch copy, rewrite that using
			// apply_on_mut for performance
			let mut branch = self.branches.get(branch_ix);
			let new_start = if branch.state <= gc.composite_treshold.0 {
				match start_history.as_ref() {
					None => None,
					Some(n_start) => {
						if first_new_start {
							self.remove_branch(branch_ix);
							result = UpdateResult::Changed(());
							next_branch_index = self.branches.previous_index(branch_ix);
							continue;
						} else {
							if let Some(b) = branch.value.storage().lookup(0) {
								let b = branch.value.storage().get(b);
								if &b.state < n_start {
									first_new_start = true;
								}
							}
							start_history.cloned()
						}
					},
				}
			} else {
				None
			};

			if let Some(mut gc) = if let Some(change) = gc.storage.get(&branch.state) {
				if change.0.is_none() {
					self.remove_branch(branch_ix);
					result = UpdateResult::Changed(());
					None
				} else {
					Some(LinearGC {
						new_start,
						new_end: change.0.clone(),
					})
				}
			} else {
				if first_new_start {
					Some(LinearGC {
						new_start,
						new_end: None,
					})
				} else {
					None
				}
			} {
				match branch.value.gc(&mut gc) {
					UpdateResult::Unchanged => (),
						UpdateResult::Changed(_) => { 
						self.branches.emplace(branch_ix, branch);
						result = UpdateResult::Changed(());
					},
					UpdateResult::Cleared(_) => {
						self.remove_branch(branch_ix);
						result = UpdateResult::Changed(());
					}
				}
			}
			next_branch_index = self.branches.previous_index(branch_ix);
		}

		if let UpdateResult::Changed(()) = result {
			if self.branches.len() == 0 {
				result = UpdateResult::Cleared(());
			}
		}

		result
	}

	fn trigger_clear_branch(&mut self, branch_ix: D::Index) {
		// TODO have variant of remove that return old value.
		self.branches.apply_on_mut(branch_ix, |branch| {
			branch.value.storage_mut().clear();
			branch.value.trigger_flush();
			true
		});
	}

	// any removal of branch need to trigger its old value.
	fn remove_branch(&mut self, branch_ix: D::Index) {
		if BD::TRIGGER {
			self.trigger_clear_branch(branch_ix);
		}
		self.branches.remove(branch_ix);
	}

	// trigger when we truncate or truncate_until.
	// Warning all indexes are inclusive
	fn truncate_branches_until(&mut self, end: usize) {
		if BD::TRIGGER {
			let nb = end;
			if nb > 0 {
				let mut index = self.branches.lookup(end - 1);
				for _ in 0..nb {
					if let Some(branch_index) = index {
						self.trigger_clear_branch(branch_index);
						index = self.branches.previous_index(branch_index);
					} else {
						break;
					}
				}
			}
		}
		self.branches.truncate_until(end);
	}

	/// Iterate on history content, this expose external
	/// types directly, see 'export_to_linear' implementation
	/// for usage.
	/// This is doing a backward iteration, and only enter branch
	/// Iterate on history content, this expose internal type directly. 
	pub fn map_backward(
		&self,
		mut filter: impl FnMut(I, D::Index, &D) -> bool,
	) {
		for handle in self.branches.rev_index_iter() {
			let state = self.branches.get_state(handle);
			if filter(state, handle, &self.branches) {
				break
			}
		}
	}

	/// Export a given tree value to linear history, given a read query plan.
	pub fn export_to_linear<V2, BO>	(
		&self,
		filter: ForkPlan<I, BI>, // Self::S
		include_all_treshold_value: bool,
		include_treshold_value: bool,
		dest: &mut crate::historied::linear::Linear<V2, BI, BO>,
		need_prev: bool, 
		map_value: impl Fn(Option<&V>, V) -> V2
	)	where
		V2: Value + Clone + Eq,
		BO: LinearStorage<V2::Storage, BI>,
	{
		let mut accu = Vec::new();
		let accu = &mut accu;
		let mut iter_forkplan = filter.iter();
		let mut fork_plan_head = iter_forkplan.next();
		let fork_plan_head = &mut fork_plan_head;
		let composite_treshold = &filter.composite_treshold.1;
		self.map_backward(|index, handle, branches| {
			while let Some((branch_range, branch_index)) = fork_plan_head.as_ref() {
				if &index > branch_index {
					return false;
				} else if &index == branch_index {
					branches.apply_on(handle, |branch| {
						branch.value.map_backward(|index, handle, branch| {
							if index < branch_range.end {
								if index >= branch_range.start {
									accu.push(branch.get(handle));
								} else {
									return true;
								}
							}
							false
						});
					});
					return false;
				} else {
					*fork_plan_head = iter_forkplan.next();
				}
			}

			if !include_all_treshold_value {
				if include_treshold_value {
					branches.apply_on(handle, |branch| {
						branch.value.map_backward(|index, handle, branch| {
							if &index < composite_treshold {
								accu.push(branch.get(handle));
								return true;
							}
							false
						});
					});
					return true;
				} else {
					return true;
				}
			}

			branches.apply_on(handle, |branch| {
				branch.value.map_backward(|index, handle, branch| {
					if &index < composite_treshold {
						accu.push(branch.get(handle));
					}
					false
				});
			});

			false
		});

		let mut prev = None;
		while let Some(value) = accu.pop() {
			let v = V::from_storage(value.value);
			let prev2 = need_prev.then(|| v.clone());
			let v2 = map_value(prev.as_ref(), v);
			prev = prev2;
			assert!(matches!(dest.set(v2, &Latest(value.state)), UpdateResult::Changed(..)));
		}
	}

	/// Export a given tree value to another tree value, given a read query plan.
	/// If needed to filter some content, one can use 'gc' on destination tree.
	pub fn export_to_tree<V2, DO, BO>	(
		&self,
		dest: &mut Tree<I, BI, V2, DO, BO>,
		need_prev: bool, 
		mut map_value: impl FnMut(Option<&V>, &(I, BI), V) -> V2
	)	where
		V2: Value + Clone + Eq,
		DO: LinearStorage<Linear<V2, BI, BO>, I>,
		BO: LinearStorage<V2::Storage, BI>,
		BO: Trigger,
		I: Encode,
	{
		let mut accu = Vec::new();
		let accu = &mut accu;
		self.map_backward(|index_br, handle, branches| {
			branches.apply_on(handle, |branch| {
				branch.value.map_backward(|index, handle, branch| {
					accu.push((index_br.clone(), index, V::from_storage(branch.get(handle).value)));
					false
				});
			});
			false
		});
		let mut prev = None;
		let mut prev_state: Option<(I, BI)> = None;
		while let Some(value) = accu.pop() {
			let state = (value.0, value.1);
			let value = value.2;
			let is_parent = prev_state.as_ref().map(|prev_state| {
				// no check for between branch, would require
				// management and backward query of dest to get previous
				// value (forkplan recalc being anti efficient there).
				prev_state.0 == state.0
			}).unwrap_or(false);
			let prev2 = need_prev.then(|| value.clone());
			prev_state = need_prev.then(|| state.clone());
			let v2 = map_value(is_parent.then(|| prev.as_ref()).flatten(), &state, value);
			prev = prev2;

			assert!(matches!(dest.set(v2, &Latest(state)), UpdateResult::Changed(..)));
		}
	}

	/// Temporary export with management backed parent resolution.
	/// Note that management usage is done in an antipattern way
	/// so resolution of query plan is involving redundant accesses,
	/// and access to previous value also involve redundant backward
	/// iteration.
	pub fn export_to_tree_mgmt<V2, DO, BO, H, M>	(
		&self,
		dest: &mut Tree<I, BI, V2, DO, BO>,
		mgmt: &mut M,
		map_value: impl Fn(Option<&V>, &(I, BI), V) -> V2
	)	where
		V2: Value + Clone + Eq,
		DO: LinearStorage<Linear<V2, BI, BO>, I>,
		BO: LinearStorage<V2::Storage, BI>,
		BO: Trigger,
		I: Encode,
		M: crate::management::Management<H, Index = (I, BI), S = ForkPlan<I, BI>>,
	{
		self.export_to_tree(
			dest,
			false,
			|_, index, value| {
				let prev = if let Some(query_plan) = mgmt.get_db_state_from_index(index) {
					self.get(&query_plan)
				} else {
					None
				};
				map_value(prev.as_ref(), index, value)
			},

		)
	}
	
}

#[cfg(test)]
pub(crate) trait TreeTestMethods {
	fn nb_internal_history(&self) -> usize;
	fn nb_internal_branch(&self) -> usize;
}

#[cfg(test)]
impl<I, BI, V, D, BD> TreeTestMethods for Tree<I, BI, V, D, BD>
	where
		I: Default + Ord + Clone,
		BI: LinearState,
		V: Value + Clone + Eq,
		D: LinearStorage<Linear<V, BI, BD>, I>,
		BD: LinearStorage<V::Storage, BI>,
{
	fn nb_internal_history(&self) -> usize {
		let nb = &mut 0;
		for index in self.branches.rev_index_iter() {
			self.branches.apply_on(index, |branch| {
				*nb += branch.value.storage().len();
			});
		}
		*nb
	}

	fn nb_internal_branch(&self) -> usize {
		self.branches.len()
	}
}

impl<I, BI, V, D, BD> DataRefMut<V> for Tree<I, BI, V, D, BD>
	where
		I: Default + Ord + Clone + Encode,
		BI: LinearState,
		V: ValueRef + Clone + Eq,
		D: for<'a> LinearStorageMem<'a, Linear<V, BI, BD>, I>,
		BD: for<'a> LinearStorageMem<'a, V::Storage, BI, Context = D::Context> + Trigger,
{
	fn get_mut(&mut self, at: &Self::SE) -> Option<&mut V> {
		let (branch_index, index) = at.latest();
		for branch_ix in self.branches.rev_index_iter() {
			let iter_branch_index = self.branches.get_state(branch_ix);
			if &iter_branch_index == branch_index {
				let branch = self.branches.get_ref_mut(branch_ix);
				let index = Latest::unchecked_latest(index.clone());
				return branch.value.get_mut(&index);
			}
			if &iter_branch_index < branch_index {
				break;
			}
		}
		None
	}

	fn set_mut(&mut self, value: V, at: &Self::SE) -> UpdateResult<Option<V>> {
		// Warn dup code, can be merge if change set to return previ value: with
		// ref refact will be costless
		let (branch_index, index) = at.latest();
		let mut insert_at = None;
		let mut next_branch_index = self.branches.last();
		while let Some(branch_ix) = next_branch_index {
			let branch = self.branches.get_ref_mut(branch_ix);
			let iter_branch_index = &branch.state;
			if iter_branch_index == branch_index {
				let index = Latest::unchecked_latest(index.clone());
				return branch.value.set_mut(value, &index);
			}
			if iter_branch_index < branch_index {
				break;
			}
			insert_at = Some(branch_ix);
			next_branch_index = self.branches.previous_index(branch_ix);
		}
		let branch = Branch::new(value, at.latest(), &self.init_child);
		if let Some(index) = insert_at {
			self.branches.insert(index, branch);
		} else {
			self.branches.push(branch);
		}
		UpdateResult::Changed(None)
	}
}

#[cfg(feature = "temp-size-impl")]
type LinearBackendTempSize = crate::backend::in_memory::MemoryOnly<Option<Vec<u8>>, u64>;
#[cfg(feature = "temp-size-impl")]
type TreeBackendTempSize = crate::backend::in_memory::MemoryOnly<Linear<Option<Vec<u8>>, u64, LinearBackendTempSize>, u32>;

#[cfg(feature = "temp-size-impl")]
impl Tree<u32, u64, Option<Vec<u8>>, TreeBackendTempSize, LinearBackendTempSize> {
	/// Temporary function to get occupied stage.
	/// TODO replace by heapsizeof
	pub fn temp_size(&self) -> usize {
		let mut size = 0;
		for i in self.branches.rev_index_iter() {
			let b = self.branches.get_ref(i);
			size += 4; // branch index (using u32 as usize)
			size += b.value.temp_size();
		}
		size
	}
}

impl<'a, I, BI, V, D, BD> DataSlices<'a, V> for Tree<I, BI, V, D, BD>
	where
		I: Default + Ord + Clone,
		BI: LinearState,
		V: Value + Clone + AsRef<[u8]>,
		D: LinearStorageSlice<Linear<V, BI, BD>, I>,
		BD: AsRef<[u8]> + LinearStorageRange<'a, V::Storage, BI>,
{
	tree_get!(
		get_slice,
		&[u8],
		apply_on_slice,
		|b: &'a [u8], ix| <Linear<V, BI, BD>>::get_range(b, ix),
		|result, b: &'a [u8], _| &b[result],
		'b
	);
}

pub mod aggregate {
	use super::*;

	/// Tree access to Sum structure.
	///
	/// The aggregate must be applied in a non associative
	/// non commutative way (operations only apply
	/// from oldest zero item to the target state).
	/// Good for diff, but can be use for other use case
	/// with simple implementation (eg list). 
	pub struct Sum<'a, I, BI, V: SummableValue, D: Context, BD: Context>(pub &'a Tree<I, BI, V::Value, D, BD>);

	impl<'a, I, BI, V: SummableValue, D: Context, BD: Context> sp_std::ops::Deref for Sum<'a, I, BI, V, D, BD> {
		type Target = Tree<I, BI, V::Value, D, BD>;

		fn deref(&self) -> &Tree<I, BI, V::Value, D, BD> {
			&self.0
		}
	}

	impl<'a, I, BI, V, D, BD> DataBasis for Sum<'a, I, BI, V, D, BD>
		where
			I: Default + Ord + Clone,
			BI: LinearState,
			V: SummableValue,
			V::Value: Value + Clone,
			D: LinearStorage<Linear<V::Value, BI, BD>, I>,
			BD: LinearStorage<<V::Value as Value>::Storage, BI>,
	{
		type S = ForkPlan<I, BI>;
		type Index = (I, BI);

		fn contains(&self, at: &Self::S) -> bool {
			self.0.contains(at)
		}

		fn is_empty(&self) -> bool {
			self.0.is_empty()
		}
	}

	impl<'a, I, BI, V, D, BD> Data<V::Value> for Sum<'a, I, BI, V, D, BD>
		where
			I: Default + Ord + Clone,
			BI: LinearState,
			V: SummableValue,
			V::Value: Value + Clone,
			D: LinearStorage<Linear<V::Value, BI, BD>, I>,
			BD: LinearStorage<<V::Value as Value>::Storage, BI>,
	{
		fn get(&self, at: &Self::S) -> Option<V::Value> {
			self.0.get(at)
		}
	}

	impl<'a, I, BI, V, D, BD> DataSum<V> for Sum<'a, I, BI, V, D, BD>
		where
			I: Default + Ord + Clone,
			BI: LinearState,
			V: SummableValue,
			V::Value: Value + Clone,
			D: LinearStorage<Linear<V::Value, BI, BD>, I>,
			BD: LinearStorage<<V::Value as Value>::Storage, BI>,
	{
		fn get_sum_values(&self, at: &Self::S, changes: &mut Vec<V::Value>) -> bool {
			// could also exten tree_get macro but it will end up being hard to read,
			// so copying loop here.
			let mut next_branch_index = self.branches.last();
			for (state_branch_range, state_branch_index) in at.iter() {
				while let Some(branch_ix) = next_branch_index {
					let branch_index = &self.branches.get_state(branch_ix);
					if branch_index < &state_branch_index {
						break;
					} else if branch_index == &state_branch_index {
						// TODO add a lower bound check (maybe debug_assert it only).
						let mut upper_bound = state_branch_range.end.clone();
						upper_bound -= BI::one();
						let result = &mut false;
						self.branches.apply_on(branch_ix, |branch| {
							*result = LinearSum::<V, _, _>(&branch.value)
								.get_sum_values(&upper_bound, changes);
						});
						if *result {
							return true;
						}
					}
					next_branch_index = self.branches.previous_index(branch_ix);
				}
			}

			// composite part.
			while let Some(branch_ix) = next_branch_index {
				let branch_index = &self.branches.get_state(branch_ix);
				if branch_index <= &at.composite_treshold.0 {
					let result = &mut false;
					self.branches.apply_on(branch_ix, |branch| {
						*result = LinearSum::<V, _, _>(&branch.value)
							.get_sum_values(&at.composite_treshold.1, changes);
					});
					if *result {
						return true;
					}
				}
				next_branch_index = self.branches.previous_index(branch_ix);
			}
		
			false
		}
	}
}

#[cfg(feature = "force-data")]
pub mod force {
	use super::*;
	use crate::historied::force::ForceDataMut;

	impl<I, BI, V, D, BD> ForceDataMut<V> for Tree<I, BI, V, D, BD>
		where
			I: Default + Ord + Clone + Encode,
			BI: LinearState,
			V: Value + Clone + Eq,
			D: LinearStorage<Linear<V, BI, BD>, I>,
			BD: LinearStorage<V::Storage, BI> + Trigger,
	{
		fn force_set(&mut self, value: V, at: &Self::Index) -> UpdateResult<()> {
			// Warn dup code, just different linear function call from fn set,
			// and using directly index, TODO factor result handle at least.
			let (branch_index, index) = at;
			let mut insert_at = None;
			for branch_ix in self.branches.rev_index_iter() {
				let iter_branch_index = self.branches.get_state(branch_ix);
				if &iter_branch_index == branch_index {
					let index = index.clone();
					let mut result= UpdateResult::Unchanged;
					self.branches.apply_on_mut(branch_ix, |branch| {
						result = branch.value.force_set(value, &index);
						matches!(result, UpdateResult::Changed(_))
					});
					return match result {
						UpdateResult::Changed(_) => {
							UpdateResult::Changed(())
						},
						UpdateResult::Cleared(_) => {
							self.remove_branch(branch_ix);
							if self.branches.len() == 0 {
								UpdateResult::Cleared(())
							} else {
								UpdateResult::Changed(())
							}
						},
						UpdateResult::Unchanged => UpdateResult::Unchanged,
					};
				}
				if &iter_branch_index < branch_index {
					break;
				}
				insert_at = Some(branch_ix);
			}
			let branch = Branch::new(value, at, &self.init_child);
			if let Some(index) = insert_at {
				self.branches.insert(index, branch);
			} else {
				self.branches.push(branch);
			}
			UpdateResult::Changed(())
		}
	}
}

#[cfg(feature = "conditional-data")]
pub mod conditional {
	use super::*;
	use crate::historied::conditional::ConditionalDataMut;

	// TODO current implementation is incorrect, we need an index that fails at first
	// branch that is parent to the dest (a tree path flattened into a ForkPlan like
	// struct). Element prior (I, BI) are not needed (only children).
	// Then we still apply only at designated (I, BI) but any value in the plan are
	// skipped.
	impl<I, BI, V, D, BD> ConditionalDataMut<V> for Tree<I, BI, V, D, BD>
		where
			I: Default + Ord + Clone + Encode,
			BI: LinearState,
			V: Value + Clone + Eq,
			D: LinearStorage<Linear<V, BI, BD>, I>,
			BD: LinearStorage<V::Storage, BI> + Trigger,
	{
		// TODO this would require to get all branch index that are children
		// of this index, and also their current upper bound.
		// That can be fairly costy.
		type IndexConditional = Self::Index;

		fn can_set(&self, no_overwrite: Option<&V>, at: &Self::IndexConditional) -> bool {
			self.can_if_inner(no_overwrite, at)
		}
		
		fn set_if_possible(&mut self, value: V, at: &Self::IndexConditional) -> Option<UpdateResult<()>> {
			self.set_if_inner(value, at, false)
		}

		fn set_if_possible_no_overwrite(&mut self, value: V, at: &Self::IndexConditional) -> Option<UpdateResult<()>> {
			self.set_if_inner(value, at, true)
		}
	}

	impl<I, BI, V, D, BD> Tree<I, BI, V, D, BD>
		where
			I: Default + Ord + Clone + Encode,
			BI: LinearState,
			V: Value + Clone + Eq,
			D: LinearStorage<Linear<V, BI, BD>, I>,
			BD: LinearStorage<V::Storage, BI> + Trigger,
	{

		fn set_if_inner(
			&mut self,
			value: V,
			at: &<Self as DataBasis>::Index,
			no_overwrite: bool,
		) -> Option<UpdateResult<()>> {
			let (branch_index, index) = at;
			let mut insert_at = None;
			for branch_ix in self.branches.rev_index_iter() {
				let iter_branch_index = self.branches.get_state(branch_ix);
				if &iter_branch_index == branch_index {
					let mut result = None;
					self.branches.apply_on_mut(branch_ix, |branch| {
						result = if no_overwrite {
							branch.value.set_if_possible_no_overwrite(value, &index)
						} else {
							branch.value.set_if_possible(value, &index)
						};
						matches!(result, Some(UpdateResult::Changed(_)))
					});
					return match result {
						Some(UpdateResult::Cleared(_)) => {
							self.remove_branch(branch_ix);
							if self.branches.len() == 0 {
								Some(UpdateResult::Cleared(()))
							} else {
								Some(UpdateResult::Changed(()))
							}
						},
						r => r,
					};
				}
				if &iter_branch_index < branch_index {
					break;
				}
				insert_at = Some(branch_ix);
			}
			let branch = Branch::new(value, at, &self.init_child);
			if let Some(index) = insert_at {
				self.branches.insert(index, branch);
			} else {
				self.branches.push(branch);
			}
			Some(UpdateResult::Changed(()))
		}

		fn can_if_inner(
			&self,
			value: Option<&V>,
			at: &<Self as DataBasis>::Index,
		) -> bool {
			let (branch_index, index) = at;
			for branch_ix in self.branches.rev_index_iter() {
				let iter_branch_index = self.branches.get_state(branch_ix);
				if &iter_branch_index == branch_index {
					let result = &mut false;
					self.branches.apply_on(branch_ix, |branch| {
						*result = branch.value.can_set(value, &index);
					});
					return *result;
				}
				if &iter_branch_index < branch_index {
					break;
				}
			}
			true
		}
	}
}

#[cfg(test)]
mod test {
	use super::*;
	use crate::management::tree::test::test_states;
	use crate::{InitFrom, StateIndex};
	use super::aggregate::Sum as TreeSum;

	#[cfg(feature = "encoded-array-backend")]
	#[test]
	fn compile_double_encoded_single() {
		use crate::backend::encoded_array::{EncodedArray, NoVersion};
		use crate::historied::Data;

		type BD<'a> = EncodedArray<'a, Vec<u8>, NoVersion>;
//		type D<'a> = crate::historied::linear::MemoryOnly<
		type D<'a> = EncodedArray<'a,
			crate::historied::linear::Linear<Vec<u8>, u64, BD<'a>>,
			NoVersion,
//			u64
		>;
		let item: Tree<u64, u64, Vec<u8>, D, BD> = InitFrom::init_from(((), ()));
		let at: ForkPlan<u64, u64> = Default::default();
		item.get(&at);
		item.get_slice(&at);
		let latest = Latest::unchecked_latest((0, 0));
		let _item: Tree<u64, u64, Vec<u8>, D, BD> = Tree::new(b"dtd".to_vec(), &latest, ((), ()));
//		let slice = &b"dtdt"[..];
//		use crate::backend::encoded_array::{EncodedArrayValue};
//		let bd = crate::historied::linear::Linear::<Vec<u8>, u64, BD>::from_slice(slice);
//		let bd = BD::from_slice(slice);
		let bd = D::default();
		use crate::backend::LinearStorage;
		bd.get_lookup(1usize);
	}

	#[cfg(feature = "encoded-array-backend")]
	#[test]
	fn compile_double_encoded_node() {
		use crate::backend::encoded_array::{EncodedArray, DefaultVersion};
		use crate::backend::nodes::{Head, Node, ContextHead};
		use crate::backend::nodes::test::MetaSize;
		use crate::historied::Data;
		use sp_std::collections::btree_map::BTreeMap;

		type EncArray<'a> = EncodedArray<'a, Vec<u8>, DefaultVersion>;
		type Backend<'a> = BTreeMap<Vec<u8>, Node<Vec<u8>, u64, EncArray<'a>, MetaSize>>;
		type BD<'a> = Head<Vec<u8>, u64, EncArray<'a>, MetaSize, Backend<'a>, ()>;

		type V2<'a> = crate::historied::linear::Linear<Vec<u8>, u64, BD<'a>>;
		type EncArray2<'a> = EncodedArray<'a, V2<'a>, DefaultVersion>;
		type Backend2<'a> = BTreeMap<Vec<u8>, Node<V2<'a>, u64, EncArray2<'a>, MetaSize>>;
//		type D<'a> = crate::historied::linear::MemoryOnly<
		type D<'a> = Head<V2<'a>, u64, EncArray2<'a>, MetaSize, Backend2<'a>, ContextHead<Backend<'a>, ()>>;
		let init_head_child = ContextHead {
			backend: Backend::new(),
			key: b"any".to_vec(),
			node_init_from: (),
			encoded_indexes: Vec::new(),
		};
		let init_head = ContextHead {
			backend: Backend2::new(),
			key: b"any".to_vec(),
			node_init_from: init_head_child.clone(),
			encoded_indexes: Vec::new(),
		};
		let item: Tree<u64, u64, Vec<u8>, D, BD> = InitFrom::init_from((init_head.clone(), init_head_child.clone()));
		let at: ForkPlan<u64, u64> = Default::default();
		item.get(&at);

//	D: LinearStorage<Linear<V, BI, BD>, I>, // TODO rewrite to be linear storage of BD only.
//	BD: LinearStorage<V, BI>,

/*
//		item.get_slice(&at);
		let latest = Latest::unchecked_latest((0, 0));
		let _item: Tree<u64, u64, Vec<u8>, D, BD> = Tree::new(b"dtd".to_vec(), &latest, init_head.clone());
*/
//		let slice = &b"dtdt"[..];
//		use crate::backend::encoded_array::{EncodedArrayValue};
//		let bd = crate::historied::linear::Linear::<Vec<u8>, u64, BD>::from_slice(slice);
//		let bd = BD::from_slice(slice);
		let bd = D::init_from(init_head);
		use crate::backend::LinearStorage;
		let _a: Option<HistoriedValue<V2, u64>> = bd.get_lookup(1usize);
	}

	#[cfg(feature = "encoded-array-backend")]
	#[test]
	fn compile_double_encoded_node_2() {
		use crate::backend::in_memory::MemoryOnly;
		use crate::backend::nodes::{Head, Node, ContextHead};
		use crate::backend::nodes::test::MetaSize;
		use crate::historied::Data;
		use sp_std::collections::btree_map::BTreeMap;

		type MemOnly = MemoryOnly<Vec<u8>, u32>;
		type Backend = BTreeMap<Vec<u8>, Node<Vec<u8>, u32, MemOnly, MetaSize>>;
		type BD = Head<Vec<u8>, u32, MemOnly, MetaSize, Backend, ()>;

		type V2 = crate::historied::linear::Linear<Vec<u8>, u32, BD>;
		type MemOnly2 = MemoryOnly<V2, u32>;
		type Backend2 = BTreeMap<Vec<u8>, Node<V2, u32, MemOnly2, MetaSize>>;
		type D = Head<V2, u32, MemOnly2, MetaSize, Backend2, ContextHead<Backend, ()>>;
		let init_head_child = ContextHead {
			backend: Backend::new(),
			key: b"any".to_vec(),
			node_init_from: (),
			encoded_indexes: Vec::new(),
		};
		let init_head = ContextHead {
			backend: Backend2::new(),
			key: b"any".to_vec(),
			node_init_from: init_head_child.clone(),
			encoded_indexes: Vec::new(),
		};
		let item: Tree<u32, u32, Vec<u8>, D, BD> = InitFrom::init_from((init_head.clone(), init_head_child.clone()));
		let at: ForkPlan<u32, u32> = Default::default();
		item.get(&at);

		let bd = D::init_from(init_head);
		use crate::backend::LinearStorage;
		let _a: Option<HistoriedValue<V2, u32>> = bd.get_lookup(1usize);
	}

	fn test_set_get_ref<T, V>(context: T::Context)
		where
			V: crate::historied::Value + std::fmt::Debug + From<u16> + Eq,
			T: InitFrom,
			T: crate::historied::DataBasis<S = ForkPlan<u32, u32>>,
			T: crate::historied::DataRef<V>,
			T: crate::historied::Data<V>,
			T: crate::historied::DataMut<
				V,
				Index = (u32, u32),
				SE = Latest<(u32, u32)>,
			>,
	{
		// 0> 1: _ _ X
		// |			 |> 3: 1
		// |			 |> 4: 1
		// |		 |> 5: 1
		// |> 2: _
		let mut states = test_states();

		let mut item: T = InitFrom::init_from(context.clone());

		// could rand shuffle if rand get imported later.
		let disordered = [
			[1u16,2,3,5,4],
			[2,5,1,3,4],
			[5,3,2,4,1],
		];
		for r in disordered.iter() {
			for i in r {
				let v: V = i.clone().into();
				let i: u32 = i.clone().into();
				item.set(v, &states.unchecked_latest_at(i).unwrap());
			}
			for i in r {
				let v: V = i.clone().into();
				let i: u32 = i.clone().into();
				assert_eq!(item.get_ref(&states.query_plan(i)), Some(&v));
			}
		}
	}


	fn test_set_get<T, V>(context: T::Context)
		where
			V: crate::historied::Value + std::fmt::Debug + From<u16> + Eq,
			T: InitFrom,
			T: crate::historied::DataBasis<S = ForkPlan<u32, u32>>,
			T: crate::historied::Data<V>,
			T: crate::historied::DataMut<
				V,
				Index = (u32, u32),
				SE = Latest<(u32, u32)>,
			>,
	{
		// 0> 1: _ _ X
		// |			 |> 3: 1
		// |			 |> 4: 1
		// |		 |> 5: 1
		// |> 2: _
		let mut states = test_states();

		let mut item: T = InitFrom::init_from(context.clone());

		for i in 0..6 {
			assert_eq!(item.get(&states.query_plan(i)), None);
		}

		// setting value respecting branch build order
		for i in 1..6 {
			item.set(i.into(), &states.unchecked_latest_at(i.into()).unwrap());
		}

		for i in 1..6 {
			assert_eq!(item.get(&states.query_plan(i.into())), Some(i.into()));
		}

		let ref_1 = states.query_plan(1u16.into());
		assert_eq!(Some(false), states.branch_state_mut(&1, |ls| ls.drop_state()));

		let ref_1_bis = states.query_plan(1);
		assert_eq!(item.get(&ref_1), Some(1.into()));
		assert_eq!(item.get(&ref_1_bis), None);
		item.set(11.into(), &states.unchecked_latest_at(1).unwrap());
		// lazy linear clean of drop state on insert
		assert_eq!(item.get(&ref_1), Some(11.into()));
		assert_eq!(item.get(&ref_1_bis), Some(11.into()));

		item = InitFrom::init_from(context.clone());

		// need fresh state as previous modification leaves unattached branches
		let mut states = test_states();
		// could rand shuffle if rand get imported later.
		let disordered = [
			[1u16,2,3,5,4],
			[2,5,1,3,4],
			[5,3,2,4,1],
		];
		for r in disordered.iter() {
			for i in r {
				let v: V = i.clone().into();
				let i: u32 = i.clone().into();
				item.set(v, &states.unchecked_latest_at(i).unwrap());
			}
			for i in r {
				let v: V = i.clone().into();
				let i: u32 = i.clone().into();
				assert_eq!(item.get(&states.query_plan(i)), Some(v));
			}
		}
	}

	#[cfg(not(feature = "conditional-data"))]
	fn test_conditional_set_get<T, V>(_context: T::Context, _context2: T::Context)
		where T: crate::historied::DataMut<u32> {
	}

	#[cfg(feature = "conditional-data")]
	fn test_conditional_set_get<T, V>(context: T::Context, context2: T::Context)
		where
			V: crate::historied::Value + std::fmt::Debug + From<u16> + Eq,
			T: InitFrom,
			T: crate::historied::DataBasis<S = ForkPlan<u32, u32>>,
			T: crate::historied::DataRef<V>,
			T: crate::historied::Data<V>,
			T: crate::historied::DataMut<
				V,
				Index = (u32, u32),
				SE = Latest<(u32, u32)>,
			>,
			T: crate::historied::conditional::ConditionalDataMut<
				V,
				IndexConditional = (u32, u32),
			>,
	{
		// 0> 1: _ _ X
		// |			 |> 3: 1
		// |			 |> 4: 1
		// |		 |> 5: 1
		// |> 2: _
		let mut states = test_states();
		let mut item: T = InitFrom::init_from(context.clone());
		let mut item2: T = InitFrom::init_from(context2.clone());

		for i in 0..6 {
			assert_eq!(item.get(&states.query_plan(i)), None);
		}

		// setting value not respecting branch build order
		// set in past (latest is 1, 2) is fine
		assert_eq!(Some(UpdateResult::Changed(())), item.set_if_possible(1.into(), &(1, 1)));
		assert_eq!(Some(UpdateResult::Changed(())), item2.set_if_possible(1.into(), &(1, 2)));
		// but not with another value
		assert_eq!(None, item.set_if_possible(8.into(), &(1, 0)));
		assert_eq!(None, item2.set_if_possible(8.into(), &(1, 1)));
		// can overwrite
		assert_eq!(Some(UpdateResult::Changed(())), item.set_if_possible(2.into(), &(1, 1)));
		assert_eq!(Some(UpdateResult::Changed(())), item2.set_if_possible(2.into(), &(1, 2)));
		// not if not allowed
		assert_eq!(None, item.set_if_possible_no_overwrite(3.into(), &(1, 1)));
		assert_eq!(None, item2.set_if_possible_no_overwrite(3.into(), &(1, 2)));
		// unchanged is allowed
		assert_eq!(Some(UpdateResult::Unchanged), item.set_if_possible(2.into(), &(1, 1)));
		assert_eq!(Some(UpdateResult::Unchanged), item2.set_if_possible(2.into(), &(1, 2)));
		assert_eq!(item.get_ref(&states.query_plan(1)), Some(&2.into()));
		states.drop_state(&1u32);
		states.drop_state(&1u32);
		assert_eq!(item.get_ref(&states.query_plan(1)), None);
		assert_eq!(item2.get_ref(&states.query_plan(1)), None);
		// no longer allowd to change the branch TODO we should be able to, but
		// with blockchain tree use case with removal only on canonicalisation
		// and pruning it should be fine.
		assert_eq!(None, item2.set_if_possible(3.into(), &(1, 1)));
	}

	#[cfg(not(feature = "force-data"))]
	fn test_force_set_get<T, V>(_context: T::Context) where
		T: crate::historied::DataMut<V>,
		V: crate::historied::Value,
	{ }

	#[cfg(feature = "force-data")]
	fn test_force_set_get<T, V>(context: T::Context)
		where
			V: crate::historied::Value + std::fmt::Debug + From<u16> + Eq,
			T: InitFrom,
			T: codec::Encode,
			T: DecodeWithContext,
			T: crate::Trigger,
			T: crate::historied::DataBasis<
				S = ForkPlan<u32, u32>,
				Index = (u32, u32),
			>,
			T: crate::historied::Data<V>,
			T: crate::historied::DataMut<
				V,
				SE = Latest<(u32, u32)>,
			>,
			T: crate::historied::force::ForceDataMut<V>,
	{
		// 0> 1: _ _ X
		// |			 |> 3: 1
		// |			 |> 4: 1
		// |		 |> 5: 1
		// |> 2: _
		let mut states = test_states();
		let mut item: T = InitFrom::init_from(context.clone());

		for i in 0..6 {
			assert_eq!(item.get(&states.query_plan(i)), None);
		}

		// setting value not respecting branch build order
		assert_eq!(UpdateResult::Changed(()), item.force_set(0.into(), &(1, 2)));
		assert_eq!(UpdateResult::Changed(()), item.force_set(1.into(), &(1, 1)));
		// out of range
		assert_eq!(UpdateResult::Changed(()), item.force_set(8.into(), &(1, 0)));
		// can set in invalid range too
		assert_eq!(UpdateResult::Changed(()), item.force_set(3.into(), &(2, 5)));
		assert_eq!(UpdateResult::Changed(()), item.force_set(2.into(), &(2, 1)));

		let mut states2 = states.clone();

		let check = |states: &mut crate::management::tree::test::TestState, item: &T| {
			assert_eq!(item.get(&states.query_plan(1)), Some(0.into()));
			assert_eq!(item.get(&states.query_plan(2)), Some(2.into()));
			states.drop_state(&1u32);
			assert_eq!(item.get(&states.query_plan(1)), Some(1.into()));
			states.drop_state(&1u32);
			assert_eq!(item.get(&states.query_plan(1)), None);
		};
		check(&mut states, &item);

		if T::TRIGGER {
			// Using the item from fresh state
			// reqires trigger first
			item.trigger_flush();
		}

		let encoded = item.encode();
		let item = T::decode_with_context(&mut encoded.as_slice(), &context).unwrap();
		check(&mut states2, &item);
	}

	use ref_cast::RefCast;
	#[derive(RefCast)]
	#[repr(transparent)]
	#[derive(Clone, Copy, PartialEq, Eq, Debug)]
	/// U16 with 0 as neutral item.
	struct U16Neutral(u16); 

	impl std::ops::Deref for U16Neutral {
		type Target = u16;
		fn deref(&self) -> &u16 {
			&self.0
		}
	}

	impl std::ops::DerefMut for U16Neutral {
		fn deref_mut(&mut self) -> &mut u16 {
			&mut self.0
		}
	}

	impl From<u16> for U16Neutral {
		#[inline(always)]
		fn from(v: u16) -> Self {
			U16Neutral(v)
		}
	}

	impl Value for U16Neutral {
		const NEUTRAL: bool = true;

		type Storage = u16;

		#[inline(always)]
		fn is_neutral(&self) -> bool {
			self.0 == 0
		}

		#[inline(always)]
		fn is_storage_neutral(storage: &Self::Storage) -> bool {
			storage == &0u16
		}

		#[inline(always)]
		fn from_storage(storage: Self::Storage) -> Self {
			U16Neutral(storage)
		}

		#[inline(always)]
		fn into_storage(self) -> Self::Storage {
			self.0
		}
	}

	impl ValueRef for U16Neutral {
		fn from_storage_ref(storage: &Self::Storage) -> &Self {
			U16Neutral::ref_cast(storage)
		}

		fn into_storage_ref(&self) -> &Self::Storage {
			&self.0
		}

		fn from_storage_ref_mut(storage: &mut Self::Storage) -> &mut Self {
			U16Neutral::ref_cast_mut(storage)
		}

		fn into_storage_ref_mut(&mut self) -> &mut Self::Storage {
			&mut self.0
		}
	}

	fn test_migrate<T, V> (
		context1: T::Context,
		context2: T::Context,
		context3: T::Context,
		context4: T::Context,
	)	where
		V: crate::historied::Value + std::fmt::Debug + From<u16> + Eq,
		T: InitFrom + Trigger,
		T: Clone + codec::Encode + DecodeWithContext,
		T: TreeTestMethods,
		T: crate::historied::DataBasis<S = ForkPlan<u32, u32>>,
		T: crate::historied::Data<V>,
		T: crate::historied::DataMut<
			V,
			Index = (u32, u32),
			SE = Latest<(u32, u32)>,
			GC = MultipleGc<u32, u32>,
			Migrate = MultipleMigrate<u32, u32>,
		>,
	{
		use crate::management::{ManagementMut, Management, ForkableManagement};
		use crate::test::StateInput;

		let check_state = |states: &mut crate::test::InMemoryMgmtSer, target: Vec<(u32, u32)>| {
			let mut gc = states.get_migrate();
			let (pruning, iter) = gc.migrate().touched_state();
			assert_eq!(pruning, None);
			let mut set = std::collections::BTreeSet::new();
			for s in iter {
				set.insert(s.clone());
			}

			let reference: std::collections::BTreeSet<_> = target.into_iter().collect();
			assert_eq!(set, reference);
		};

		let mut states = crate::test::InMemoryMgmtSer::default();
		let s0 = states.latest_state_fork();

		let mut item1: T = InitFrom::init_from(context1.clone());
		let mut item2: T = InitFrom::init_from(context2.clone());
		let s1 = states.append_external_state(StateInput(1), &s0).unwrap();
		item1.set(1.into(), &states.get_db_state_mut(&StateInput(1)).unwrap());
		item2.set(1.into(), &states.get_db_state_mut(&StateInput(1)).unwrap());
		// fusing cano
		let _ = states.append_external_state(StateInput(101), s1.latest()).unwrap();
		item1.set(2.into(), &states.get_db_state_mut(&StateInput(101)).unwrap());
		item2.set(2.into(), &states.get_db_state_mut(&StateInput(101)).unwrap());
		let s1 = states.append_external_state(StateInput(102), s1.latest()).unwrap();
		item1.set(3.into(), &states.get_db_state_mut(&StateInput(102)).unwrap());
		let s1 = states.append_external_state(StateInput(103), s1.latest()).unwrap();
		item1.set(4.into(), &states.get_db_state_mut(&StateInput(103)).unwrap());
		let _ = states.append_external_state(StateInput(104), s1.latest()).unwrap();
		item1.set(5.into(), &states.get_db_state_mut(&StateInput(104)).unwrap());
		let s1 = states.append_external_state(StateInput(105), s1.latest()).unwrap();
		item1.set(6.into(), &states.get_db_state_mut(&StateInput(105)).unwrap());
		item2.set(6.into(), &states.get_db_state_mut(&StateInput(105)).unwrap());
		// end fusing (shift following branch index by 2)
		let _s2 = states.append_external_state(StateInput(2), &s0).unwrap();
		let s1b = states.append_external_state(StateInput(12), s1.latest()).unwrap();
		let s1 = states.append_external_state(StateInput(13), s1b.latest()).unwrap();
		let sx = states.append_external_state(StateInput(14), s1.latest()).unwrap();
		let qp14 = states.get_db_state(&StateInput(14)).unwrap();
		assert_eq!(states.drop_state(sx.latest(), true).unwrap().len(), 1);
		let s3 = states.append_external_state(StateInput(3), s1.latest()).unwrap();
		let _s4 = states.append_external_state(StateInput(4), s1.latest()).unwrap();
		let _s5 = states.append_external_state(StateInput(5), s1b.latest()).unwrap();
		// 0> 1: _ _ X
		// |			 |> 3: 1
		// |			 |> 4: 1
		// |		 |> 5: 1
		// |> 2: _ _
		let mut item3: T = InitFrom::init_from(context3.clone());
		let mut item4: T = InitFrom::init_from(context4.clone());
		item1.set(15.into(), &states.get_db_state_mut(&StateInput(5)).unwrap());
		item2.set(15.into(), &states.get_db_state_mut(&StateInput(5)).unwrap());
		item1.set(12.into(), &states.get_db_state_mut(&StateInput(2)).unwrap());

		let s3head = states.append_external_state(StateInput(32), s3.latest()).unwrap();
		item1.set(13.into(), &states.get_db_state_mut(&StateInput(32)).unwrap());
		item2.set(13.into(), &states.get_db_state_mut(&StateInput(32)).unwrap());
		item3.set(13.into(), &states.get_db_state_mut(&StateInput(32)).unwrap());
		item4.set(13.into(), &states.get_db_state_mut(&StateInput(32)).unwrap());
		let s3tmp = states.append_external_state(StateInput(33), s3head.latest()).unwrap();
		item1.set(14.into(), &states.get_db_state_mut(&StateInput(33)).unwrap());
		item3.set(0.into(), &states.get_db_state_mut(&StateInput(33)).unwrap());
		let s3head = states.append_external_state(StateInput(34), s3tmp.latest()).unwrap();
		let _s6 = states.append_external_state(StateInput(6), s3tmp.latest()).unwrap();
		let _s3head = states.append_external_state(StateInput(35), s3head.latest()).unwrap();
		item1.set(15.into(), &states.get_db_state_mut(&StateInput(35)).unwrap());
		item2.set(15.into(), &states.get_db_state_mut(&StateInput(35)).unwrap());
		item4.set(0.into(), &states.get_db_state_mut(&StateInput(35)).unwrap());
		item1.set(0.into(), &states.get_db_state_mut(&StateInput(6)).unwrap());

		//let old_state = states.clone();
		// Apply change of composite to 33
		let filter_out = [101, 104, 2, 4, 5];
		let mut filter_qp = vec![qp14.index()];
		// dropped 14
		check_state(&mut states, filter_qp.clone());
		for i in filter_out.iter() {
			let qp = states.get_db_state(&StateInput(*i)).unwrap();
			filter_qp.push(qp.index());
		}

		let fp = states.get_db_state(&StateInput(35)).unwrap();
		states.canonicalize(fp, *s3tmp.latest(), None);
		// other drops from filter_out
		check_state(&mut states, filter_qp.clone());
		// no query plan for 14
		let filter_in = [1, 102, 103, 105, 12, 13, 32, 33, 34, 35, 6];

		let check_gc = |item1: &T, item2: &T, item3: &T, item4: &T, states: &mut crate::test::InMemoryMgmtSer| {
			//panic!("{:?} \n {:?}", old_state, states);
			let mut gc_item1 = item1.clone();
			let mut gc_item2 = item2.clone();
			let mut gc_item3 = item3.clone();
			let mut gc_item4 = item4.clone();
			{
				let gc = states.get_gc().unwrap();
				gc_item1.gc(gc.as_ref());
				gc_item2.gc(gc.as_ref());
				gc_item3.gc(gc.as_ref());
				gc_item4.gc(gc.as_ref());
				//panic!("{:?}", (gc.as_ref(), item4, gc_item4));
			}
			assert_eq!(gc_item1.nb_internal_history(), 8);
			assert_eq!(gc_item2.nb_internal_history(), 4);
			assert_eq!(gc_item3.nb_internal_history(), 2); // actually untouched
			assert_eq!(gc_item4.nb_internal_history(), 2); // actually untouched
			assert_eq!(gc_item1.nb_internal_branch(), 5);
			assert_eq!(gc_item2.nb_internal_branch(), 3);
			assert_eq!(gc_item3.nb_internal_branch(), 1);
			assert_eq!(gc_item4.nb_internal_branch(), 1);

			for i in filter_in.iter() {
				let fp = states.get_db_state(&StateInput(*i)).unwrap();
				assert_eq!(gc_item1.get(&fp), item1.get(&fp));
				assert_eq!(gc_item2.get(&fp), item2.get(&fp));
				assert_eq!(gc_item3.get(&fp), item3.get(&fp));
				assert_eq!(gc_item4.get(&fp), item4.get(&fp));
			}
			//panic!("{:?}", (gc, item1, gc_item1));
		};

		check_gc(&item1, &item2, &item3, &item4, &mut states.clone());
		item1.trigger_flush();
		let encoded = item1.encode();
		item1 = T::decode_with_context(&mut encoded.as_slice(), &context1).unwrap();
		item2.trigger_flush();
		let encoded = item2.encode();
		item2 = T::decode_with_context(&mut encoded.as_slice(), &context2).unwrap();
		item3.trigger_flush();
		let encoded = item3.encode();
		item3 = T::decode_with_context(&mut encoded.as_slice(), &context3).unwrap();
		item4.trigger_flush();
		let encoded = item4.encode();
		item4 = T::decode_with_context(&mut encoded.as_slice(), &context4).unwrap();
		check_gc(&item1, &item2, &item3, &item4, &mut states.clone());

		let check_migrate = |item1: &T, item2: &T, item3: &T, item4: &T, states: &mut crate::test::InMemoryMgmtSer| {
			let old_state = states.clone();
			// migrate 
			let filter_in = [33, 34, 35, 6];
			let mut gc_item1 = item1.clone();
			let mut gc_item2 = item2.clone();
			let mut gc_item3 = item3.clone();
			let mut gc_item4 = item4.clone();
			let mut states = states;
			{
				let mut gc = states.get_migrate();
				gc_item1.migrate(gc.migrate());
				gc_item2.migrate(gc.migrate());
				gc_item3.migrate(gc.migrate());
				gc_item4.migrate(gc.migrate());
				gc.applied_migrate();
			}
			// empty (applied_migrate ran)
			check_state(&mut states, vec![]);

			for i in filter_in.iter() {
				let fp = states.get_db_state(&StateInput(*i)).unwrap();
				assert_eq!(gc_item1.get(&fp), item1.get(&fp));
				assert_eq!(gc_item2.get(&fp), item2.get(&fp));
				assert_eq!(gc_item3.get(&fp), item3.get(&fp));
				assert_eq!(gc_item4.get(&fp), item4.get(&fp));
			}
			assert_eq!(gc_item1.nb_internal_history(), 8);
			assert_eq!(gc_item2.nb_internal_history(), 4);
			assert_eq!(gc_item3.nb_internal_history(), 2);
			assert_eq!(gc_item4.nb_internal_history(), 2);
			assert_eq!(gc_item1.nb_internal_branch(), 2);
			assert_eq!(gc_item2.nb_internal_branch(), 1);
			assert_eq!(gc_item3.nb_internal_branch(), 1);
			assert_eq!(gc_item4.nb_internal_branch(), 1);

			// on previous state set migrate with pruning_treshold 
			let filter_in = [33, 34, 35, 6];
			let mut gc_item1 = item1.clone();
			let mut gc_item2 = item2.clone();
			let mut gc_item3 = item3.clone();
			let mut gc_item4 = item4.clone();
			let mut states = old_state;
			let fp = states.get_db_state(&StateInput(35)).unwrap();
			states.canonicalize(fp, *s3tmp.latest(), Some(s3tmp.latest().1));
			{
				let mut gc = states.get_migrate();
				gc_item1.migrate(gc.migrate());
				gc_item2.migrate(gc.migrate());
				gc_item3.migrate(gc.migrate());
				gc_item4.migrate(gc.migrate());
				gc.applied_migrate();
				//panic!("{:?}", (gc, item3, gc_item3));
			}
			for i in filter_in.iter() {
				let fp = states.get_db_state(&StateInput(*i)).unwrap();
				assert_eq!(gc_item1.get(&fp), item1.get(&fp));
				assert_eq!(gc_item2.get(&fp), item2.get(&fp));
				if V::NEUTRAL {
					assert_eq!(gc_item3.get(&fp), None);
				} else {
					assert_eq!(gc_item3.get(&fp), item3.get(&fp));
				}
				assert_eq!(gc_item4.get(&fp), item4.get(&fp));
			}
			assert_eq!(gc_item1.nb_internal_history(), 3);
			assert_eq!(gc_item2.nb_internal_history(), 2);
			if V::NEUTRAL {
				assert_eq!(gc_item3.nb_internal_history(), 0);
			} else {
				assert_eq!(gc_item3.nb_internal_history(), 1);
			}
			assert_eq!(gc_item4.nb_internal_history(), 2);
			assert_eq!(gc_item1.nb_internal_branch(), 2);
			assert_eq!(gc_item2.nb_internal_branch(), 1);
			if V::NEUTRAL {
				assert_eq!(gc_item3.nb_internal_branch(), 0);
			} else {
				assert_eq!(gc_item3.nb_internal_branch(), 1);
			}
			assert_eq!(gc_item4.nb_internal_branch(), 1);
		};

		check_migrate(&item1, &item2, &item3, &item4, &mut states.clone());
		item1.trigger_flush();
		let encoded = item1.encode();
		item1 = T::decode_with_context(&mut encoded.as_slice(), &context1).unwrap();
		item2.trigger_flush();
		let encoded = item2.encode();
		item2 = T::decode_with_context(&mut encoded.as_slice(), &context2).unwrap();
		item3.trigger_flush();
		let encoded = item3.encode();
		item3 = T::decode_with_context(&mut encoded.as_slice(), &context3).unwrap();
		item4.trigger_flush();
		let encoded = item4.encode();
		item4 = T::decode_with_context(&mut encoded.as_slice(), &context4).unwrap();
		check_migrate(&item1, &item2, &item3, &item4, &mut states.clone());
	}

	#[test]
	fn test_memory_only() {
		type BD = crate::backend::in_memory::MemoryOnly<u32, u32>;
		type D = crate::backend::in_memory::MemoryOnly<
			crate::historied::linear::Linear<u32, u32, BD>,
			u32,
		>;
		type Tree = super::Tree<u32, u32, u32, D, BD>;
		test_set_get::<Tree, u32>(((), ()));
		test_set_get_ref::<Tree, u32>(((), ()));
		test_migrate::<Tree, u32>(((), ()), ((), ()), ((), ()), ((), ()));
		test_conditional_set_get::<Tree, u32>(((), ()), ((), ()));
		test_force_set_get::<Tree, u32>(((), ()));
	}

	macro_rules! test_with_trigger_inner {
		($meta: ty) => {{
		use crate::backend::nodes::{Head, ContextHead, InMemoryNoThreadBackend};
		use crate::backend::in_memory::MemoryOnly;

		type M = $meta;
		type Value = u16;
		type MemOnly = MemoryOnly<Value, u32>;
		type Backend1 = InMemoryNoThreadBackend::<Value, u32, MemOnly, M>;
		type BD = Head<Value, u32, MemOnly, M, Backend1, ()>;

		type V2 = crate::historied::linear::Linear<Value, u32, BD>;
		type MemOnly2 = MemoryOnly<V2, u32>;
		type Backend2 = InMemoryNoThreadBackend::<V2, u32, MemOnly2, M>;
		type D = Head<V2, u32, MemOnly2, M, Backend2, ContextHead<Backend1, ()>>;
		let backend1 = Backend1::new();
		let init_head_child = ContextHead {
			backend: backend1.clone(),
			key: b"any".to_vec(),
			node_init_from: (),
			encoded_indexes: Vec::new(),
		};
		let backend2 = Backend2::new();
		let init_head = ContextHead {
			backend: backend2.clone(),
			key: b"any".to_vec(),
			node_init_from: init_head_child.clone(),
			encoded_indexes: Vec::new(),
		};
		type Tree = super::Tree<u32, u32, Value, D, BD>;
		let context1 = (init_head, init_head_child);
		let mut context2 = context1.clone();
		context2.0.key = b"other".to_vec();
		context2.1.key = context2.0.key.clone();
		context2.0.node_init_from = context2.1.clone();
		let mut context3 = context1.clone();
		context3.0.key = b"othe3".to_vec();
		context3.1.key = context3.0.key.clone();
		context3.0.node_init_from = context3.1.clone();
		let mut context4 = context1.clone();
		context4.0.key = b"othe4".to_vec();
		context4.1.key = context4.0.key.clone();
		context4.0.node_init_from = context4.1.clone();

		test_set_get::<Tree, u16>(context1.clone());
		// trigger flush test is into test_migrate
		test_migrate::<Tree, u16>(context1.clone(), context2.clone(), context3.clone(), context4.clone());
		test_force_set_get::<Tree, u16>(context1.clone());
	}}}

	#[test]
	fn test_with_trigger() {
		test_with_trigger_inner!(crate::backend::nodes::test::MetaNb1);
		test_with_trigger_inner!(crate::backend::nodes::test::MetaNb2);
		test_with_trigger_inner!(crate::backend::nodes::test::MetaNb3);
	}

	#[cfg(feature = "xdelta3-diff")]
	#[test]
	fn test_diff1() {
		use crate::historied::aggregate::xdelta::{BytesDelta, BytesDiff, substract}; 
		type BD = crate::backend::in_memory::MemoryOnly8<Vec<u8>, u32>;
		type D = crate::backend::in_memory::MemoryOnly4<
			crate::historied::linear::Linear<BytesDiff, u32, BD>,
			u32,
		>;

		// 0> 1: _ _ X
		// |			 |> 3: 1
		// |			 |> 4: 1
		// |		 |> 5: 1
		// |> 2: _
		let mut states = test_states();
		let mut item: Tree<u32, u32, BytesDiff, D, BD> = InitFrom::init_from(((), ()));

		let successive_values: Vec<BytesDelta> = vec![
			Some(&[0u8, 1, 2, 3][..]).into(), // (1, 1)
			Some(&[1, 1, 2, 3][..]).into(), // (1,2)
			Some(&[1, 3][..]).into(), // (3, 3)
			BytesDelta::default(), // aka None, 4, 3 (follow) (1, 2)
		];

		let mut successive_deltas: Vec<BytesDiff> = Vec::with_capacity(successive_values.len());

		successive_deltas.push(substract(&Default::default(), &successive_values[0]));
		successive_deltas.push(substract(&successive_values[0], &successive_values[1]));
		successive_deltas.push(substract(&successive_values[1], &successive_values[2]));
		successive_deltas.push(substract(&successive_values[1], &successive_values[3]));

		let successive_deltas = successive_deltas;

		item.set(successive_deltas[0].clone(), &Latest::unchecked_latest((1, 1)));
		item.set(successive_deltas[1].clone(), &Latest::unchecked_latest((1, 2)));
		item.set(successive_deltas[2].clone(), &Latest::unchecked_latest((3, 3)));
		item.set(successive_deltas[3].clone(), &Latest::unchecked_latest((4, 3)));

		assert_eq!(item.get(&states.query_plan(1)).as_ref(), Some(&successive_deltas[1]));
		assert_eq!(item.get(&states.query_plan(3)).as_ref(), Some(&successive_deltas[2]));
		assert_eq!(item.get(&states.query_plan(4)).as_ref(), Some(&successive_deltas[3]));

		let item = TreeSum::<_, _, BytesDelta, _, _>(&item);
		assert_eq!(item.get_sum(&states.query_plan(1)).as_ref(), Some(&successive_values[1]));
		assert_eq!(item.get_sum(&states.query_plan(3)).as_ref(), Some(&successive_values[2]));
		assert_eq!(item.get_sum(&states.query_plan(4)).as_ref(), Some(&successive_values[3]));
	}

	#[test]
	fn test_diff2() {
		use crate::historied::aggregate::map_delta::{MapDelta, MapDiff, UnitDiff}; 
		type BD = crate::backend::in_memory::MemoryOnly<Vec<u8>, u32>;
		type D = crate::backend::in_memory::MemoryOnly<
			crate::historied::linear::Linear<MapDiff<u8, u8>, u32, BD>,
			u32,
		>;

		// 0> 1: _ _ X
		// |			 |> 3: 1
		// |			 |> 4: 1
		// |		 |> 5: 1
		// |> 2: _
		let mut states = test_states();
		let mut item: Tree<u32, u32, MapDiff<u8, u8>, D, BD> = InitFrom::init_from(((), ()));

		let successive_values: Vec<MapDelta<u8, u8>> = vec![
			MapDelta::default(), // (1, 1)
			MapDelta([(0, 1)][..].iter().cloned().collect()), // (1,2)
			MapDelta([(0, 1), (1, 3)][..].iter().cloned().collect()), // (3, 3)
			MapDelta::default(), // (1, 1)
		];

		let successive_deltas: Vec<MapDiff<u8, u8>> = vec![
			MapDiff::Reset(vec![]),
			MapDiff::Changes(vec![UnitDiff::Insert(0, 1)]),
			MapDiff::Changes(vec![UnitDiff::Insert(1, 3)]),
			MapDiff::Changes(vec![UnitDiff::Remove(0)]),
		];

		item.set(successive_deltas[0].clone(), &Latest::unchecked_latest((1, 1)));
		item.set(successive_deltas[1].clone(), &Latest::unchecked_latest((1, 2)));
		item.set(successive_deltas[2].clone(), &Latest::unchecked_latest((3, 3)));
		item.set(successive_deltas[3].clone(), &Latest::unchecked_latest((4, 3)));

		assert_eq!(item.get(&states.query_plan(1)).as_ref(), Some(&successive_deltas[1]));
		assert_eq!(item.get(&states.query_plan(3)).as_ref(), Some(&successive_deltas[2]));
		assert_eq!(item.get(&states.query_plan(4)).as_ref(), Some(&successive_deltas[3]));

		let item = TreeSum::<_, _, MapDelta<u8, u8>, _, _>(&item);
		assert_eq!(item.get_sum(&states.query_plan(1)).as_ref(), Some(&successive_values[1]));
		assert_eq!(item.get_sum(&states.query_plan(3)).as_ref(), Some(&successive_values[2]));
		assert_eq!(item.get_sum(&states.query_plan(4)).as_ref(), Some(&successive_values[3]));
	}
}
