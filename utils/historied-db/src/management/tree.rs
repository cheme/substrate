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

//! Implementation of state management for tree like
//! state.
//!
//! State changes are limited so resulting tree is rather unbalance.
//! This is best when there is not to many branch (fork)

use sp_std::ops::{AddAssign, SubAssign};
use sp_std::collections::btree_map::BTreeMap;
use sp_std::vec::Vec;
use sp_std::boxed::Box;
use sp_std::fmt::Debug;
use num_traits::One;
use crate::historied::linear::LinearGC;
use crate::{StateIndex, Latest};
use crate::management::{ManagementMut, Management, Migrate, ForkableManagement};
use codec::{Codec, Encode, Decode};
use crate::mapped_db::{MappedDB, Map as MappedDbMap, Variable as MappedDbVariable, MapInfo, VariableInfo};
use derivative::Derivative;

// TODO try removing Send + Sync here.
/// Base trait to give access to 'crate::mapped_db::MappedDB'
/// storage for `TreeManagement`.
///
/// If no backend storage is needed, a blank implementation
/// is provided for type '()'
pub trait TreeManagementStorage: Sized {
	/// Do we keep trace of changes. TODO rename JOURNAL_CHANGES
	const JOURNAL_DELETE: bool;
	type Storage: MappedDB;
	type Mapping: MapInfo;
	type JournalDelete: MapInfo;
	type TouchedGC: VariableInfo;
	type CurrentGC: VariableInfo;
	type LastIndex: VariableInfo;
	type NeutralElt: VariableInfo;
	type TreeMeta: VariableInfo;
	type TreeState: MapInfo;
}

impl TreeManagementStorage for () {
	const JOURNAL_DELETE: bool = false;
	type Storage = ();
	type Mapping = ();
	type JournalDelete = ();
	type TouchedGC = ();
	type CurrentGC = ();
	type LastIndex = ();
	type NeutralElt = ();
	type TreeMeta = ();
	type TreeState = ();
}

/// Trait defining a state for querying or modifying a branch.
/// This is therefore the representation of a branch state.
pub trait BranchContainer<I> {
	/// Get state for node at a given index.
	fn exists(&self, i: &I) -> bool;

	/// Get the last index for the state, inclusive.
	fn last_index(&self) -> I;
}

/// Stored states for a branch, it contains branch reference information,
/// structural information (index of parent branch) and fork tree building
/// information (is branch appendable).
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct BranchState<I, BI> {
	state: BranchRange<BI>,
	/// does a state get rollback.
	can_append: bool,
	/// is the branch latest.
	is_latest: bool,
	parent_branch_index: I,
}

impl<I, BI: Clone> BranchState<I, BI> {
	pub(crate) fn range(&self) -> (BI, BI) {
		(self.state.start.clone(), self.state.end.clone())
	}
}

/// This is a simple range, end non inclusive.
/// TODO type alias or use ops::Range? see next todo?
#[derive(Debug, Clone, Default, PartialEq, Eq, Encode, Decode)]
pub struct BranchRange<I> {
	// TODO rewrite this to use as single linear index?
	// we could always start at 0 but the state could not
	// be compared between branch which is sad.
	// Still start info is not that relevant, this is probably
	// removable.
	pub start: I,
	pub end: I,
}

/// Full state of current tree layout.
/// It contains all layout information for branches
/// states.
/// Branches are indexed by a sequential index.
/// Element of branches are indexed by a secondary
/// sequential indexes.
///
/// New branches index are defined by using `last_index`.
///
/// Also acts as a cache, storage can store
/// unknown db value as `None`.
///
/// NOTE that the single element branch at default index
/// containing the default branch index element does always
/// exist by convention.
#[derive(Derivative)]
#[derivative(Debug(bound="I: Debug, BI: Debug, S::Storage: Debug"))]
#[derivative(Clone(bound="I: Clone, BI: Clone, S::Storage: Clone"))]
#[cfg_attr(test, derivative(PartialEq(bound="I: PartialEq, BI: PartialEq, S::Storage: PartialEq")))]
pub struct Tree<I: Ord, BI, S: TreeManagementStorage> {
	// TODO this could probably be cleared depending on S::ACTIVE.
	// -> on gc ?
	/// Maps the different branches with their index.
	pub(crate) storage: MappedDbMap<I, BranchState<I, BI>, S::Storage, S::TreeState>,
	pub(crate) meta: MappedDbVariable<TreeMeta<I, BI>, S::Storage, S::TreeMeta>,
	/// serialize implementation
	pub(crate) serialize: S::Storage,
	// TODO some strategie to close a long branch that gets
	// behind multiple fork? This should only be usefull
	// for high number of modification, small number of
	// fork. The purpose is to avoid history where meaningfull
	// value is always in a low number branch behind a few fork.
	// A longest branch pointer per history is also a viable
	// strategy and avoid fragmenting the history to much.
	//
	// First optional BI is new end or delete, second is the previous range value.
	pub(crate) journal_delete: MappedDbMap<I, (Option<BI>, BranchRange<BI>), S::Storage, S::JournalDelete>,
}

#[derive(Derivative, Encode, Decode)]
#[derivative(Debug(bound="I: Debug, BI: Debug"))]
#[derivative(Clone(bound="I: Clone, BI: Clone"))]
#[cfg_attr(test, derivative(PartialEq(bound="I: PartialEq, BI: PartialEq")))]
pub(crate) struct TreeMeta<I, BI> {
	// TODO pub(crate) storage: MappedDbMap<I, BranchState<I, BI>>,
	pub(crate) last_index: I,
	/// treshold for possible node value, correspond
	/// roughly to last cannonical block branch index.
	/// If at default state value, we go through simple storage.
	/// TODO move in tree management??
	/// TODO only store BI, mgmt rule out all that is > bi and
	pub(crate) composite_treshold: (I, BI),
	/// Next value for composite treshold (requires data migration
	/// to switch current treshold but can already be use by gc).
	pub(crate) next_composite_treshold: Option<(I, BI)>,
	/// Pruned history index, all history before this cannot be queried.
	/// Those state can be pruned.
	pub(crate) pruning_treshold: Option<BI>,
	/// Is composite latest, so can we write its last state (only
	/// possible on new or after a migration).
	pub(crate) composite_latest: bool,
}

impl<I: Default, BI: Default> Default for TreeMeta<I, BI> {
	fn default() -> Self {
		TreeMeta {
			last_index: I::default(),
			composite_treshold: Default::default(),
			next_composite_treshold: None,
			pruning_treshold: None,
			composite_latest: true,
		}
	}
}

impl<I: Ord + Default, BI: Default, S: TreeManagementStorage> Default for Tree<I, BI, S>
	where
		I: Ord + Default,
		BI: Default,
		S: TreeManagementStorage,
		S::Storage: Default,
{
	fn default() -> Self {
		let serialize = S::Storage::default();
		let storage = MappedDbMap::default_from_db(&serialize);
		let journal_delete = MappedDbMap::default_from_db(&serialize);
		Tree {
			storage,
			journal_delete,
			meta: Default::default(),
			serialize,
		}
	}
}

