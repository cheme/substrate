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

//! Filters associated to externalities access.
//! These are used to ensure declarative workers accesses are correct.
//! It is plugged in the overlay by commodity, but could be at a higher level.

use crate::overlayed_changes::end_prefix;
use failure::{DeclFailureHandling, FailureHandlers};
use sp_core::storage::{ChildInfo, ChildType, PrefixedStorageKey};
use sp_externalities::{
	AccessDeclaration, AccessDeclarations, DeclarationFailureHandling, LocalAccessDeclaration,
	TaskId, WorkerResult, WorkerState, WorkerType,
};
use sp_std::{
	collections::{btree_map::BTreeMap, btree_set::BTreeSet},
	fmt::Debug,
	vec,
	vec::Vec,
};

/// Worker declaration assertion failure.
const BROKEN_DECLARATION_ACCESS: &str = "Key access impossible due to worker access declaration";

type FilterTree<F> = BTreeMap<Vec<u8>, F>; // TODO hashmap in std
/// A tree of filter rules.
#[derive(Debug, Clone, Default)]
pub(super) struct FilterTrees<F: Debug + Clone + Default> {
	pub(super) top: FilterTree<F>,
	pub(super) children: BTreeMap<super::StorageKey, FilterTree<F>>,
}

impl<F: Debug + Clone + Default> FilterTrees<F> {
	pub(super) fn filter(&self, child_info: Option<&ChildInfo>) -> Option<&FilterTree<F>> {
		if let Some(child_info) = child_info {
			self.children.get(child_info.storage_key())
		} else {
			Some(&self.top)
		}
	}
}

// Allow for child worker.
type FilterTreesAllow = FilterTrees<bool>;
type FilterTreeAllow = FilterTree<bool>;

// Forbid for parent worker. Forbid always override allow.
type FilterTreesForbid = FilterTrees<FilterOrigin>;
type FilterTreeForbid = FilterTree<FilterOrigin>;

// TODO same consuming function?
fn prefixed_storage_key_to_child_info(prefixed_storage_key: &Vec<u8>) -> ChildInfo {
	let prefixed_key = PrefixedStorageKey::new_ref(prefixed_storage_key);
	match ChildType::from_prefixed_key(prefixed_key) {
		Some((ChildType::ParentKeyId, storage_key)) => ChildInfo::new_default(storage_key),
		None => panic!("Unsupported child key"), // actually should be unreachable.
	}
}

/// Main filter state.
/// TODO default child access
#[derive(Debug, Clone)]
pub(super) struct Filters {
	failure_handlers: FailureHandlers,
	allow_read_active: bool,
	allow_write_active: bool,
	/* TODO instead of commented code, have a Allow child by prefix and forbid child by prefix
	 * for parent: then all are allowed like if empty prefix defined in them.
	 * Currently allow enable means we need to specify each child but we may not know.
	/// if true all child state are forbid.
	allow_read_child_active: bool,
	allow_write_child_active: bool,
	*/
	/// accesses allowed by parent.
	filters_allow_read: FilterTreesAllow,
	/// accesses allowed by parent.
	filters_allow_write: FilterTreesAllow,
	/// accesses forbid due to child.
	filters_forbid_read: FilterTreesForbid,
	/// accesses forbid due to child.
	filters_forbid_write: FilterTreesForbid,
	/// accesses allowed by parent.
	filters_interval_allow_read: interval_filters::FilterTreesAllowInterval,
	/// accesses allowed by parent.
	filters_interval_allow_write: interval_filters::FilterTreesAllowInterval,
	/// accesses forbid due to child.
	filters_interval_forbid_read: interval_filters::FilterTreesForbidInterval,
	/// accesses forbid due to child.
	filters_interval_forbid_write: interval_filters::FilterTreesForbidInterval,
	/// Keeping history to remove constraint on join or dismiss.
	changes: BTreeMap<TaskId, Vec<AccessDeclarations>>,
}

impl Default for Filters {
	fn default() -> Self {
		Filters {
			failure_handlers: FailureHandlers {
				parent_failure_handler: DeclFailureHandling::default(),
				children_failure_handler: BTreeMap::new(),
			},
			allow_read_active: Default::default(),
			allow_write_active: Default::default(),
			filters_allow_read: Default::default(),
			filters_allow_write: Default::default(),
			filters_forbid_read: Default::default(),
			filters_forbid_write: Default::default(),
			filters_interval_allow_read: Default::default(),
			filters_interval_allow_write: Default::default(),
			filters_interval_forbid_read: Default::default(),
			filters_interval_forbid_write: Default::default(),
			changes: BTreeMap::new(),
		}
	}
}

impl Filters {
	pub(super) fn set_failure_handler(
		&mut self,
		from_child: Option<TaskId>,
		failure: DeclarationFailureHandling,
	) {
		self.failure_handlers.set_failure_handler(from_child, failure);
	}

	fn remove_filter(tree: &mut FilterTreesForbid, filter: &AccessDeclaration, from_child: TaskId) {
		Self::remove_filter_internal(&mut tree.top, &filter.top, from_child);
		for (prefixed_storage_key, filter_child) in filter.children.iter() {
			if let Some(child) = tree.children.get_mut(prefixed_storage_key) {
				Self::remove_filter_internal(child, &filter_child, from_child);
			}
		}
	}

	fn remove_filter_internal(
		tree: &mut FilterTreeForbid,
		filter: &LocalAccessDeclaration,
		from_child: TaskId,
	) {
		for key in filter.keys_lock.iter() {
			if let Some(filter) = tree.get_mut(key.as_slice()) {
				filter.remove(from_child);
				if filter.is_empty() {
					tree.remove(key.as_slice());
				}
			}
		}
	}

	fn set_filter_forbid(
		tree: &mut FilterTreesForbid,
		filter: &AccessDeclaration,
		from_child: TaskId,
	) -> bool {
		if !Self::set_filter_forbid_internal(&mut tree.top, &filter.top, from_child) {
			return false
		}
		for (prefixed_storage_key, filter_child) in filter.children.iter() {
			let mut child = tree.children.entry(prefixed_storage_key.clone()).or_default();
			if !Self::set_filter_forbid_internal(&mut child, &filter_child, from_child) {
				return false
			}
		}
		true
	}

	fn set_filter_forbid_internal(
		tree: &mut FilterTreeForbid,
		filter: &LocalAccessDeclaration,
		from_child: TaskId,
	) -> bool {
		for key in filter.keys_lock.iter() {
			// TODO return false if unordered
			if let Some(filter) = tree.get_mut(key.as_slice()) {
				filter.set(from_child);
			} else {
				// new entry
				let mut filter = FilterOrigin::default();
				filter.set(from_child);
				tree.insert(key.clone(), filter);
			}
		}
		true
	}

	fn set_filter_allow(tree: &mut FilterTreesAllow, filter: &AccessDeclaration) -> bool {
		if !Self::set_filter_allow_internal(&mut tree.top, &filter.top) {
			return false
		}
		for (prefixed_storage_key, filter_child) in filter.children.iter() {
			let mut child = tree.children.entry(prefixed_storage_key.clone()).or_default();
			if !Self::set_filter_allow_internal(&mut child, filter_child) {
				return false
			}
		}
		true
	}

	fn set_filter_allow_internal(
		tree: &mut FilterTreeAllow,
		filter: &LocalAccessDeclaration,
	) -> bool {
		for key in filter.keys_lock.iter() {
			// TODO return false if unordered
			if let Some(filter) = tree.get_mut(key.as_slice()) {
				*filter = true;
			} else {
				// new entry
				tree.insert(key.clone(), true);
			}
		}
		true
	}

	pub(super) fn allow_all(&mut self, filter: AccessDeclaration) {
		self.allow_write_active = true;
		interval_filters::set_filter_allow(&mut self.filters_interval_allow_read, &filter); // TODO return false if wrong declaration same for others allow
		interval_filters::set_filter_allow(&mut self.filters_interval_allow_write, &filter);
		Self::set_filter_allow(&mut self.filters_allow_read, &filter);
		Self::set_filter_allow(&mut self.filters_allow_write, &filter);
	}

	pub(super) fn allow_writes(&mut self, filter: AccessDeclaration) {
		self.allow_write_active = true;
		interval_filters::set_filter_allow(&mut self.filters_interval_allow_write, &filter);
		Self::set_filter_allow(&mut self.filters_allow_write, &filter);
	}

	pub(super) fn allow_reads(&mut self, filter: AccessDeclaration) {
		self.allow_read_active = true;
		interval_filters::set_filter_allow(&mut self.filters_interval_allow_read, &filter);
		Self::set_filter_allow(&mut self.filters_allow_read, &filter);
	}

	pub(super) fn add_change(&mut self, decl: AccessDeclarations, from_child: TaskId) {
		self.changes.insert(from_child, vec![decl]);
	}

	pub(super) fn forbid_all(&mut self, filter: AccessDeclaration, from_child: TaskId) {
		interval_filters::set_filter_forbid(
			&mut self.filters_interval_forbid_read,
			&filter,
			from_child,
		);
		interval_filters::set_filter_forbid(
			&mut self.filters_interval_forbid_write,
			&filter,
			from_child,
		);
		Self::set_filter_forbid(&mut self.filters_forbid_read, &filter, from_child);
		Self::set_filter_forbid(&mut self.filters_forbid_write, &filter, from_child);
	}

	fn remove_forbid_all(&mut self, filter: AccessDeclaration, from_child: TaskId) {
		interval_filters::remove_filter_forbid(
			&mut self.filters_interval_forbid_read,
			&filter,
			from_child,
		);
		interval_filters::remove_filter_forbid(
			&mut self.filters_interval_forbid_write,
			&filter,
			from_child,
		);
		Self::remove_filter(&mut self.filters_forbid_read, &filter, from_child);
		Self::remove_filter(&mut self.filters_forbid_write, &filter, from_child);
	}

	pub(super) fn forbid_reads(&mut self, filter: AccessDeclaration, from_child: TaskId) {
		interval_filters::set_filter_forbid(
			&mut self.filters_interval_forbid_read,
			&filter,
			from_child,
		);
		Self::set_filter_forbid(&mut self.filters_forbid_read, &filter, from_child);
	}

	fn remove_forbid_reads(&mut self, filter: AccessDeclaration, from_child: TaskId) {
		interval_filters::remove_filter_forbid(
			&mut self.filters_interval_forbid_read,
			&filter,
			from_child,
		);
		Self::remove_filter(&mut self.filters_forbid_read, &filter, from_child);
	}

	pub(super) fn forbid_writes(&mut self, filter: AccessDeclaration, from_child: TaskId) {
		interval_filters::set_filter_forbid(
			&mut self.filters_interval_forbid_write,
			&filter,
			from_child,
		);
		Self::set_filter_forbid(&mut self.filters_forbid_write, &filter, from_child);
	}

	pub(super) fn remove_forbid_writes(&mut self, filter: AccessDeclaration, from_child: TaskId) {
		interval_filters::remove_filter_forbid(
			&mut self.filters_interval_forbid_write,
			&filter,
			from_child,
		);
		Self::remove_filter(&mut self.filters_forbid_write, &filter, from_child);
	}

	// Declaring a child read access, we ensure access is allowed in the first place.
	pub(super) fn guard_declare_child_filter_read(&mut self, filter: &AccessDeclaration) -> bool {
		if !self.guard_declare_child_filter_read_inner(None, &filter.top) {
			return false
		}
		for (prefixed_storage_key, filter) in filter.children.iter() {
			let child_info = prefixed_storage_key_to_child_info(prefixed_storage_key);
			if !self.guard_declare_child_filter_read_inner(Some(&child_info), filter) {
				return false
			}
		}
		true
	}