impl<I: Ord + Default + Codec, BI: Default + Codec, S: TreeManagementStorage> Tree<I, BI, S> {
	pub fn from_ser(mut serialize: S::Storage) -> Self {
		let storage = MappedDbMap::default_from_db(&serialize);
		let journal_delete = MappedDbMap::default_from_db(&serialize);
		Tree {
			storage,
			journal_delete,
			meta: MappedDbVariable::from_ser(&mut serialize),
			serialize,
		}
	}
}

/// Gc against a current tree state.
/// This requires going through all of a historied value
/// branches and should be use when gc happens rarely.
#[derive(Clone, Debug)]
pub struct TreeStateGc<I, BI> {
	/// see Tree `storage`
	pub(crate) storage: BTreeMap<I, BranchState<I, BI>>,
	/// see TreeMeta `composite_treshold`
	pub(crate) composite_treshold: (I, BI),
	/// All data before this can get pruned for composite non forked part.
	pub(crate) pruning_treshold: Option<BI>,
}

/// Gc against a given set of changes.
/// This should be use when there is few state changes,
/// or frequent migration.
/// Generally if management collect those information (see associated
/// constant `JOURNAL_DELETE`) this gc should be use.
#[derive(Clone, Debug)]
pub struct DeltaTreeStateGc<I, BI> {
	/// Set of every branch that get reduced (new end stored) or deleted.
	pub(crate) storage: BTreeMap<I, (Option<BI>, BranchRange<BI>)>,
	/// New composite treshold value, this is not strictly needed but
	/// potentially allows skipping some iteration into storage.
	pub(crate) composite_treshold: (I, BI),
	/// All data before this can get pruned for composite non forked part.
	pub(crate) pruning_treshold: Option<BI>,
}

#[derive(Clone, Debug)]
pub enum MultipleGc<I, BI> {
	Journaled(DeltaTreeStateGc<I, BI>),
	State(TreeStateGc<I, BI>),
}

impl<I: Clone, BI: Clone + Ord + AddAssign<BI> + One> MultipleMigrate<I, BI> {
	/// Return upper limit (all state before it are touched),
	/// and explicit touched state.
	pub fn touched_state(&self) -> (Option<BI>, impl Iterator<Item = (I, BI)>) {

		let (pruning, touched) = match self {
			MultipleMigrate::JournalGc(gc) => {
				let iter = Some(
					gc.storage.clone().into_iter()
						.map(|(index, (change, old))| {
							let mut bindex = old.start;
							let end = old.end;
							sp_std::iter::from_fn(move || {
								if bindex < end {
									let result = Some(bindex.clone());
									bindex += BI::one();
									result
								} else {
									None
								}
							}).filter_map(move |branch_index| match change.as_ref() {
								Some(new_end) => if &branch_index >= new_end {
									Some((index.clone(), branch_index))
								} else {
									None
								},
								None => Some((index.clone(), branch_index)),
							})
						}).flatten()
				);
				(gc.pruning_treshold.clone(), iter)
			},
			MultipleMigrate::Rewrite(..)
				| MultipleMigrate::Noops => {
				(None, None)
			},
		};

		// TODO require storing original range un DeltaTreeStateGc for the iterator.
		// TODO when using in actual consumer, it means that journals need to be
		// stored ordered with (BI, I) as key (currently it is I, BI).
		// Note that iterating on all value will be ok there since we always got BI
		// incremental.
		(pruning, touched.into_iter().flatten())
	}
}

impl<I: Ord, BI, S: TreeManagementStorage> Tree<I, BI, S> {
	pub fn ser(&mut self) -> &mut S::Storage {
		&mut self.serialize
	}
}

#[derive(Derivative)]
#[derivative(Debug(bound="H: Debug, I: Debug, BI: Debug, S::Storage: Debug"))]
#[derivative(Clone(bound="H: Clone, I: Clone, BI: Clone, S::Storage: Clone"))]
#[cfg_attr(test, derivative(PartialEq(bound="H: PartialEq, I: PartialEq, BI: PartialEq, S::Storage: PartialEq")))]
pub struct TreeManagement<H: Ord, I: Ord, BI, S: TreeManagementStorage> {
	state: Tree<I, BI, S>,
	/// Map a given tag to its state index.
	ext_states: MappedDbMap<H, (I, BI), S::Storage, S::Mapping>,
	touched_gc: MappedDbVariable<bool, S::Storage, S::TouchedGC>, // TODO currently damned unused thing??
	current_gc: MappedDbVariable<TreeMigrate<I, BI>, S::Storage, S::CurrentGC>, // TODO currently unused??
	last_in_use_index: MappedDbVariable<((I, BI), Option<H>), S::Storage, S::LastIndex>, // TODO rename to last inserted as we do not rebase on query
}

#[derive(Derivative)]
#[derivative(Debug(bound="H: Debug, I: Debug, BI: Debug, S::Storage: Debug"))]
#[cfg_attr(test, derivative(PartialEq(bound="H: PartialEq, I: PartialEq, BI: PartialEq, S::Storage: PartialEq")))]
pub struct TreeManagementWithConsumer<H: Ord + 'static, I: Ord + 'static, BI: 'static, S: TreeManagementStorage + 'static> {
	inner: TreeManagement<H, I, BI, S>,
	#[derivative(Debug="ignore")]
	#[derivative(PartialEq="ignore")]
	registered_consumer: RegisteredConsumer<H, I, BI, S>,
}

impl<H: Ord, I: Ord, BI, S: TreeManagementStorage> sp_std::ops::Deref for TreeManagementWithConsumer<H, I, BI, S> {
	type Target = TreeManagement<H, I, BI, S>;
	fn deref(&self) -> &Self::Target {
		&self.inner
	}
}

impl<H: Ord, I: Ord, BI, S: TreeManagementStorage> sp_std::ops::DerefMut for TreeManagementWithConsumer<H, I, BI, S> {
	fn deref_mut(&mut self) -> &mut Self::Target {
		&mut self.inner
	}
}

impl<H: Ord, I: Ord, BI, S: TreeManagementStorage> From<TreeManagement<H, I, BI, S>> for TreeManagementWithConsumer<H, I, BI, S> {
	fn from(inner: TreeManagement<H, I, BI, S>) -> Self {
		TreeManagementWithConsumer {
			inner,
			registered_consumer: RegisteredConsumer(Vec::new()),
		}
	}
}

pub struct RegisteredConsumer<H: Ord + 'static, I: Ord + 'static, BI: 'static, S: TreeManagementStorage + 'static>(
	Vec<Box<dyn super::ManagementConsumer<H, TreeManagement<H, I, BI, S>>>>,
);

impl<H, I, BI, S> Default for RegisteredConsumer<H, I, BI, S>
	where
		H: Ord,
		I: Ord,
		S: TreeManagementStorage,
{
	fn default() -> Self {
		RegisteredConsumer(Vec::new())
	}
}

impl<H, I, BI, S> Default for TreeManagement<H, I, BI, S>
	where
		H: Ord,
		I: Default + Ord,
		BI: Default,
		S: TreeManagementStorage,
		S::Storage: Default,
{
	fn default() -> Self {
		let tree = Tree::default();
		let ext_states = MappedDbMap::default_from_db(&tree.serialize);
		TreeManagement {
			state: tree,
			ext_states,
			touched_gc: Default::default(),
			current_gc: Default::default(),
			last_in_use_index: Default::default(),
		}
	}
}

impl<H: Ord + Codec, I: Default + Ord + Codec, BI: Default + Codec, S: TreeManagementStorage> TreeManagement<H, I, BI, S> {
	/// Initialize from a default ser
	pub fn from_ser(serialize: S::Storage) -> Self {
		let ext_states = MappedDbMap::default_from_db(&serialize);
		TreeManagement {
			ext_states,
			touched_gc: MappedDbVariable::from_ser(&serialize),
			current_gc: MappedDbVariable::from_ser(&serialize),
			last_in_use_index: MappedDbVariable::from_ser(&serialize),
			state: Tree::from_ser(serialize),
		}
	}

	/// Also should guaranty to flush change (but currently implementation
	/// writes synchronously).
	pub fn extract_ser(self) -> S::Storage {
		self.state.serialize
	}

	pub fn ser(&mut self) -> &mut S::Storage {
		&mut self.state.serialize
	}

	pub fn ser_ref(&self) -> &S::Storage {
		&self.state.serialize
	}
}

impl<
	H: Clone + Ord + Codec,
	I: Clone + Default + SubAssign<I> + AddAssign<I> + Ord + Debug + Codec + One,
	BI: Ord + SubAssign<BI> + AddAssign<BI> + Clone + Default + Debug + Codec + One,
	S: TreeManagementStorage,
> TreeManagement<H, I, BI, S> {
	/// Associate a state for the initial root (default index).
	pub fn map_root_state(&mut self, root: H) {
		self.ext_states.mapping(self.state.ser()).insert(root, Default::default());
	}

	// TODO consider removing drop_ext_states argument (is probably default)
	// TODO this is unused (all apply_drop_state)
	pub fn apply_drop_state(
		&mut self,
		state: &(I, BI),
		mut drop_ext_states: bool,
		collect_dropped: Option<&mut Vec<H>>,
	) {
		drop_ext_states |= collect_dropped.is_some();
		let mut tree_meta = self.state.meta.mapping(&mut self.state.serialize).get().clone();
		// TODO optimized drop from I, BI == 0, 0 and ignore x, 0
		let ext_states = &mut self.ext_states;
		let mut no_collect = Vec::new();
		let collect_dropped = collect_dropped.unwrap_or(&mut no_collect);
		let mut call_back = move |i: &I, bi: &BI, ser: &mut S::Storage| {
			if drop_ext_states {
				let mut ext_states = ext_states.mapping(ser);
				let state = (i.clone(), bi.clone());
				let start = collect_dropped.len();
				// TODO again cost of reverse lookup: consider double ext_states
				if let Some(h) = ext_states.iter()
					.find(|(_k, v)| v == &state)
					.map(|(k, _v)| k.clone()) {
					collect_dropped.push(h);
				}
				for h in &collect_dropped[start..] {
					ext_states.remove(h);
				}
			}
		};
		// Less than composite treshold, we delete all and switch composite
		if state.1 <= tree_meta.composite_treshold.1 {
			// No branch delete (the implementation guaranty branch 0 is a single element)
			self.state.apply_drop_state_rec_call(&state.0, &state.1, &mut call_back, true);
			let treshold = tree_meta.composite_treshold.clone();
			self.last_in_use_index.mapping(self.state.ser()).set((treshold, None));

			if tree_meta.composite_latest == false {
				tree_meta.composite_latest = true;
				self.state.meta.mapping(&mut self.state.serialize).set(tree_meta);
			}
			return;
		}
		let mut previous_index = state.1.clone();
		previous_index -= BI::one();
		if let Some((parent, branch_end)) = self.state.branch_state(&state.0)
			.map(|s| if s.state.start <= previous_index {
				((state.0.clone(), previous_index), s.state.end)
			} else {
				((s.parent_branch_index.clone(), previous_index), s.state.end)
			}) {
			let mut bi = state.1.clone();
			// TODO consider moving this to tree `apply_drop_state`!! (others calls are at tree level)
			while bi < branch_end { // TODO should be < branch_end - 1
				call_back(&state.0, &bi, self.state.ser());
				bi += BI::one();
			}
			call_back(&state.0, &state.1, self.state.ser());
			self.state.apply_drop_state(&state.0, &state.1, &mut call_back);
			self.last_in_use_index.mapping(self.state.ser()).set((parent, None));
		}
	}

	pub fn apply_drop_from_latest(&mut self, back: BI, do_prune: bool) -> bool {
		let latest = self.last_in_use_index.mapping(self.state.ser()).get().clone();
		let mut switch_index = (latest.0).1.clone();
		switch_index -= back;
		let qp = self.state.query_plan_at(latest.0);
		let mut branch_index = self.state.meta.mapping(&mut self.state.serialize).get().composite_treshold.0.clone();
		for b in qp.iter() {
			if b.0.end <= switch_index {
				branch_index = b.1;
				break;
			}
		}
		let prune_index = if do_prune {
			Some(switch_index.clone())
		} else {
			None
		};
		self.canonicalize(qp, (branch_index, switch_index.clone()), prune_index)
	}

	// TODO subfunction in tree (more tree related)? This is a migrate (we change
	// composite_treshold).
	pub fn canonicalize(&mut self, branch: ForkPlan<I, BI>, switch_index: (I, BI), prune_index: Option<BI>) -> bool {
		// TODO makes last index the end of this canonicalize branch

		// TODO move fork plan resolution in?? -> wrong fork plan usage can result in incorrect
		// latest.

		// TODO EMCH keep if branch start index is before switch index, keep
		// only if part of fork plan (putting fork plan in a map as branch index are
		// unrelated to their start).
		// For branch that are in fork plan, if end index is more than the fork plan one (and less than
		// switch index), align.

		// TODO it may be reasonable most of the time to use forkplan index lookup up to some
		// treshold: may need variant depending on number of branch in the forkplan, or
		// have state trait and change name of `filter` to `cache` as it is a particular
		// use case.
		let mut filter: BTreeMap<_, _> = Default::default();
		for h in branch.history.into_iter() {
			//if h.state.end > switch_index.1 {
			if h.state.start < switch_index.1 {
				filter.insert(h.branch_index, h.state);
			}
		}
		let mut change = false;
		let mut to_change = Vec::new();
		let mut to_remove = Vec::new();
		for (branch_ix, mut branch) in self.state.storage.mapping(&mut self.state.serialize).iter() {
			if branch.state.start < switch_index.1 {
				if let Some(ref_range) = filter.get(&branch_ix) {
					debug_assert!(ref_range.start == branch.state.start);
					debug_assert!(ref_range.end <= branch.state.end);
					if ref_range.end < branch.state.end {
						let old = branch.state.clone();
						branch.state.end = ref_range.end.clone();
						branch.can_append = false;
						to_change.push((branch_ix, branch, old));
						// TODO EMCH clean ext_states for ends shifts
					}
				} else {
					to_remove.push((branch_ix.clone(), branch.state.clone()));
				}
			}
		}
		if to_remove.len() > 0 {
			change = true;
			for to_remove in to_remove {
				self.state.register_drop(&to_remove.0, to_remove.1, None);
				self.state.storage.mapping(&mut self.state.serialize).remove(&to_remove.0);
				// TODO EMCH clean ext_states for range -> in applied_migrate
			}
		}
		if to_change.len() > 0 {
			change = true;
			for (branch_ix, branch, old_branch) in to_change {
				self.state.register_drop(&branch_ix, old_branch, Some(branch.state.end.clone()));
				self.state.storage.mapping(&mut self.state.serialize).insert(branch_ix, branch);
			}
		}

		let mut mapping = self.state.meta.mapping(&mut self.state.serialize);
		let tree_meta = mapping.get();
		if switch_index != tree_meta.composite_treshold || prune_index.is_some() {
			let mut tree_meta = tree_meta.clone();
			tree_meta.next_composite_treshold = Some(switch_index);
			tree_meta.pruning_treshold = prune_index;
			mapping.set(tree_meta);
			change = true;
		}
		change
	}
}