	fn guard_declare_child_filter_read_inner(
		&mut self,
		child_info: Option<&ChildInfo>,
		filter: &LocalAccessDeclaration,
	) -> bool {
		for key in filter.keys_lock.iter() {
			if !self.guard_read_inner(child_info, key, true) {
				return false
			}
		}

		for interval in filter.intervals_lock.iter().map(|(start, end)| (start, end.clone())).chain(
			filter.prefixes_lock.iter().map(|prefix| {
				let end = end_prefix(prefix.as_slice());
				(prefix, end)
			}),
		) {
			if !self.guard_interval_inner(
				child_info,
				interval.0.as_slice(),
				interval.1.as_ref().map(|e| e.as_slice()),
				true,
				true,
				false,
			) {
				return false
			}
		}
		true
	}

	pub(super) fn guard_declare_child_filter_write(&mut self, filter: &AccessDeclaration) -> bool {
		if !self.guard_declare_child_filter_write_inner(None, &filter.top) {
			return false
		}
		for (prefixed_storage_key, filter) in filter.children.iter() {
			let child_info = prefixed_storage_key_to_child_info(prefixed_storage_key);
			if !self.guard_declare_child_filter_write_inner(Some(&child_info), filter) {
				return false
			}
		}
		true
	}

	fn guard_declare_child_filter_write_inner(
		&mut self,
		child_info: Option<&ChildInfo>,
		filter: &LocalAccessDeclaration,
	) -> bool {
		for key in filter.keys_lock.iter() {
			if !self.guard_write_inner(child_info, key, true) {
				return false
			}
		}
		for interval in filter.intervals_lock.iter().map(|(start, end)| (start, end.clone())).chain(
			filter.prefixes_lock.iter().map(|prefix| {
				let end = end_prefix(prefix.as_slice());
				(prefix, end)
			}),
		) {
			if !self.guard_interval_inner(
				child_info,
				interval.0.as_slice(),
				interval.1.as_ref().map(|e| e.as_slice()),
				true,
				false,
				true,
			) {
				return false
			}
		}
		true
	}

	pub(super) fn on_worker_result(&mut self, result: &WorkerResult) -> bool {
		let did_fail = self.failure_handlers.did_fail();
		match result.state {
			WorkerState::CallAt => {
				self.remove_worker(result.id);
			},
			// When using filter there is no directly valid case.
			WorkerState::Valid => (),
			// When using filter there is no invalid case.
			WorkerState::Invalid => (),
			WorkerState::NeedResolve => unimplemented!("TODO"),
			// will panic on resolve so no need to clean filter
			WorkerState::RuntimePanic | WorkerState::HardPanic => (),
		};
		!did_fail
	}

	fn children(&self) -> BTreeSet<ChildInfo> {
		// TODO memoize it.!!!! or not usefull (some code is ok without)
		let mut children: BTreeSet<ChildInfo> = Default::default();
		self.filters_forbid_read.children.iter().for_each(|(storage_key, _child)| {
			children.insert(prefixed_storage_key_to_child_info(storage_key));
		});
		self.filters_forbid_write.children.iter().for_each(|(storage_key, _child)| {
			children.insert(prefixed_storage_key_to_child_info(storage_key));
		});
		if self.allow_read_active {
			self.filters_allow_read.children.iter().for_each(|(storage_key, _child)| {
				children.insert(prefixed_storage_key_to_child_info(storage_key));
			});
		}
		if self.allow_write_active {
			self.filters_allow_write.children.iter().for_each(|(storage_key, _child)| {
				children.insert(prefixed_storage_key_to_child_info(storage_key));
			});
		}
		children
	}

	pub(super) fn guard_read_all(&mut self) {
		self.guard_read_interval(None, &[], None);

		// costy alloc for mutable reference question, could be optimize.
		for child_info in self.children().into_iter() {
			self.guard_read_interval(Some(&child_info), &[], None);
		}
	}

	/// Guard invalid access for reading a key.
	pub(super) fn guard_read(&mut self, child_info: Option<&ChildInfo>, key: &[u8]) {
		let _ = self.guard_read_inner(child_info, key, false);
	}

	fn guard_read_inner(
		&mut self,
		child_info: Option<&ChildInfo>,
		key: &[u8],
		ret_result: bool,
	) -> bool {
		let blocked = Self::key_read_forbid(
			&self.filters_forbid_read,
			&self.filters_interval_forbid_read,
			child_info,
			key,
		);
		for origin in blocked {
			if ret_result {
				return false
			}
			self.failure_handlers.invalid_accesses(origin);
		}
		if self.allow_read_active {
			if !Self::guard_read_allow(
				&self.filters_allow_read,
				&self.filters_interval_allow_read,
				child_info,
				key,
			) {
				if ret_result {
					return false
				}
				self.failure_handlers.invalid_allowed_access();
			}
		}
		true
	}

	/// Guard invalid access for writing a key.
	pub(super) fn guard_write(&mut self, child_info: Option<&ChildInfo>, key: &[u8]) {
		let _ = self.guard_write_inner(child_info, key, false);
	}

	fn guard_write_inner(
		&mut self,
		child_info: Option<&ChildInfo>,
		key: &[u8],
		ret_result: bool,
	) -> bool {
		let blocked = Self::key_write_forbid(
			&self.filters_forbid_write,
			&self.filters_interval_forbid_write,
			child_info,
			key,
		);
		for origin in blocked {
			if ret_result {
				return false
			}
			self.failure_handlers.invalid_accesses(origin);
		}
		if self.allow_write_active {
			if !Self::guard_write_allow(
				&self.filters_allow_write,
				&self.filters_interval_allow_write,
				child_info,
				key,
			) {
				if ret_result {
					return false
				}
				self.failure_handlers.invalid_allowed_access();
			}
		}
		true
	}