impl<
	I: Clone + Default + SubAssign<I> + AddAssign<I> + Ord + Debug + Codec + One,
	BI: Ord + SubAssign<BI> + AddAssign<BI> + Clone + Default + Debug + Codec + One,
	H: Clone + Ord + Codec,
	S: TreeManagementStorage,
> TreeManagementWithConsumer<H, I, BI, S> {
	pub fn register_consumer(&mut self, consumer: Box<dyn super::ManagementConsumer<H, TreeManagement<H, I, BI, S>>>) {
		self.registered_consumer.0.push(consumer);
	}

	pub fn migrate(&mut self) {
		self.registered_consumer.migrate(&mut self.inner)
	}
}

impl<
	I: Clone + Default + SubAssign<I> + AddAssign<I> + Ord + Debug + Codec + One,
	BI: Ord + SubAssign<BI> + AddAssign<BI> + Clone + Default + Debug + Codec + One,
	H: Clone + Ord + Codec,
	S: TreeManagementStorage,
> RegisteredConsumer<H, I, BI, S> {
	pub fn register_consumer(&mut self, consumer: Box<dyn super::ManagementConsumer<H, TreeManagement<H, I, BI, S>>>) {
		self.0.push(consumer);
	}

	pub fn migrate(&self, mgmt: &mut TreeManagement<H, I, BI, S>) {
		// In this case (register consumer is design to run with sync backends), the management
		// lock is very likely to be ineffective.
		let mut migrate = mgmt.get_migrate();
		let need_migrate = match &migrate.1 {
			MultipleMigrate::Noops => false,
			_ => true,
		};
		if need_migrate {
			for consumer in self.0.iter() {
				consumer.migrate(&mut migrate);
			}
		}
		
		migrate.0.applied_migrate()
	}
}

	
impl<
	I: Clone + Default + SubAssign<I> + AddAssign<I> + Ord + Debug + Codec + One,
	BI: Ord + Default + SubAssign<BI> + AddAssign<BI> + Clone + Default + Debug + Codec + One,
	S: TreeManagementStorage,