	fn key_write_forbid<'a>(
		filters: &'a FilterTreesForbid,
		filters_interval: &'a interval_filters::FilterTreesForbidInterval,
		child_info: Option<&ChildInfo>,
		key: &'a [u8],
	) -> Vec<&'a FilterOrigin> {
		let mut blocked = Vec::new();

		if let Some(filter) = filters_interval.filter(child_info) {
			let origins = interval_filters::check_key_inclusion_origin(filter, key);
			if let Some(origins) = origins {
				if origins.is_defined() {
					blocked.push(origins);
				}
			}
		}

		let filters =
			if let Some(filter) = filters.filter(child_info) { filter } else { return blocked };
		if let Some(value) = filters.get(key) {
			if value.is_defined() {
				blocked.push(value);
			}
		}
		blocked
	}

	// TODO fuse with key_write_forbid
	fn key_read_forbid<'a>(
		filters: &'a FilterTreesForbid,
		filters_interval: &'a interval_filters::FilterTreesForbidInterval,
		child_info: Option<&ChildInfo>,
		key: &'a [u8],
	) -> Vec<&'a FilterOrigin> {
		Self::key_write_forbid(filters, filters_interval, child_info, key)
	}

	// TODO  ret_result bool to return on first match
	fn key_forbid_interval<'a>(
		filters: &'a FilterTreesForbid,
		filters_interval: &'a interval_filters::FilterTreesForbidInterval,
		child_info: Option<&ChildInfo>,
		key: &'a [u8],
		key_end: Option<&'a [u8]>,
	) -> Vec<&'a FilterOrigin> {
		let mut blocked = Vec::new();

		if let Some(filter) = filters_interval.filter(child_info) {
			let origins =
				interval_filters::check_exclusive_interval_overlap_origins(filter, key, key_end);
			for origin in origins.into_iter() {
				if origin.is_defined() {
					blocked.push(origin);
				}
			}
		}

		let filters =
			if let Some(filter) = filters.filter(child_info) { filter } else { return blocked };

		let start = key;
		let iter = if let Some(end) = key_end {
			filters.range::<Vec<u8>, _>(start.to_vec()..end.to_vec())
		} else {
			filters.range::<Vec<u8>, _>(start.to_vec()..)
		};
		for (_, value) in iter {
			if value.is_defined() {
				blocked.push(value);
			}
		}
		blocked
	}

	/// Return false if blocked.
	fn guard_read_allow(
		filters: &FilterTreesAllow,
		filters_interval: &interval_filters::FilterTreesAllowInterval,
		child_info: Option<&ChildInfo>,
		key: &[u8],
	) -> bool {
		if let Some(filter) = filters_interval.filter(child_info) {
			if interval_filters::check_key_inclusion(filter, key) {
				return true
			}
		}

		let filters =
			if let Some(filter) = filters.filter(child_info) { filter } else { return false };
		if let Some(value) = filters.get(key) {
			if *value {
				return true
			}
		}
		false
	}

	/// allow iteration is only allowing interval definition.
	fn guard_allow_interval(
		_filters: &FilterTreesAllow, /* cannot create an interval from individual value
		                              * (infinite size) */
		filters_interval: &interval_filters::FilterTreesAllowInterval,
		child_info: Option<&ChildInfo>,
		key: &[u8],
		key_end: Option<&[u8]>,
	) -> bool {
		if let Some(filter) = filters_interval.filter(child_info) {
			if interval_filters::check_interval_into(filter, key, key_end) {
				return true
			}
		}
		false
	}

	//TODO factor with guard_read_alow
	/// Return false if blocked.
	fn guard_write_allow(
		filters: &FilterTreesAllow,
		filters_interval: &interval_filters::FilterTreesAllowInterval,
		child_info: Option<&ChildInfo>,
		key: &[u8],
	) -> bool {
		if let Some(filter) = filters_interval.filter(child_info) {
			if interval_filters::check_key_inclusion(filter, key) {
				return true
			}
		}
		let filters =
			if let Some(filter) = filters.filter(child_info) { filter } else { return false };
		if let Some(value) = filters.get(key) {
			if *value {
				return true
			}
		}
		false
	}

	pub(super) fn guard_read_interval(
		&mut self,
		child_info: Option<&ChildInfo>,
		key: &[u8],
		key_end: Option<&[u8]>,
	) {
		self.guard_interval(child_info, key, key_end, true, false)
	}

	pub(super) fn guard_write_interval(
		&mut self,
		child_info: Option<&ChildInfo>,
		key: &[u8],
		key_end: Option<&[u8]>,
	) {
		self.guard_interval(child_info, key, key_end, true, false)
	}

	fn guard_interval(
		&mut self,
		child_info: Option<&ChildInfo>,
		key: &[u8],
		key_end: Option<&[u8]>,
		read: bool,
		write: bool,
	) {
		self.guard_interval_inner(child_info, key, key_end, false, read, write);
	}

	// TODO split read / write
	pub(super) fn guard_interval_inner(
		&mut self,
		child_info: Option<&ChildInfo>,
		key: &[u8],
		key_end: Option<&[u8]>,
		ret_result: bool,
		read: bool,
		write: bool,
	) -> bool {
		debug_assert!(!(read && write)); // due to interval api
		let forbid_intervals = if read {
			&self.filters_interval_forbid_read
		} else if write {
			&self.filters_interval_forbid_write
		} else {
			unreachable!(); // TODO refact api to be at least one.
		};
		if read {
			let blocked = Self::key_forbid_interval(
				&self.filters_forbid_read,
				forbid_intervals,
				child_info,
				key,
				key_end,
			);
			for origin in blocked {
				if ret_result {
					return false
				}
				self.failure_handlers.invalid_accesses(origin);
			}
		}
		if write {
			let blocked = Self::key_forbid_interval(
				&self.filters_forbid_write,
				forbid_intervals,
				child_info,
				key,
				key_end,
			);
			for origin in blocked {
				if ret_result {
					return false
				}
				self.failure_handlers.invalid_accesses(origin);
			}
		}

		if self.allow_read_active {
			if read &&
				!Self::guard_allow_interval(
					&self.filters_allow_read,
					&self.filters_interval_allow_read,
					child_info,
					key,
					key_end,
				) {
				if ret_result {
					return false
				}
				self.failure_handlers.invalid_allowed_access();
			}
			if write &&
				!Self::guard_allow_interval(
					&self.filters_allow_write,
					&self.filters_interval_allow_write,
					child_info,
					key,
					key_end,
				) {
				if ret_result {
					return false
				}
				self.failure_handlers.invalid_allowed_access();
			}
		}
		true
	}

	pub(super) fn remove_worker(&mut self, task_id: TaskId) {
		if let Some(decl) = self.changes.remove(&task_id) {
			for filters in decl.into_iter() {
				self.failure_handlers.remove(task_id);
				self.remove_forbid_all(filters.read_write, task_id);
				self.remove_forbid_writes(filters.read_only, task_id);
				self.remove_forbid_reads(filters.write_only, task_id);
			}
		}
	}

	pub(super) fn is_result_for_parent_invalid(&self) -> bool {
		self.failure_handlers.parent_failure_handler.did_fail()
	}
}

#[derive(Debug, Clone, Default)]
pub struct FilterOrigin {
	children: Option<BTreeSet<TaskId>>,
}

impl FilterOrigin {
	fn is_defined(&self) -> bool {
		self.children.is_some()
	}

	fn set(&mut self, from_child: TaskId) {
		if self.children.is_none() {
			let mut children = BTreeSet::new();
			children.insert(from_child);
			self.children = Some(children);
		} else {
			if let Some(children) = self.children.as_mut() {
				children.insert(from_child);
			}
		}
	}

	fn remove(&mut self, from_child: TaskId) {
		if self
			.children
			.as_mut()
			.map(|set| {
				set.remove(&from_child);
				set.is_empty()
			})
			.unwrap_or(false)
		{
			self.children = None;
		}
	}

	fn is_empty(&self) -> bool {
		self.children.as_ref().map(|c| c.is_empty()).unwrap_or(true)
	}

	fn is_single(&self) -> bool {
		self.children.as_ref().map(|c| c.len() == 1).unwrap_or(false)
	}
}

pub(super) mod failure {
	use sp_externalities::{DeclarationFailureHandling, TaskId};
	use sp_std::collections::btree_map::BTreeMap;

	/// Worker state for failure.
	#[derive(Debug, Clone, Default)]
	pub(super) struct DeclFailureHandling {
		/// When failure handling is not panic this is set to `true` on
		/// error.
		/// When this is true the worker result will be resolved to invalid.
		did_fail: bool,

		/// How to handle worker broken assumption (panic or invalid result).
		failure: DeclarationFailureHandling,
	}

	impl DeclFailureHandling {
		pub(super) fn did_fail(&self) -> bool {
			self.did_fail
		}

		fn invalid_access(&mut self) {
			match self.failure {
				DeclarationFailureHandling::Panic => {
					panic!("{}", super::BROKEN_DECLARATION_ACCESS);
				},
				DeclarationFailureHandling::InvalidAtJoin => {
					self.did_fail = true;
				},
			}
		}
	}

	/// Main state for failure handlers.
	/// We can have different handling for every worker.
	#[derive(Debug, Clone)]
	pub(super) struct FailureHandlers {
		// For failure as declare by parent.
		pub(super) parent_failure_handler: DeclFailureHandling,
		// For failure as declare by children.
		pub(super) children_failure_handler: BTreeMap<TaskId, DeclFailureHandling>,
	}

	impl FailureHandlers {
		// TODO rename to invalid parent access?
		pub(super) fn invalid_allowed_access(&mut self) {
			self.parent_failure_handler.invalid_access();
		}

		// TODO rename to invalid child access?
		pub(super) fn invalid_accesses(&mut self, origin: &super::FilterOrigin) {
			if let Some(children) = origin.children.as_ref() {
				for child in children.iter() {
					self.invalid_access(*child);
				}
			}
		}

		fn invalid_access(&mut self, from_child: TaskId) {
			self.children_failure_handler
				.get_mut(&from_child)
				.expect("TODO")
				.invalid_access()
		}

		pub(super) fn set_failure_handler(
			&mut self,
			from_child: Option<TaskId>,
			failure: DeclarationFailureHandling,
		) {
			let handling = DeclFailureHandling { did_fail: Default::default(), failure };
			if let Some(id) = from_child {
				self.children_failure_handler.insert(id, handling);
			} else {
				self.parent_failure_handler = handling;
			}
		}

		pub(super) fn remove(&mut self, child: TaskId) {
			self.children_failure_handler.remove(&child);
		}

		// TODO this does not make much sense, should be specific to a child marker
		pub(super) fn did_fail(&self) -> bool {
			if self.parent_failure_handler.did_fail {
				return true
			}
			for (_id, handler) in self.children_failure_handler.iter() {
				if handler.did_fail {
					return true
				}
			}
			false
		}
	}
}

pub(super) mod interval_filters {
	use super::*;
	/// TODO bad def, need a quick check algo.
	#[derive(Debug, Clone, Default)]
	pub struct FilterInterval<F: Debug + Clone + Default> {
		// if F is default it is end of.
		// if Not it is start for F and may be end for others, but that is irrelevent
		// as we only ever check previous declaration not empty to know if we are in
		// an interval.
		start_of: F,
		rc_end: usize, // TODO can be part of F so save memory of allow
	}

	impl FilterInterval<FilterOrigin> {
		fn is_empty(&self) -> bool {
			self.start_of.is_empty()
		}
		fn can_remove(&self) -> bool {
			self.is_empty() && self.rc_end == 0
		}
	}