> Tree<I, BI, S> {
	/// Return anchor index for this branch history:
	/// - same index as input if the branch was modifiable
	/// - new index in case of branch range creation
	pub fn add_state(
		&mut self,
		branch_index: I,
		number: BI,
	) -> Option<I> {
		let mut meta = self.meta.mapping(&mut self.serialize).get().clone();
		if number < meta.composite_treshold.1 {
			return None;
		}
		let mut create_new = false;
		if branch_index <= meta.composite_treshold.0 {
			// only allow terminal append
			let mut next = meta.composite_treshold.1.clone();
			next += BI::one();
			if number == next {
				if meta.composite_latest {
					meta.composite_latest = false;
				}
				create_new = true;
			} else {
				return None;
			}
		} else {
			let mut mapping = self.storage.mapping(&mut self.serialize);
			assert!(mapping.get(&branch_index).is_some(),
				"Inconsistent state on new block: {:?} {:?}, {:?}",
				branch_index,
				number,
				meta.composite_treshold,
			);
			let branch_state = mapping.entry(&branch_index);

			let mut can_fork = true;
			branch_state.and_modify(|branch_state| {
				if branch_state.can_append && branch_state.can_add(&number) {
					branch_state.add_state();
				} else {
					if !branch_state.can_fork(&number) {
						can_fork = false;
					} else {
						if branch_state.state.end == number {
							branch_state.is_latest = false;
						}
						create_new = true;
					}
				}
			});
			if !can_fork {
				return None;
			}
		}
		Some(if create_new {
			meta.last_index += I::one();
			let state = BranchState::new(number, branch_index);
			self.storage.mapping(&mut self.serialize).insert(meta.last_index.clone(), state);
			let result = meta.last_index.clone();

			self.meta.mapping(&mut self.serialize).set(meta);
			result
		} else {
			branch_index
		})
	}

	#[cfg(test)]
	pub fn unchecked_latest_at(&mut self, branch_index : I) -> Option<Latest<(I, BI)>> {
		{
			let mut mapping = self.meta.mapping(&mut self.serialize);
			let meta = mapping.get();
			if meta.composite_latest {
				// composite
				if branch_index <= meta.composite_treshold.0 {
					return Some(Latest::unchecked_latest(meta.composite_treshold.clone()));
				} else {
					return None;
				}
			}
		}
		self.storage.mapping(&mut self.serialize).get(&branch_index).map(|branch| {
			let mut end = branch.state.end.clone();
			end -= BI::one();
			Latest::unchecked_latest((branch_index, end))
		})
	}
	
	// TODO this and is_latest is borderline useless, for management implementation only.
	pub fn if_latest_at(&mut self, branch_index: I, seq_index: BI) -> Option<Latest<(I, BI)>> {
		{
			let mut mapping = self.meta.mapping(&mut self.serialize);
			let meta = mapping.get();
			if meta.composite_latest {
				// composite
				if branch_index <= meta.composite_treshold.0 && seq_index == meta.composite_treshold.1 {
					return Some(Latest::unchecked_latest(meta.composite_treshold.clone()));
				} else {
					return None;
				}
			}
		}
		self.storage.mapping(&mut self.serialize).get(&branch_index).and_then(|branch| {
			if !branch.is_latest {
				None
			} else {
				let mut end = branch.state.end.clone();
				end -= BI::one();
				if seq_index == end {
					Some(Latest::unchecked_latest((branch_index, end)))
				} else {
					None
				}
			}
		})
	}
	
	/// TODO doc & switch to &I
	pub fn query_plan_at(&mut self, (branch_index, mut index) : (I, BI)) -> ForkPlan<I, BI> {
		// make index exclusive
		index += BI::one();
		self.query_plan_inner(branch_index, Some(index))
	}
	/// TODO doc & switch to &I
	pub fn query_plan(&mut self, branch_index: I) -> ForkPlan<I, BI> {
		self.query_plan_inner(branch_index, None)
	}

	fn query_plan_inner(&mut self, mut branch_index: I, mut parent_fork_branch_index: Option<BI>) -> ForkPlan<I, BI> {
		let composite_treshold = self.meta.mapping(&mut self.serialize).get().composite_treshold.clone();
		let mut history = Vec::new();
		while branch_index >= composite_treshold.0 {
			if let Some(branch) = self.storage.mapping(&mut self.serialize).get(&branch_index) {
				let branch_ref = if let Some(end) = parent_fork_branch_index.take() {
					branch.query_plan_to(end)
				} else {
					branch.query_plan()
				};
				parent_fork_branch_index = Some(branch_ref.start.clone());
				if branch_ref.end > branch_ref.start {
					// vecdeque would be better suited
					history.insert(0, BranchPlan {
						state: branch_ref,
						branch_index: branch_index.clone(),
					});
				}
				branch_index = branch.parent_branch_index.clone();
			} else {
				break;
			}
		}
		ForkPlan {
			history,
			composite_treshold: composite_treshold,
		}
	}

	/// Return anchor index for this branch history:
	/// - same index as input if branch is not empty
	/// - parent index if branch is empty
	/// TODO is it of any use, we probably want to recurse.
	pub fn drop_state(
		&mut self,
		branch_index: &I,
	) -> Option<I> {
		let mut do_remove = None;
		{
			let mut mapping = self.storage.mapping(&mut self.serialize);
			let mut has_state = false;
			mapping.entry(branch_index).and_modify(|branch_state| {
				has_state = true;
				if branch_state.drop_state() {
					do_remove = Some(branch_state.parent_branch_index.clone());
				}
			});
			if !has_state {
				return None;
			}
		}

		Some(if let Some(parent_index) = do_remove {
			self.storage.mapping(&mut self.serialize).remove(branch_index);
			parent_index
		} else {
			branch_index.clone()
		})
	}

	pub fn branch_state(&mut self, branch_index: &I) -> Option<BranchState<I, BI>> {
		self.storage.mapping(&mut self.serialize).get(branch_index).cloned()
	}

	pub fn branch_state_mut<R, F: FnOnce(&mut BranchState<I, BI>) -> R>(&mut self, branch_index: &I, f: F) -> Option<R> {
		let mut result: Option<R> = None;
		self.storage.mapping(&mut self.serialize)
			.entry(branch_index)
			.and_modify(|s: &mut BranchState<I, BI>| {
				result = Some(f(s));
			});
		result
	}

	/// this function can go into deep recursion with full scan, it indicates
	/// that the tree model use here should only be use for small data or
	/// tests. TODO should apply call back here and remove from caller!!
	pub fn apply_drop_state(&mut self,
		branch_index: &I,
		node_index: &BI,
		call_back: &mut impl FnMut(&I, &BI, &mut S::Storage),
	) {
		// Never remove default
		let mut remove = false;
		let mut register = None;
		let mut last = Default::default();
		let mut has_branch = false;
		let mut mapping = self.storage.mapping(&mut self.serialize);
		let branch_entry = mapping.entry(branch_index);
		branch_entry.and_modify(|branch| {
			has_branch = true;
			branch.is_latest = true;
			last = branch.state.clone();
			while &branch.state.end > node_index {
				// TODO a function to drop multiple state in linear.
				if branch.drop_state() {
					remove = true;
					register = Some(None);
					break;
				}
			}
			if register.is_none() {
				register = Some(Some(node_index.clone()));
			}
		});
		if !has_branch {
			return;
		}
		if remove {
			self.storage.mapping(&mut self.serialize).remove(branch_index);
		}
		if let Some(register) = register {
			self.register_drop(branch_index, last.clone(), register);
		}
		while &last.end > node_index {
			last.end -= BI::one();
			self.apply_drop_state_rec_call(branch_index, &last.end, call_back, false);
		}
	}

	pub fn apply_drop_state_rec_call(&mut self,
		branch_index: &I,
		node_index: &BI,
		call_back: &mut impl FnMut(&I, &BI, &mut S::Storage),
		composite: bool,
	) {
		let mut to_delete = Vec::new();
		if composite {
			for (i, s) in self.storage.mapping(&mut self.serialize).iter() {
				if &s.state.start >= node_index {
					to_delete.push((i, s));
				}
			}
		} else {
			for (i, s) in self.storage.mapping(&mut self.serialize).iter() {
				if &s.parent_branch_index == branch_index && &s.state.start > node_index {
					to_delete.push((i, s));
				}
			}
		}
		for (i, s) in to_delete.into_iter() {
			self.register_drop(&i, s.state.clone(), None);
			// TODO these drop is a full branch drop: we could recurse on ourselves
			// into calling function and this function rec on itself and do its own drop
			let mut bi = s.state.start.clone();
			while bi < s.state.end {
				call_back(&i, &bi, &mut self.serialize);
				bi += BI::one();
			}
			self.storage.mapping(&mut self.serialize).remove(&i);
			// composite to false, as no in composite branch are stored.
			self.apply_drop_state_rec_call(&i, &s.state.start, call_back, false);
		}
	}

	fn register_drop(&mut self,
		branch_index: &I,
		branch_range: BranchRange<BI>,
		new_node_index: Option<BI>, // if none this is a delete
	) {
		if S::JOURNAL_DELETE {
			let mut journal_delete = self.journal_delete.mapping(&mut self.serialize);
			if let Some(new_node_index) = new_node_index {
				if let Some((to_insert, old_range)) = match journal_delete.get(branch_index) {
					Some((Some(old), old_range)) => if &new_node_index < old {
						// can use old range because the range gets read only on first
						// change.
						Some((new_node_index, old_range.clone()))
					} else {
						None
					},
					Some((None, _)) => None,
					None => Some((new_node_index, branch_range)),
				} {
					journal_delete.insert(branch_index.clone(), (Some(to_insert), old_range));
				}
			} else {
				journal_delete.insert(branch_index.clone(), (None, branch_range));
			}
		}
	}

	fn clear_journal_delete(&mut self) {
		let mut journal_delete = self.journal_delete.mapping(&mut self.serialize);
		// TODO remove ext_states for all delete!!
		journal_delete.clear()
	}

	// TODO should not be a function, this is very specific
	fn clear_composite(&mut self) {
		let mut to_remove = Vec::new();
		if let Some(composite_treshold) = self.meta.get().next_composite_treshold.clone() {
			for (ix, branch) in self.storage.iter(&mut self.serialize) {
				if branch.state.start < composite_treshold.1 {
					to_remove.push(ix.clone());
				}
			}
		}

		let mut storage = self.storage.mapping(&mut self.serialize);
		for i in to_remove {
			storage.remove(&i);
		}
	}

}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Query plane needed for operation for a given
/// fork.
/// This is a subset of the full branch set definition.
///
/// Values are ordered by branch_ix,
/// and only a logic branch path should be present.
///
/// Note that an alternative could be a pointer to the full state
/// a branch index corresponding to the leaf for the fork.
/// Here we use an in memory copy of the path because it seems
/// to fit query at a given state with multiple operations
/// (block processing), that way we iterate on a vec rather than
/// hoping over linked branches.
/// TODO small vec that ??
/// TODO add I treshold (everything valid starting at this one)?
pub struct ForkPlan<I, BI> {
	history: Vec<BranchPlan<I, BI>>,
	pub composite_treshold: (I, BI),
}