	#[derive(Debug, Clone, Default)]
	pub(super) struct FilterTrees<F: Debug + Clone + Default> {
		pub(super) top: FilterTreeInterval<F>,
		pub(super) children: BTreeMap<crate::overlayed_changes::StorageKey, FilterTreeInterval<F>>,
	}

	impl<F: Debug + Clone + Default> FilterTrees<F> {
		pub(super) fn filter(
			&self,
			child_info: Option<&ChildInfo>,
		) -> Option<&FilterTreeInterval<F>> {
			if let Some(child_info) = child_info {
				self.children.get(child_info.storage_key())
			} else {
				Some(&self.top)
			}
		}
	}

	pub(super) type FilterTreesAllowInterval = FilterTrees<bool>;
	pub(super) type FilterTreeInterval<F> = BTreeMap<Vec<u8>, FilterInterval<F>>;
	pub(super) type FilterTreeAllowInterval = FilterTreeInterval<bool>;

	pub(super) type FilterTreesForbidInterval = FilterTrees<FilterOrigin>;
	pub(super) type FilterTreeForbidInterval = FilterTreeInterval<FilterOrigin>;

	pub(super) trait FilterBound: Default + Debug + Clone {
		fn is_empty(&self) -> bool;
		fn is_single(&self) -> bool;
	}

	impl FilterBound for bool {
		fn is_empty(&self) -> bool {
			!self
		}
		fn is_single(&self) -> bool {
			*self
		}
	}

	impl FilterBound for FilterOrigin {
		fn is_empty(&self) -> bool {
			self.is_empty()
		}
		fn is_single(&self) -> bool {
			self.is_single()
		}
	}

	pub(super) fn set_filter_allow(
		filter: &mut FilterTreesAllowInterval,
		decl: &AccessDeclaration,
	) -> bool {
		if !set_filter_allow_internal(&mut filter.top, &decl.top) {
			return false
		}
		for (storage_key, decl) in decl.children.iter() {
			let child = filter.children.entry(storage_key.clone()).or_default();
			if !set_filter_allow_internal(child, decl) {
				return false
			}
		}
		true
	}

	// TODO should merge intervals : remove all start in interval, including end. Also remove all
	// end except end.
	fn set_filter_allow_internal(
		filter: &mut FilterTreeAllowInterval,
		decl: &LocalAccessDeclaration,
	) -> bool {
		// TODO avoid a clone.
		for (start, end) in
			decl.intervals_lock.iter().map(|(start, end)| (start, end.clone())).chain(
				decl.prefixes_lock.iter().map(|prefix| {
					let end = end_prefix(prefix.as_slice());
					(prefix, end)
				}),
			) {
			let mut do_insert_start = true;
			let mut do_insert_end = true;
			let mut loop_insert_start = true;
			let mut end_has_parent_origin = false;
			let mut iter = filter.range_mut::<Vec<u8>, _>(start..);
			// set origin to all end into interval
			for (k, value) in &mut iter {
				if loop_insert_start && do_insert_start {
					loop_insert_start = false;
					if k == start {
						do_insert_start = false;
						value.start_of = true;
						continue
					}
				}
				if end.as_ref().map(|end| k < end).unwrap_or(true) {
					value.start_of = true;
				} else if Some(k) == end.as_ref() {
					do_insert_end = false;
					break
				} else {
					break
				}
			}
			if do_insert_start {
				filter.insert(start.clone(), FilterInterval { start_of: true, rc_end: 0 });
			}
			if do_insert_end {
				if let Some(end) = end.clone() {
					filter.insert(end, FilterInterval { start_of: false, rc_end: 0 });
				}
			}
		}
		true
	}

	pub(super) fn set_filter_forbid(
		filter: &mut FilterTrees<FilterOrigin>,
		decl: &AccessDeclaration,
		from_child: TaskId,
	) -> bool {
		if !set_filter_forbid_internal(&mut filter.top, &decl.top, from_child) {
			return false
		}
		for (storage_key, decl) in decl.children.iter() {
			let child = filter.children.entry(storage_key.clone()).or_default();
			if !set_filter_forbid_internal(child, decl, from_child) {
				return false
			}
		}
		true
	}

	// TODO always return true
	fn set_filter_forbid_internal(
		filter: &mut FilterTreeForbidInterval,
		decl: &LocalAccessDeclaration,
		from_child: TaskId,
	) -> bool {
		// TODO avoid a clone.
		for (start, end) in
			decl.intervals_lock.iter().map(|(start, end)| (start, end.clone())).chain(
				decl.prefixes_lock.iter().map(|prefix| {
					let end = end_prefix(prefix.as_slice());
					(prefix, end)
				}),
			) {
			let mut do_insert_start = true;
			let mut do_insert_end = true;
			let mut loop_insert_start = true;
			let mut end_has_parent_origin = false;
			let mut iter = filter.range_mut::<Vec<u8>, _>(start..);
			// set origin to all end into interval
			for (k, value) in &mut iter {
				if loop_insert_start && do_insert_start {
					loop_insert_start = false;
					if k == start {
						do_insert_start = false;
						value.start_of.set(from_child);
						continue
					}
				}
				if end.as_ref().map(|end| k < end).unwrap_or(true) {
					value.start_of.set(from_child);
				} else if Some(k) == end.as_ref() {
					do_insert_end = false;
					value.rc_end += 1;
					break
				} else {
					break
				}
			}
			if do_insert_start {
				let mut start_origin = FilterOrigin::default();
				start_origin.set(from_child);
				filter.insert(start.clone(), FilterInterval { start_of: start_origin, rc_end: 0 });
			}
			if do_insert_end {
				if let Some(end) = end.clone() {
					filter.insert(end, FilterInterval { start_of: Default::default(), rc_end: 1 });
				}
			}
		}
		true
	}

	pub(super) fn remove_filter_forbid(
		filter: &mut FilterTrees<FilterOrigin>,
		decl: &AccessDeclaration,
		from_child: TaskId,
	) {
		remove_filter_forbid_internal(&mut filter.top, &decl.top, from_child);
		for (storage_key, decl) in decl.children.iter() {
			if let Some(child) = filter.children.get_mut(storage_key) {
				remove_filter_forbid_internal(child, decl, from_child);
			}
		}
	}

	fn remove_filter_forbid_internal(
		filter: &mut FilterTreeForbidInterval,
		decl: &LocalAccessDeclaration,
		from_child: TaskId,
	) {
		let mut to_remove = Vec::new();

		// TODO avoid a clone.
		for (start, end) in
			decl.intervals_lock.iter().map(|(start, end)| (start, end.clone())).chain(
				decl.prefixes_lock.iter().map(|prefix| {
					let end = end_prefix(prefix.as_slice());
					(prefix, end)
				}),
			) {
			let mut iter = filter.range_mut::<Vec<u8>, _>(start..);
			let mut touched = false;
			let mut loop_remove_start = true;
			for (k, value) in &mut iter {
				if loop_remove_start {
					if k == start {
						value.start_of.remove(from_child);
						if value.can_remove() {
							to_remove.push(k.to_vec());
						}
						continue
					}
				}
				if end.as_ref().map(|end| k < end).unwrap_or(true) {
					value.start_of.remove(from_child);
					if value.can_remove() {
						to_remove.push(k.to_vec());
					}
				} else if Some(k) == end.as_ref() {
					value.rc_end -= 1;
					if value.can_remove() {
						to_remove.push(k.to_vec());
					}
				} else {
					break
				}
			}
		}
		for key in to_remove {
			filter.remove(key.as_slice());
		}
	}

	pub(super) fn check_key_inclusion_origin<'a, D: FilterBound>(
		filter: &'a FilterTreeInterval<D>,
		key: &[u8],
	) -> Option<&'a D> {
		let mut iter = filter.range::<Vec<u8>, _>(..=key.to_vec()).rev();
		if let Some((k, value)) = iter.next() {
			Some(&value.start_of)
		} else {
			None
		}
	}

	pub(super) fn check_key_inclusion<D: FilterBound>(
		filter: &FilterTreeInterval<D>,
		key: &[u8],
	) -> bool {
		let mut iter = filter.range::<Vec<u8>, _>(..=key.to_vec()).rev();
		if let Some((k, value)) = iter.next() {
			!value.start_of.is_empty()
		} else {
			false
		}
	}

	pub(super) fn check_interval_into<D: FilterBound>(
		// TODO use prefix only?
		filter: &FilterTreeInterval<D>,
		from: &[u8],
		to: Option<&[u8]>,
	) -> bool {
		if !check_key_inclusion(filter, from) {
			return false
		}
		let mut iter = if let Some(to) = to {
			filter.range::<Vec<u8>, _>(from.to_vec()..to.to_vec())
		} else {
			filter.range::<Vec<u8>, _>(from.to_vec()..)
		};
		for (k, value) in iter {
			if value.start_of.is_empty() {
				return false
			}
		}
		true
	}

	// TODO this only work if we do not allow both interval and prefix definition
	// (prefix could complet interval).
	pub(super) fn check_exclusive_interval_overlap<D: FilterBound>(
		// TODO use prefix only?
		filter: &FilterTreeInterval<D>,
		from: &[u8],
		to: Option<&[u8]>,
	) -> bool {
		if check_key_inclusion(filter, from) {
			return true
		}
		let mut iter = if let Some(to) = to {
			filter.range::<Vec<u8>, _>(from.to_vec()..to.to_vec())
		} else {
			filter.range::<Vec<u8>, _>(from.to_vec()..)
		};
		for (k, value) in iter {
			if !value.start_of.is_empty() {
				return true
			}
		}
		false
	}

	pub(super) fn check_prefix_overlap<D: FilterBound>(
		filter: &FilterTreeInterval<D>,
		prefix: &[u8],
	) -> bool {
		let end = end_prefix(prefix);
		check_exclusive_interval_overlap::<D>(filter, prefix, end.as_ref().map(Vec::as_slice))
	}

	pub(super) fn check_exclusive_interval_overlap_origins<'a, D: FilterBound>(
		// TODO use prefix only?
		filter: &'a FilterTreeInterval<D>,
		from: &[u8],
		to: Option<&[u8]>,
	) -> Vec<&'a D> {
		let mut result = Vec::new();
		if let Some(d) = check_key_inclusion_origin(filter, from) {
			result.push(d);
		}
		let mut iter = if let Some(to) = to {
			filter.range::<Vec<u8>, _>(from.to_vec()..to.to_vec())
		} else {
			filter.range::<Vec<u8>, _>(from.to_vec()..)
		};
		for (k, value) in iter {
			if !value.start_of.is_empty() {
				result.push(&value.start_of)
			}
		}
		result
	}

	pub(super) fn check_prefix_overlap_origins<'a, D: FilterBound>(
		filter: &'a FilterTreeInterval<D>,
		// TODO this 'a is needed due to radix iter needing 'a, but if iter would manage two
		// lifetime one for the iterator and one for the actual iterated value.
		// Then we would not need it this is a bit everywhere.
		prefix: &[u8],
	) -> Vec<&'a D> {
		let end = end_prefix(prefix);
		check_exclusive_interval_overlap_origins::<D>(
			filter,
			prefix,
			end.as_ref().map(Vec::as_slice),
		)
	}

	#[cfg(test)]
	mod test {
		use super::*;
		// TODO fuzzing against list of defined interval could be good.
		#[test]
		fn test_1() {
			let mut filter = FilterTreeInterval::<FilterOrigin>::default();
			assert!(!check_prefix_overlap(&filter, &[]));
			assert!(!check_prefix_overlap(&filter, b"test"));
			assert!(!check_key_inclusion(&filter, b"test"));
			set_filter_forbid_internal(
				&mut filter,
				&LocalAccessDeclaration {
					prefixes_lock: Default::default(),
					keys_lock: Default::default(),
					intervals_lock: vec![(b"ke".to_vec(), Some(b"l".to_vec()))],
					all: false,
				},
				1,
			);
			assert!(check_prefix_overlap(&filter, &[]));
			assert!(check_prefix_overlap(&filter, b"ke"));
			assert!(!check_prefix_overlap(&filter, b"a"));
			assert!(check_exclusive_interval_overlap(&filter, b"ke", None));
			assert!(check_exclusive_interval_overlap(&filter, b"ken", Some(b"kez")));
			assert!(!check_exclusive_interval_overlap(&filter, b"k", Some(b"ke")));
			assert!(!check_exclusive_interval_overlap(&filter, b"l", None));
			assert!(!check_key_inclusion(&filter, b"a"));
			assert!(check_key_inclusion_origin(&filter, b"a")
				.map(|o| o.is_empty())
				.unwrap_or(true));
			assert!(!check_key_inclusion(&filter, b"test"));
			assert!(check_key_inclusion(&filter, b"kenny"));
			assert!(check_key_inclusion(&filter, b"ke"));
			assert!(!check_key_inclusion_origin(&filter, b"ke")
				.map(|o| o.is_empty())
				.unwrap_or(true));
			set_filter_forbid_internal(
				&mut filter,
				&LocalAccessDeclaration {
					prefixes_lock: Default::default(),
					keys_lock: Default::default(),
					intervals_lock: vec![(b"l".to_vec(), Some(b"op".to_vec()))],
					all: false,
				},
				2,
			);
			set_filter_forbid_internal(
				&mut filter,
				&LocalAccessDeclaration {
					prefixes_lock: Default::default(),
					keys_lock: Default::default(),
					intervals_lock: vec![(b"opa".to_vec(), Some(b"z".to_vec()))],
					all: false,
				},
				3,
			);
			let decl_4 = LocalAccessDeclaration {
				prefixes_lock: Default::default(),
				keys_lock: Default::default(),
				intervals_lock: vec![(b"o".to_vec(), Some(b"z".to_vec()))],
				all: false,
			};
			set_filter_forbid_internal(&mut filter, &decl_4, 4);
			remove_filter_forbid_internal(&mut filter, &decl_4, 4);
		}
	}
}