impl<I: Clone, BI: Clone + SubAssign<BI> + One> StateIndex<(I, BI)> for ForkPlan<I, BI> {
	fn index(&self) -> (I, BI) {
		// Extract latest state index use by the fork plan.
		// In other words latest state index is use to obtain this
		// plan.
		self.latest()
	}

	fn index_ref(&self) -> Option<&(I, BI)> {
		None
	}
}

// Note that this is fairly incorrect (we should bound on I : StateIndex),
// but very convenient.
// Otherwhise, could put SF into a wrapper type.
impl<I: Clone, BI: Clone> StateIndex<(I, BI)> for (I, BI) {
	fn index(&self) -> (I, BI) {
		self.clone()
	}

	fn index_ref(&self) -> Option<&(I, BI)> {
		Some(self)
	}
}

// TODO drop in favor of StateIndex impl.
impl<I: Clone, BI: Clone + SubAssign<BI> + One> ForkPlan<I, BI> {
	/// Extract latest state index use by the fork plan.
	pub fn latest_index(&self) -> (I, BI) {
		self.latest()
	}
	fn latest(&self) -> (I, BI) {
		if let Some(branch_plan) = self.history.last() {
			let mut index = branch_plan.state.end.clone();
			index -= BI::one();
			(branch_plan.branch_index.clone(), index)
		} else {
			self.composite_treshold.clone()
		}
	}
}

impl<I, BI: Clone + SubAssign<BI> + One + Default + Ord> ForkPlan<I, BI> {
	/// Calculate forkplan that does not include current state,
	/// very usefull to produce diff of value at a given state
	/// (we make the diff against the previous, not the current).
	pub fn previous_forkplan(mut self) -> Option<ForkPlan<I, BI>> {
		if self.history.len() > 0 {
			debug_assert!(self.history[0].state.start > self.composite_treshold.1);
			if let Some(branch) = self.history.last_mut() {
				branch.state.end -= One::one();
				if branch.state.end != branch.state.start {
					return Some(self);
				}
			}
			self.history.pop();
		} else if self.composite_treshold.1 == Default::default() {
			return None;
		} else {
			self.composite_treshold.1 -= One::one();
		}
		Some(self)
	}
}