#[cfg(test)]
mod test {
	use super::*;

	#[test]
	fn test_guard_read_allow_interval() {
		let mut filters1 = Filters::default();
		filters1.allow_reads(AccessDeclaration {
			top: LocalAccessDeclaration {
				prefixes_lock: vec![b"key".to_vec()],
				keys_lock: vec![],
				intervals_lock: vec![],
				all: false,
			},
			default_children: Default::default(),
			children: Default::default(),
		});
		assert!(Filters::guard_allow_interval(
			&filters1.filters_allow_read,
			&filters1.filters_interval_allow_read,
			None,
			&b"key"[..],
			Some(&b"key8"[..]),
		));
		assert!(Filters::guard_allow_interval(
			&filters1.filters_allow_read,
			&filters1.filters_interval_allow_read,
			None,
			&b"key"[..],
			Some(&b"kez"[..]),
		));
		assert!(!Filters::guard_allow_interval(
			&filters1.filters_allow_read,
			&filters1.filters_interval_allow_read,
			None,
			&b"key"[..],
			Some(&b"keza"[..]),
		));
		assert!(!Filters::guard_allow_interval(
			&filters1.filters_allow_read,
			&filters1.filters_interval_allow_read,
			None,
			&b"key"[..],
			None,
		));
		//let mut filters1 = Filters::default();
		filters1.allow_reads(AccessDeclaration {
			top: LocalAccessDeclaration {
				prefixes_lock: vec![b"".to_vec()],
				keys_lock: vec![],
				intervals_lock: vec![],
				all: false,
			},
			default_children: Default::default(),
			children: Default::default(),
		});
		assert!(Filters::guard_allow_interval(
			&filters1.filters_allow_read,
			&filters1.filters_interval_allow_read,
			None,
			&b"a"[..],
			Some(&b"kez"[..]),
		));
		assert!(Filters::guard_allow_interval(
			&filters1.filters_allow_read,
			&filters1.filters_interval_allow_read,
			None,
			&b""[..],
			None,
		));
		assert!(Filters::guard_allow_interval(
			&filters1.filters_allow_read,
			&filters1.filters_interval_allow_read,
			None,
			&b"key"[..],
			None,
		));

		let mut filters1 = Filters::default();
		filters1.allow_reads(AccessDeclaration {
			top: LocalAccessDeclaration {
				prefixes_lock: vec![b"key".to_vec(), b"kez".to_vec()],
				keys_lock: vec![],
				intervals_lock: vec![],
				all: false,
			},
			default_children: Default::default(),
			children: Default::default(),
		});
		assert!(Filters::guard_allow_interval(
			&filters1.filters_allow_read,
			&filters1.filters_interval_allow_read,
			None,
			&b"key"[..],
			Some(&b"kez"[..]),
		));

		let mut filters1 = Filters::default();
		filters1.allow_reads(AccessDeclaration {
			top: LocalAccessDeclaration {
				prefixes_lock: vec![b"kex".to_vec(), b"kez".to_vec()],
				keys_lock: vec![],
				intervals_lock: vec![],
				all: false,
			},
			default_children: Default::default(),
			children: Default::default(),
		});
		assert!(!Filters::guard_allow_interval(
			&filters1.filters_allow_read,
			&filters1.filters_interval_allow_read,
			None,
			&b"kex"[..],
			Some(&b"kez"[..]),
		));
	}
}