impl<I: Default, BI: Default> Default for ForkPlan<I, BI> {
	fn default() -> Self {
		ForkPlan {
			history: Vec::new(),
			composite_treshold: Default::default(),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Query plan element for a single branch.
pub struct BranchPlan<I, BI> {
	// TODO rename to index
	pub branch_index: I,
	pub state: BranchRange<BI>,
}

impl<I, BI> ForkPlan<I, BI>
	where
		I: Default + Ord + Clone,
		BI: SubAssign<BI> + Ord + Clone + One,
{
	/// Iterator over the branch states in query order
	/// (more recent first).
	pub fn iter(&self) -> ForkPlanIter<I, BI> {
		ForkPlanIter(self, self.history.len())
	}
}

/// Iterator, contains index of last inner struct.
pub struct ForkPlanIter<'a, I, BI>(&'a ForkPlan<I, BI>, usize);

impl<'a, I: Clone, BI> Iterator for ForkPlanIter<'a, I, BI> {
	type Item = (&'a BranchRange<BI>, I);

	fn next(&mut self) -> Option<Self::Item> {
		if self.1 > 0 {
			self.1 -= 1;
			Some((
				&(self.0).history[self.1].state,
				(self.0).history[self.1].branch_index.clone(),
			))
		} else {
			None
		}
	}
}

impl<I: Ord> BranchRange<I> {
	fn exists(&self, i: &I) -> bool {
		i >= &self.start && i < &self.end
	}
}

impl<I: Default, BI: Default + AddAssign<u32>> Default for BranchState<I, BI> {

	// initialize with one element
	fn default() -> Self {
		let mut end = BI::default();
		end += 1;
		BranchState {
			state: BranchRange {
				start: Default::default(),
				end,
			},
			can_append: true,
			is_latest: true,
			parent_branch_index: Default::default(),
		}
	}
}

impl<I, BI: Ord + SubAssign<BI> + AddAssign<BI> + Clone + One> BranchState<I, BI> {

	pub fn query_plan(&self) -> BranchRange<BI> {
		self.state.clone()
	}

	pub fn query_plan_to(&self, end: BI) -> BranchRange<BI> {
		debug_assert!(self.state.end >= end);
		BranchRange {
			start: self.state.start.clone(),
			end,
		}
	}

	pub fn new(offset: BI, parent_branch_index: I) -> Self {
		let mut end = offset.clone();
		end += BI::one();
		BranchState {
			state: BranchRange {
				start: offset,
				end,
			},
			can_append: true,
			is_latest: true,
			parent_branch_index,
		}
	}

	/// Return true if you can add this index.
	pub fn can_add(&self, index: &BI) -> bool {
		index == &self.state.end
	}

 	pub fn can_fork(&self, index: &BI) -> bool {
		index <= &self.state.end && index > &self.state.start
	}

	pub fn add_state(&mut self) -> bool {
		if self.can_append {
			self.state.end += BI::one();
			true
		} else {
			false
		}
	}

	/// Return true if resulting branch is empty.
	pub fn drop_state(&mut self) -> bool {
		if self.state.end > self.state.start {
			self.state.end -= BI::one();
			self.can_append = false;
			if self.state.end == self.state.start {
				true
			} else {
				false
			}
		} else {
			true
		}
	}
}

#[derive(Debug, Clone, Encode, Decode)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct BranchGC<I, BI> {
	pub branch_index: I,
	/// A new start - end limit for the branch or a removed
	/// branch.
	pub new_range: Option<LinearGC<BI>>,
}


// TODO delete
#[derive(Debug, Clone, Encode, Decode)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct TreeMigrate<I, BI> {
	/// Every modified branch.
	/// Ordered by branch index.
	pub changes: Vec<BranchGC<I, BI>>,
}

/// Same as `DeltaTreeStateGc`, but also
/// indicates the changes journaling can be clean.
/// TODO requires a function returning all H indices.
pub struct TreeMigrateGC<I, BI> {
	pub gc: DeltaTreeStateGc<I, BI>,
	pub changed_composite_treshold: bool,
}

#[derive(Debug, Clone)]
/// A migration that swap some branch indices.
/// Note that we do not touch indices into branch.
pub struct TreeRewrite<I, BI> {
	/// Original branch index (and optionally a treshold) mapped to new branch index or deleted.
	pub rewrite: Vec<((I, Option<BI>), Option<I>)>,
	/// Possible change in composite treshold.
	pub composite_treshold: (I, BI),
	pub changed_composite_treshold: bool,
	/// All data before this can get pruned.
	pub pruning_treshold: Option<BI>,
}

#[derive(Debug, Clone)]
pub enum MultipleMigrate<I, BI> {
	JournalGc(DeltaTreeStateGc<I, BI>),
	Rewrite(TreeRewrite<I, BI>),
	Noops,
}

impl<I, BI> Default for TreeMigrate<I, BI> {
	fn default() -> Self {
		TreeMigrate {
			changes: Vec::new(),
		}
	}
}

impl<
	H: Ord + Clone + Codec,
	I: Clone + Default + SubAssign<I> + AddAssign<I> + Ord + Debug + Codec + One,
	BI: Ord + SubAssign<BI> + AddAssign<BI> + Clone + Default + Debug + Codec + One,
	S: TreeManagementStorage,
> TreeManagement<H, I, BI, S> {
	fn get_inner_gc(&self) -> Option<MultipleGc<I, BI>> {
		let tree_meta = self.state.meta.get();
		let composite_treshold = tree_meta.next_composite_treshold.clone()
			.unwrap_or(tree_meta.composite_treshold.clone());
		let pruning_treshold = tree_meta.pruning_treshold.clone();
		let gc = if Self::JOURNAL_DELETE {
			let mut storage = BTreeMap::new();
			for (k, v) in self.state.journal_delete.iter(&self.state.serialize) {
				storage.insert(k, v);
			}

			if pruning_treshold.is_none() && storage.is_empty() {
				return None;
			}

			let gc = DeltaTreeStateGc {
				storage,
				composite_treshold,
				pruning_treshold,
			};

			MultipleGc::Journaled(gc)
		} else {
			let mut storage: BTreeMap<I, BranchState<I, BI>> = Default::default();

			// TODO can have a ref to the serialized collection instead (if S is ACTIVE)
			// or TODO restor to ref of treestate if got non mutable interface for access.
			//  + could remove default and codec of V
			for (ix, v) in self.state.storage.iter(&self.state.serialize) {
				storage.insert(ix, v);
			}
			let gc = TreeStateGc {
				storage,
				composite_treshold,
				pruning_treshold,
			};
			MultipleGc::State(gc)
		};

		Some(gc)
	}
}
	
impl<H, I, BI, S> Management<H> for TreeManagement<H, I, BI, S>
	where
		H: Ord + Clone + Codec,
		I: Clone + Default + SubAssign<I> + AddAssign<I> + Ord + Debug + Codec + One,
		BI: Ord + SubAssign<BI> + AddAssign<BI> + Clone + Default + Debug + Codec + One,
		S: TreeManagementStorage,
{
	type Index = (I, BI);
	type S = ForkPlan<I, BI>;
	/// Garbage collect over current
	/// state or registered changes.
	/// Choice is related to `TreeManagementStorage::JOURNAL_DELETE`.
	type GC = MultipleGc<I, BI>;

	fn get_internal_index(&mut self, tag: &H) -> Option<Self::Index> {
		self.ext_states.mapping(self.state.ser()).get(tag).cloned()
	}

	fn get_db_state(&mut self, tag: &H) -> Option<Self::S> {
		self.ext_states.mapping(self.state.ser()).get(tag).cloned().map(|i| self.state.query_plan_at(i))
	}

	fn reverse_lookup(&mut self, index: &Self::Index) -> Option<H> {
		// TODO Note, from a forkplan we need to use 'latest' to get same
		// behavior as previous implementation.
		self.ext_states.mapping(self.state.ser()).iter()
			.find(|(_k, v)| v == index)
			.map(|(k, _v)| k.clone())
	}

	fn get_gc(&self) -> Option<crate::Ref<Self::GC>> {
		self.get_inner_gc().map(|gc| crate::Ref::Owned(gc))
	}
}

impl<
	H: Clone + Ord + Codec,
	I: Clone + Default + SubAssign<I> + AddAssign<I> + Ord + Debug + Codec + One,
	BI: Ord + SubAssign<BI> + AddAssign<BI> + Clone + Default + Debug + Codec + One,
	S: TreeManagementStorage,
> ManagementMut<H> for TreeManagement<H, I, BI, S> {
	// TODO attach gc infos to allow some lazy cleanup (make it optional)
	// on set and on get_mut
	type SE = Latest<(I, BI)>;

	/// TODO this needs some branch index ext_statess.
	type Migrate = MultipleMigrate<I, BI>;
	//type Migrate = TreeMigrate<I, BI, V>;

	fn get_db_state_mut(&mut self, tag: &H) -> Option<Self::SE> {
		self.ext_states.mapping(self.state.ser()).get(tag).cloned().and_then(|(i, bi)| {
			// enforce only latest
			self.state.if_latest_at(i, bi)
		})
	}
	
	fn latest_state(&mut self) -> Self::SE {
		let latest = self.last_in_use_index.mapping(self.state.ser()).get().clone();
		Latest::unchecked_latest(latest.0)
	}

	fn latest_external_state(&mut self) -> Option<H> {
		let latest = self.last_in_use_index.mapping(self.state.ser()).get().clone();
		latest.1
	}

	fn force_latest_external_state(&mut self, state: H) {
		let mut latest = self.last_in_use_index.mapping(self.state.ser()).get().clone();
		latest.1 = Some(state);
		self.last_in_use_index.mapping(self.state.ser()).set(latest);
	}

	fn get_migrate(&mut self) -> Migrate<H, Self> {
		let migrate = if S::JOURNAL_DELETE {
			// initial migrate strategie is gc.
			if let Some(MultipleGc::Journaled(gc)) = self.get_inner_gc() {
				MultipleMigrate::JournalGc(gc)
			} else {
				MultipleMigrate::Noops
			}
		} else {
			unimplemented!();
		};

		Migrate(self, migrate, sp_std::marker::PhantomData)
	}

	fn applied_migrate(&mut self) {
		if S::JOURNAL_DELETE {
			self.state.clear_journal_delete();
			self.state.clear_composite();
			let mut meta_change = false;
			let mut mapping = self.state.meta.mapping(&mut self.state.serialize);
			let mut tree_meta = mapping.get().clone();
			if let Some(treshold) = tree_meta.next_composite_treshold.take() {
				tree_meta.composite_treshold = treshold;
				meta_change = true;
			}
			if tree_meta.pruning_treshold.take().is_some() {
				meta_change = true;
			}
			if meta_change {
				mapping.set(tree_meta);
			}
		}
		
	//	self.current_gc.applied(gc); TODO pass back this reference: put it in buf more likely
	//	(remove the associated type)
		self.touched_gc.mapping(self.state.ser()).set(false);
	}
}

impl<
	H: Clone + Ord + Codec,
	I: Clone + Default + SubAssign<I> + AddAssign<I> + Ord + Debug + Codec + One,
	BI: Ord + SubAssign<BI> + AddAssign<BI> + Clone + Default + Debug + Codec + One,
	S: TreeManagementStorage,
> ForkableManagement<H> for TreeManagement<H, I, BI, S> {
	const JOURNAL_DELETE: bool = S::JOURNAL_DELETE;

	type SF = (I, BI);

	fn from_index(index: (I, BI)) -> Self::SF {
		index
	}

	fn init_state_fork(&mut self) -> Self::SF {
		let se = Latest::unchecked_latest(self.state.meta.get().composite_treshold.clone());
		Self::from_index(se.index())
	}

	fn get_db_state_for_fork(&mut self, state: &H) -> Option<Self::SF> {
		self.ext_states.mapping(self.state.ser()).get(state).cloned()
	}

	// note that se must be valid.
	fn append_external_state(&mut self, state: H, at: &Self::SF) -> Option<Self::SE> {
		let (branch_index, index) = at;
		let mut index = index.clone();
		index += BI::one();
		if let Some(branch_index) = self.state.add_state(branch_index.clone(), index.clone()) {
			let last_in_use_index = (branch_index.clone(), index);
			self.last_in_use_index.mapping(self.state.ser())
				.set((last_in_use_index.clone(), Some(state.clone())));
			self.ext_states.mapping(self.state.ser()).insert(state, last_in_use_index.clone());
			Some(Latest::unchecked_latest(last_in_use_index))
		} else {
			None
		}
	}

	fn drop_state(&mut self, state: &Self::SF, return_dropped: bool) -> Option<Vec<H>> {
		let mut result = if return_dropped {
			Some(Vec::new())
		} else {
			None
		};
		self.apply_drop_state(state, true, result.as_mut());
		result
	}
}

#[cfg(test)]
pub(crate) mod test {
	use super::*;

	/// Test state used by management test, no mappings.
	pub(crate) type TestState = Tree<u32, u32, ()>;
	/// Test state used by management test, with test mappings.
	pub(crate) type TestStateMapping = Tree<u32, u32, crate::test::MappingTests>;

	pub(crate) fn test_states() -> TestState {
		test_states_inner()
	}

	pub(crate) fn test_states_st() -> TestStateMapping {
		test_states_inner()
	}
	
	// TODO switch to management function?
	pub(crate) fn test_states_inner<T: TreeManagementStorage>() -> Tree<u32, u32, T>
		where T::Storage: Default,
	{
		let mut states = Tree::default();
		assert_eq!(states.add_state(0, 1), Some(1));
		// root branching.
		assert_eq!(states.add_state(0, 1), Some(2));
		assert_eq!(Some(true), states.branch_state_mut(&1, |ls| ls.add_state()));
		assert_eq!(Some(true), states.branch_state_mut(&1, |ls| ls.add_state()));
		assert_eq!(states.add_state(1, 3), Some(3));
		assert_eq!(states.add_state(1, 3), Some(4));
		assert_eq!(states.add_state(1, 2), Some(5));
		assert_eq!(states.add_state(2, 2), Some(2));
		assert_eq!(Some(1), states.drop_state(&1));
		// cannot create when dropped happen on branch
		assert_eq!(Some(false), states.branch_state_mut(&1, |ls| ls.add_state()));

		assert!(states.branch_state(&1).unwrap().state.exists(&1));
		assert!(states.branch_state(&1).unwrap().state.exists(&2));
		assert!(!states.branch_state(&1).unwrap().state.exists(&3));
		// 0> 1: _ _ X
		// |			 |> 3: 1
		// |			 |> 4: 1
		// |		 |> 5: 1
		// |> 2: _ _
		states
	}

	#[test]
	fn test_serialize() {
		let states = test_states_st();
		let storage = states.serialize.clone();
		let mut states = TestStateMapping::from_ser(storage);
		// just replaying the three last test of test_states_inner
		assert!(states.branch_state(&1).unwrap().state.exists(&1));
		assert!(states.branch_state(&1).unwrap().state.exists(&2));
		assert!(!states.branch_state(&1).unwrap().state.exists(&3));
	}

	#[test]
	fn test_remove_attached() {
		let mut states = test_states();
		assert_eq!(Some(false), states.branch_state_mut(&1, |ls| ls.drop_state()));
		// does not recurse
		assert!(states.branch_state(&3).unwrap().state.exists(&3));
		assert!(states.branch_state(&4).unwrap().state.exists(&3));
		assert!(states.branch_state(&5).unwrap().state.exists(&2));
		let mut states = test_states();
		states.apply_drop_state(&1, &2, &mut |_i, _bi, _ser| {});
		// does recurse
		assert_eq!(states.branch_state(&3), None);
		assert_eq!(states.branch_state(&4), None);
		assert!(states.branch_state(&5).unwrap().state.exists(&2));
	}

	#[test]
	fn test_query_plans() {
		let mut states = test_states();
		let ref_3 = vec![
			BranchPlan {
				branch_index: 1,
				state: BranchRange { start: 1, end: 3 },
			},
			BranchPlan {
				branch_index: 3,
				state: BranchRange { start: 3, end: 4 },
			},
		];
		assert_eq!(states.query_plan(3).history, ref_3);

		let mut states = states;

		assert_eq!(states.add_state(1, 2), Some(6));
		let ref_6 = vec![
			BranchPlan {
				branch_index: 1,
				state: BranchRange { start: 1, end: 2 },
			},
			BranchPlan {
				branch_index: 6,
				state: BranchRange { start: 2, end: 3 },
			},
		];
		assert_eq!(states.query_plan(6).history, ref_6);

		let mut meta = states.meta.mapping(&mut states.serialize).get().clone();
		meta.composite_treshold = (2, 1);
		states.meta.mapping(&mut states.serialize).set(meta);

		let mut ref_6 = ref_6;
		ref_6.remove(0);
		assert_eq!(states.query_plan(6).history, ref_6);
	}
}
