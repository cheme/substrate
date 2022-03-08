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

// Allow for child worker.
type FilterTreesAllow = super::radix_trees::FilterTrees<Filter<bool>>;
type FilterTreeAllow = super::radix_trees::FilterTree<Filter<bool>>;

// Forbid for parent worker. Forbid always override allow.
type FilterTreesForbid = super::radix_trees::FilterTrees<Filter<FilterOrigin>>;
type FilterTreeForbid = super::radix_trees::FilterTree<Filter<FilterOrigin>>;

// TODO same consuming function?
fn prefixed_storage_key_to_child_info(prefixed_storage_key: &Vec<u8>) -> ChildInfo {
	let prefixed_key = PrefixedStorageKey::new_ref(prefixed_storage_key);
	match ChildType::from_prefixed_key(prefixed_key) {
		Some((ChildType::ParentKeyId, storage_key)) => ChildInfo::new_default(storage_key),
		None => panic!("Unsupported child key"), // actually should be unreachable.
	}
}

/// Main filter state.
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
	filters_allow: FilterTreesAllow,
	/// accesses forbid due to child.
	filters_forbid: FilterTreesForbid,
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
			filters_allow: Default::default(),
			filters_forbid: Default::default(),
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

	fn remove_filter(
		tree: &mut FilterTreesForbid,
		filter: AccessDeclaration,
		from_child: TaskId,
		read: bool,
		write: bool,
	) {
		Self::remove_filter_internal(&mut tree.top, filter.top, from_child, read, write);
		for (prefixed_storage_key, filter_child) in filter.children {
			if let Some(child) = tree.children.get_mut(&prefixed_storage_key) {
				Self::remove_filter_internal(child, filter_child, from_child, read, write);
			}
		}
	}

	fn remove_filter_internal(
		tree: &mut FilterTreeForbid,
		filter: LocalAccessDeclaration,
		from_child: TaskId,
		read: bool,
		write: bool,
	) {
		for prefix in filter.prefixes_lock {
			if let Some(filter) = tree.get_mut(prefix.as_slice()) {
				if read {
					filter.read_prefix.remove(from_child);
				}
				if write {
					filter.write_prefix.remove(from_child);
				}
				if filter.is_empty() {
					tree.remove(prefix.as_slice(), false);
				}
			}
		}
		for key in filter.keys_lock {
			if let Some(filter) = tree.get_mut(key.as_slice()) {
				if read {
					filter.read_key.remove(from_child);
				}
				if write {
					filter.write_key.remove(from_child);
				}
				if filter.is_empty() {
					tree.remove(key.as_slice(), false);
				}
			}
		}
	}

	fn set_filter_forbid(
		tree: &mut FilterTreesForbid,
		filter: AccessDeclaration,
		from_child: TaskId,
		read: bool,
		write: bool,
	) -> bool {
		if !Self::set_filter_forbid_internal(&mut tree.top, filter.top, from_child, read, write) {
			return false
		}
		for (prefixed_storage_key, filter_child) in filter.children {
			let mut child = tree.children.entry(prefixed_storage_key).or_default();
			if !Self::set_filter_forbid_internal(&mut child, filter_child, from_child, read, write)
			{
				return false
			}
		}
		true
	}

	fn set_filter_forbid_internal(
		tree: &mut FilterTreeForbid,
		filter: LocalAccessDeclaration,
		from_child: TaskId,
		read: bool,
		write: bool,
	) -> bool {
		let set = |filter: &mut Filter<FilterOrigin>| {
			if read {
				filter.read_prefix.set(from_child);
			}
			if write {
				filter.write_prefix.set(from_child);
			}
		};
		for prefix in filter.prefixes_lock {
			// TODO return false if unordered
			if let Some(filter) = tree.get_mut(prefix.as_slice()) {
				set(filter);
			} else {
				// new entry
				let mut filter = Filter::<FilterOrigin>::default();
				set(&mut filter);
				tree.insert(prefix.as_slice(), filter);
			}
		}
		let set = |filter: &mut Filter<FilterOrigin>| {
			if read {
				filter.read_key.set(from_child);
			}
			if write {
				filter.write_key.set(from_child);
			}
		};
		for key in filter.keys_lock {
			// TODO return false if unordered
			if let Some(filter) = tree.get_mut(key.as_slice()) {
				set(filter);
			} else {
				// new entry
				let mut filter = Filter::<FilterOrigin>::default();
				set(&mut filter);
				tree.insert(key.as_slice(), filter);
			}
		}
		true
	}

	fn set_filter_allow(
		tree: &mut FilterTreesAllow,
		filter: AccessDeclaration,
		read: bool,
		write: bool,
	) -> bool {
		if !Self::set_filter_allow_internal(&mut tree.top, filter.top, read, write) {
			return false
		}
		for (prefixed_storage_key, filter_child) in filter.children {
			let mut child = tree.children.entry(prefixed_storage_key).or_default();
			if !Self::set_filter_allow_internal(&mut child, filter_child, read, write) {
				return false
			}
		}
		true
	}

	fn set_filter_allow_internal(
		tree: &mut FilterTreeAllow,
		filter: LocalAccessDeclaration,
		read: bool,
		write: bool,
	) -> bool {
		let set = |filter: &mut Filter<bool>| {
			if read {
				filter.read_prefix = read;
			}
			if write {
				filter.write_prefix = write;
			}
		};
		for prefix in filter.prefixes_lock {
			// TODO return false if unordered
			if let Some(filter) = tree.get_mut(prefix.as_slice()) {
				set(filter);
			} else {
				// new entry
				let mut filter = Filter::<bool>::default();
				set(&mut filter);
				tree.insert(prefix.as_slice(), filter);
			}
		}
		let set = |filter: &mut Filter<bool>| {
			if read {
				filter.read_key = read;
			}
			if write {
				filter.write_key = write;
			}
		};
		for key in filter.keys_lock {
			// TODO return false if unordered
			if let Some(filter) = tree.get_mut(key.as_slice()) {
				set(filter);
			} else {
				// new entry
				let mut filter = Filter::<bool>::default();
				set(&mut filter);
				tree.insert(key.as_slice(), filter);
			}
		}
		true
	}

	pub(super) fn allow_all(&mut self, filter: AccessDeclaration) {
		self.allow_write_active = true;
		interval_filters::set_filter_allow(&mut self.filters_interval_allow_read, &filter); // TODO return false if wrong declaration same for others allow
		interval_filters::set_filter_allow(&mut self.filters_interval_allow_write, &filter);
		Self::set_filter_allow(&mut self.filters_allow, filter, true, true);
	}

	pub(super) fn allow_writes(&mut self, filter: AccessDeclaration) {
		self.allow_write_active = true;
		interval_filters::set_filter_allow(&mut self.filters_interval_allow_write, &filter);
		Self::set_filter_allow(&mut self.filters_allow, filter, false, true);
	}

	pub(super) fn allow_reads(&mut self, filter: AccessDeclaration) {
		self.allow_read_active = true;
		interval_filters::set_filter_allow(&mut self.filters_interval_allow_read, &filter);
		Self::set_filter_allow(&mut self.filters_allow, filter, true, false);
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
		Self::set_filter_forbid(&mut self.filters_forbid, filter, from_child, true, true);
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
		Self::remove_filter(&mut self.filters_forbid, filter, from_child, true, true);
	}

	pub(super) fn forbid_reads(&mut self, filter: AccessDeclaration, from_child: TaskId) {
		interval_filters::set_filter_forbid(
			&mut self.filters_interval_forbid_read,
			&filter,
			from_child,
		);
		Self::set_filter_forbid(&mut self.filters_forbid, filter, from_child, true, false);
	}

	fn remove_forbid_reads(&mut self, filter: AccessDeclaration, from_child: TaskId) {
		interval_filters::remove_filter_forbid(
			&mut self.filters_interval_forbid_read,
			&filter,
			from_child,
		);
		Self::remove_filter(&mut self.filters_forbid, filter, from_child, true, false);
	}

	pub(super) fn forbid_writes(&mut self, filter: AccessDeclaration, from_child: TaskId) {
		interval_filters::set_filter_forbid(
			&mut self.filters_interval_forbid_write,
			&filter,
			from_child,
		);
		Self::set_filter_forbid(&mut self.filters_forbid, filter, from_child, false, true);
	}

	pub(super) fn remove_forbid_writes(&mut self, filter: AccessDeclaration, from_child: TaskId) {
		interval_filters::remove_filter_forbid(
			&mut self.filters_interval_forbid_write,
			&filter,
			from_child,
		);
		Self::remove_filter(&mut self.filters_forbid, filter, from_child, false, true);
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
		// This assumes small declaration (otherwhise first loading in a tree could save
		// a few tree node access.
		for prefix in filter.prefixes_lock.iter() {
			if !self.guard_read_prefix_inner(child_info, prefix, true) {
				return false
			}
		}
		for key in filter.keys_lock.iter() {
			if !self.guard_read_inner(child_info, key, true) {
				return false
			}
		}
		for interval in filter.intervals_lock.iter() {
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
		for prefix in filter.prefixes_lock.iter() {
			if !self.guard_write_prefix_inner(child_info, prefix, true) {
				return false
			}
		}
		for key in filter.keys_lock.iter() {
			if !self.guard_write_inner(child_info, key, true) {
				return false
			}
		}
		for interval in filter.intervals_lock.iter() {
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

	pub(super) fn guard_read_all(&mut self) {
		self.guard_read_prefix(None, &[]);
		// costy alloc for mutable reference question, could be optimize.
		let children: Vec<_> = self
			.filters_forbid
			.children
			.iter()
			.map(|(storage_key, _child)| prefixed_storage_key_to_child_info(storage_key))
			.collect();

		for child_info in children.into_iter() {
			self.guard_read_prefix(Some(&child_info), &[]);
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
			&self.filters_forbid,
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
				&self.filters_allow,
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
			&self.filters_forbid,
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
				&self.filters_allow,
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
			if origins.is_defined() {
				blocked.push(origins);
			}
		}

		let filters =
			if let Some(filter) = filters.filter(child_info) { filter } else { return blocked };
		let len = key.len();
		for (key, value) in filters.seek_iter(key).value_iter() {
			if value.write_prefix.is_defined() {
				blocked.push(&value.write_prefix);
			}
			if len == key.len() {
				if value.write_key.is_defined() {
					blocked.push(&value.write_key);
				}
			}
		}
		blocked
	}

	// TODO factor with key_write_forbid
	fn key_read_forbid<'a>(
		filters: &'a FilterTreesForbid,
		filters_interval: &'a interval_filters::FilterTreesForbidInterval,
		child_info: Option<&ChildInfo>,
		key: &'a [u8],
	) -> Vec<&'a FilterOrigin> {
		let mut blocked = Vec::new();

		if let Some(filter) = filters_interval.filter(child_info) {
			let origins = interval_filters::check_prefix_overlap_origins(filter, key);
			if origins.is_defined() {
				blocked.push(origins);
			}
		}

		let filters =
			if let Some(filter) = filters.filter(child_info) { filter } else { return blocked };
		let len = key.len();
		for (key, value) in filters.seek_iter(key).value_iter() {
			if value.read_prefix.is_defined() {
				blocked.push(&value.read_prefix);
			}
			if len == key.len() {
				if value.read_key.is_defined() {
					blocked.push(&value.read_key);
				}
			}
		}
		if let Some(filter) = filters_interval.filter(child_info) {
			let origins = interval_filters::check_key_inclusion_origin(filter, key);
			if origins.is_defined() {
				blocked.push(origins);
			}
		}
		blocked
	}

	// TODO can factor most of method with key only variant (need to return iterator).
	// TODO for those forbid allow a variant that only return bool is
	// fastest if panic handler, not sure it is woth it, pretty sure it is not actually.
	// (getting all origin is costy and only for failure handler)
	// But first factorize code.
	fn key_write_forbid_prefix<'a>(
		filters: &'a FilterTreesForbid,
		filters_interval: &'a interval_filters::FilterTreesForbidInterval,
		child_info: Option<&ChildInfo>,
		key: &'a [u8],
	) -> Vec<&'a FilterOrigin> {
		let mut blocked = Vec::new();

		if let Some(filter) = filters_interval.filter(child_info) {
			let origins = interval_filters::check_prefix_overlap_origins(filter, key);
			if origins.is_defined() {
				blocked.push(origins);
			}
		}

		let filters =
			if let Some(filter) = filters.filter(child_info) { filter } else { return blocked };
		let len = key.len();
		let mut iter = filters.seek_iter(key).value_iter();
		while let Some((key, value)) = iter.next() {
			if value.write_prefix.is_defined() {
				blocked.push(&value.write_prefix);
			}
			if len == key.len() {
				if value.write_key.is_defined() {
					blocked.push(&value.write_key);
				}
			}
		}
		for (_key, value) in iter.node_iter().iter_prefix().value_iter() {
			if value.write_prefix.is_defined() {
				blocked.push(&value.write_prefix);
			}
			if value.write_key.is_defined() {
				blocked.push(&value.write_key);
			}
		}
		blocked
	}

	fn key_read_forbid_prefix<'a>(
		filters: &'a FilterTreesForbid,
		filters_interval: &'a interval_filters::FilterTreesForbidInterval,
		child_info: Option<&ChildInfo>,
		key: &'a [u8],
	) -> Vec<&'a FilterOrigin> {
		let mut blocked = Vec::new();

		if let Some(filter) = filters_interval.filter(child_info) {
			let origins = interval_filters::check_prefix_overlap_origins(filter, key);
			if origins.is_defined() {
				blocked.push(origins);
			}
		}

		let filters =
			if let Some(filter) = filters.filter(child_info) { filter } else { return blocked };
		let len = key.len();
		let mut iter = filters.seek_iter(key).value_iter();
		while let Some((key, value)) = iter.next() {
			if value.read_prefix.is_defined() {
				blocked.push(&value.read_prefix);
			}
			if len == key.len() {
				if value.read_key.is_defined() {
					blocked.push(&value.read_key);
				}
			}
		}
		for (_key, value) in iter.node_iter().iter_prefix().value_iter() {
			if value.read_prefix.is_defined() {
				blocked.push(&value.read_prefix);
			}
			if value.read_key.is_defined() {
				blocked.push(&value.read_key);
			}
		}
		blocked
	}

	// TODO  ret_result bool to return on first match
	fn key_forbid_interval<'a>(
		filters: &'a FilterTreesForbid,
		filters_interval: &'a interval_filters::FilterTreesForbidInterval,
		child_info: Option<&ChildInfo>,
		key: &'a [u8],
		key_end: Option<&'a [u8]>,
		read: bool,
		write: bool,
	) -> Vec<&'a FilterOrigin> {
		let mut blocked = Vec::new();

		if let Some(filter) = filters_interval.filter(child_info) {
			let origins =
				interval_filters::check_interval_overlap_origins(filter, key, key_end, false);
			if origins.is_defined() {
				blocked.push(origins);
			}
		}

		let filters =
			if let Some(filter) = filters.filter(child_info) { filter } else { return blocked };
		let len = key.len();
		let mut iter = filters.seek_iter(key).value_iter();
		while let Some((key, value)) = iter.next() {
			if read && value.read_prefix.is_defined() {
				blocked.push(&value.read_prefix);
			}
			if write && value.write_prefix.is_defined() {
				blocked.push(&value.write_prefix);
			}
			if len == key.len() {
				if read && value.read_key.is_defined() {
					blocked.push(&value.read_key);
				}
				if write && value.write_key.is_defined() {
					blocked.push(&value.write_key);
				}
			}
		}
		for (key, value) in iter.node_iter().iter().value_iter() {
			if key_end.map(|end| key.as_slice() <= end).unwrap_or(true) {
				if read && value.read_prefix.is_defined() {
					blocked.push(&value.read_prefix);
				}
				if write && value.write_prefix.is_defined() {
					blocked.push(&value.write_prefix);
				}
				if read && value.read_key.is_defined() {
					blocked.push(&value.read_key);
				}
				if write && value.write_key.is_defined() {
					blocked.push(&value.write_key);
				}
			} else {
				break
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
		let len = key.len();
		for (key, value) in filters.seek_iter(key).value_iter() {
			// forbid write implies forbid read.
			if value.read_prefix {
				return true
			}
			if len == key.len() {
				if value.read_key {
					return true
				}
			}
		}
		false
	}

	/// allow iteration is only allowing interval definition.
	fn guard_allow_interval(
		filters: &FilterTreesAllow,
		filters_interval: &interval_filters::FilterTreesAllowInterval,
		child_info: Option<&ChildInfo>,
		key: &[u8],
		key_end: Option<&[u8]>,
		read: bool,
		write: bool,
	) -> bool {
		debug_assert!(!(read && write)); // TODO enum instead of both bool.

		if let Some(filter) = filters_interval.filter(child_info) {
			if interval_filters::check_interval_into(filter, key, key_end, false) {
				return true
			}
		}

		let filters =
			if let Some(filter) = filters.filter(child_info) { filter } else { return false };
		let mut iter = filters.seek_iter(key).value_iter();
		let mut start_interval = None;
		while let Some((key, value)) = iter.next() {
			if (read && value.read_prefix) || (write && value.write_prefix) {
				start_interval = Some(key);
				break
			}
		}
		let mut previous_prefix = if let Some(key) = start_interval {
			if key_end.map(|end| end.starts_with(key)).unwrap_or(key.is_empty()) {
				return true
			}
			key.to_vec()
		} else {
			return false
		};
		for (key, value) in iter.node_iter().iter().value_iter() {
			if (read && value.read_prefix) || (write && value.write_prefix) {
				if !key.starts_with(previous_prefix.as_slice()) {
					// only allow contigous prefix
					while previous_prefix.last() == Some(&255u8) {
						previous_prefix.pop();
					}
					previous_prefix.last_mut().map(|b| *b += 1);
					if previous_prefix != key {
						return false
					}
				}
				if key_end.map(|end| end.starts_with(&key)).unwrap_or(false) {
					return true
				}
			}
			// TODO can it be removed?
			if key_end.map(|end| key.as_slice() > end).unwrap_or(true) {
				break
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
		let len = key.len();
		for (key, value) in filters.seek_iter(key).value_iter() {
			// forbid write implies forbid read.
			if value.write_prefix {
				return true
			}
			if len == key.len() {
				if value.write_key {
					return true
				}
			}
		}
		false
	}

	fn guard_read_allow_prefix(
		filters: &FilterTreesAllow,
		filters_interval: &interval_filters::FilterTreesAllowInterval,
		child_info: Option<&ChildInfo>,
		key: &[u8],
	) -> bool {
		if let Some(filter) = filters_interval.filter(child_info) {
			if interval_filters::check_prefix_into(filter, key) {
				return true
			}
		}

		let filters =
			if let Some(filter) = filters.filter(child_info) { filter } else { return false };
		// parent or self allowed for prefix.
		for (_key, value) in filters.seek_iter(key).value_iter() {
			if value.read_prefix {
				return true
			}
		}
		false
	}

	fn guard_write_allow_prefix(
		filters: &FilterTreesAllow,
		filters_interval: &interval_filters::FilterTreesAllowInterval,
		child_info: Option<&ChildInfo>,
		key: &[u8],
	) -> bool {
		if let Some(filter) = filters_interval.filter(child_info) {
			if interval_filters::check_prefix_into(filter, key) {
				return true
			}
		}

		let filters =
			if let Some(filter) = filters.filter(child_info) { filter } else { return false };
		for (_key, value) in filters.seek_iter(key).value_iter() {
			if value.write_prefix {
				return true
			}
		}
		false
	}

	fn guard_read_prefix(&mut self, child_info: Option<&ChildInfo>, key: &[u8]) {
		let _ = self.guard_read_prefix_inner(child_info, key, false);
	}

	fn guard_read_prefix_inner(
		&mut self,
		child_info: Option<&ChildInfo>,
		key: &[u8],
		ret_result: bool,
	) -> bool {
		let blocked = Self::key_read_forbid_prefix(
			&self.filters_forbid,
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
		if self.allow_write_active {
			if !Self::guard_read_allow_prefix(
				&self.filters_allow,
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

	pub(super) fn guard_write_prefix(&mut self, child_info: Option<&ChildInfo>, key: &[u8]) {
		let _ = self.guard_write_prefix_inner(child_info, key, false);
	}

	fn guard_write_prefix_inner(
		&mut self,
		child_info: Option<&ChildInfo>,
		key: &[u8],
		ret_result: bool,
	) -> bool {
		let blocked = Self::key_write_forbid_prefix(
			&self.filters_forbid,
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
			if !Self::guard_write_allow_prefix(
				&self.filters_allow,
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
		let blocked = Self::key_forbid_interval(
			&self.filters_forbid,
			forbid_intervals,
			child_info,
			key,
			key_end,
			read,
			write,
		);
		for origin in blocked {
			if ret_result {
				return false
			}
			self.failure_handlers.invalid_accesses(origin);
		}
		if self.allow_read_active {
			if read &&
				!Self::guard_allow_interval(
					&self.filters_allow,
					&self.filters_interval_allow_read,
					child_info,
					key,
					key_end,
					read,
					false,
				) {
				if ret_result {
					return false
				}
				self.failure_handlers.invalid_allowed_access();
			}
			if write &&
				!Self::guard_allow_interval(
					&self.filters_allow,
					&self.filters_interval_allow_write,
					child_info,
					key,
					key_end,
					false,
					write,
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
pub struct Filter<F: Debug + Clone + Default> {
	read_prefix: F,
	read_key: F,
	write_prefix: F,
	write_key: F,
}

impl Filter<FilterOrigin> {
	fn is_empty(&self) -> bool {
		self.read_prefix.is_empty() &&
			self.read_key.is_empty() &&
			self.write_prefix.is_empty() &&
			self.write_key.is_empty()
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
	use crate::overlayed_changes::radix_trees::{
		FilterTreeEnd as FilterTree, FilterTreesEnd as FilterTrees,
	};

	/// TODO bad def, need a quick check algo.
	#[derive(Debug, Clone, Default)]
	pub struct FilterInterval<F: Debug + Clone + Default> {
		// TODO consider removing start of (we can avoid it by iterating next, which we do
		// when we need origins). Not sure about all case.
		start_of: F,
		end_of: F,
	}

	impl FilterInterval<FilterOrigin> {
		fn is_empty(&self) -> bool {
			self.start_of.is_empty() && self.end_of.is_empty()
		}
	}

	pub(super) type FilterTreesAllowInterval = FilterTrees<FilterInterval<bool>>;

	pub(super) type FilterTreesForbidInterval = FilterTrees<FilterInterval<FilterOrigin>>;

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
		filter: &mut FilterTrees<FilterInterval<bool>>,
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

	fn set_filter_allow_internal(
		filter: &mut FilterTree<FilterInterval<bool>>,
		decl: &LocalAccessDeclaration,
	) -> bool {
		for (start, end) in decl.intervals_lock.iter() {
			// TODO return false if overlapping or unordered
			// TODO implement entry api
			if let Some(bound) = filter.0.get_mut(start.as_slice()) {
				bound.start_of = true;
			} else {
				filter
					.0
					.insert(start.as_slice(), FilterInterval { start_of: true, end_of: false });
			}
			if let Some(end) = end.as_ref() {
				if let Some(bound) = filter.0.get_mut(end.as_slice()) {
					bound.end_of = true;
				} else {
					filter
						.0
						.insert(end.as_slice(), FilterInterval { start_of: false, end_of: true });
				}
			} else {
				filter.1.end_of = true;
			}
		}
		true
	}

	pub(super) fn set_filter_forbid(
		filter: &mut FilterTrees<FilterInterval<FilterOrigin>>,
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

	fn set_filter_forbid_internal(
		filter: &mut FilterTree<FilterInterval<FilterOrigin>>,
		decl: &LocalAccessDeclaration,
		from_child: TaskId,
	) -> bool {
		let mut insert_start;
		for (start, end) in decl.intervals_lock.iter() {
			// TODO return false if overlapping or unordered
			insert_start = None;
			let mut iter = filter.0.seek_iter_mut(start).value_iter();
			let mut do_insert_start = true;
			if let Some((k, value)) = (&mut iter).last() {
				if k == start {
					do_insert_start = false;
					value.start_of.set(from_child);
				}
			}

			let mut end_has_parent_origin = false;
			let mut iter = iter.node_iter().iter().value_iter();
			// set origin to all end into interval
			for (k, value) in &mut iter {
				if do_insert_start {
					insert_start = Some(value.end_of.clone());
					do_insert_start = false;
				}
				if end.as_ref().map(|end| &k < end).unwrap_or(true) {
					value.end_of.set(from_child);
				} else if &Some(k) == end {
					end_has_parent_origin = true;
					value.end_of.set(from_child);
					break
				} else {
					break
				}
			}
			if do_insert_start {
				// corner case of empty range
				insert_start = Some(filter.1.end_of.clone());
			}
			// find end parent intervals.
			let mut end_origin = if end_has_parent_origin {
				Default::default()
			} else if let Some((_k, value)) = iter.next() {
				value.end_of.clone()
			} else {
				filter.1.end_of.clone()
			};
			if let Some(mut start_origin) = insert_start.take() {
				start_origin.set(from_child);
				filter.0.insert(
					start.as_slice(),
					FilterInterval { start_of: start_origin, end_of: FilterOrigin::default() },
				);
			}
			if end_has_parent_origin {
				break
			}
			if let Some(end) = end.as_ref() {
				end_origin.set(from_child);
				filter.0.insert(
					end.as_slice(),
					FilterInterval { start_of: FilterOrigin::default(), end_of: end_origin },
				);
			} else {
				filter.1.end_of.set(from_child);
			}
		}
		true
	}

	pub(super) fn remove_filter_forbid(
		filter: &mut FilterTrees<FilterInterval<FilterOrigin>>,
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
		filter: &mut FilterTree<FilterInterval<FilterOrigin>>,
		decl: &LocalAccessDeclaration,
		from_child: TaskId,
	) {
		let mut to_remove = Vec::new();
		for (start, end) in decl.intervals_lock.iter() {
			let mut iter = filter.0.seek_iter_mut(start).value_iter();
			if let Some((k, value)) = (&mut iter).last() {
				value.start_of.remove(from_child);
				if value.is_empty() {
					to_remove.push(k.to_vec());
				}
			}
			let mut touched = false;
			for (k, value) in iter.node_iter().iter().value_iter() {
				if end.as_ref().map(|end| &k > end).unwrap_or(false) {
					touched = true;
					break
				}
				let at = Some(&k) == end.as_ref();
				value.end_of.remove(from_child);
				if value.is_empty() {
					to_remove.push(k);
				}
				if at {
					touched = true;
					break
				}
			}
			if !touched {
				filter.1.end_of.remove(from_child);
			}
		}
		for key in to_remove {
			filter.0.remove(key.as_slice(), false);
		}
	}

	pub(super) fn check_key_inclusion_origin<'a, D: FilterBound>(
		filter: &'a FilterTree<FilterInterval<D>>,
		key: &'a [u8],
	) -> &'a D {
		let mut iter = filter.0.seek_iter(key).value_iter();
		let _ = (&mut iter).last();
		if let Some((_k, value)) = iter.node_iter().iter().value_iter().next() {
			return &value.end_of
		}
		&filter.1.end_of
	}

	pub(super) fn check_key_inclusion<D: FilterBound>(
		filter: &FilterTree<FilterInterval<D>>,
		key: &[u8],
	) -> bool {
		let mut iter = filter.0.seek_iter(key).value_iter();
		if let Some((k, value)) = (&mut iter).last() {
			if k == key {
				if !value.start_of.is_empty() {
					return true
				}
			}
		}
		if let Some((_k, value)) = iter.node_iter().iter().value_iter().next() {
			return !value.end_of.is_empty()
		}
		!filter.1.end_of.is_empty()
	}

	// TODO this only work if we do not allow both interval and prefix definition
	// (prefix could complet interval).
	pub(super) fn check_interval_into<D: FilterBound>(
		// TODO use prefix only?
		filter: &FilterTree<FilterInterval<D>>,
		from: &[u8],
		to: Option<&[u8]>,
		inclusive: bool,
	) -> bool {
		let mut iter = filter.0.seek_iter(from).value_iter();
		let _ = (&mut iter).last(); // seek, put k after start
		for (k, value) in iter.node_iter().iter().value_iter() {
			if to
				.as_ref()
				.map(|to| (inclusive && k.as_slice() <= *to) || (!inclusive && k.as_slice() < *to))
				.unwrap_or(true)
			{
				if value.end_of.is_empty() {
					// start not into interval
					return false
				}
				if value.end_of.is_single() && value.start_of.is_empty() {
					// end interval without next interval
					return false
				}
			} else {
				return !value.end_of.is_empty()
			}
		}
		!filter.1.end_of.is_empty()
	}

	pub(super) fn check_inclusive_interval_into<D: FilterBound>(
		filter: &FilterTree<FilterInterval<D>>,
		from: &[u8],
		to: &[u8],
	) -> bool {
		check_interval_into(filter, from, Some(to), true)
	}

	pub(super) fn check_inclusive_interval_overlap<D: FilterBound>(
		filter: &FilterTree<FilterInterval<D>>,
		from: &[u8],
		to: &[u8],
	) -> bool {
		check_interval_overlap(filter, from, Some(to), true)
	}

	fn check_exclusive_interval_overlap<D: FilterBound>(
		// TODO use prefix only?
		filter: &FilterTree<FilterInterval<D>>,
		from: &[u8],
		to: Option<&[u8]>,
	) -> bool {
		check_interval_overlap(filter, from, to, false)
	}

	fn check_exclusive_interval_overlap_origins<'a, D: FilterBound>(
		// TODO use prefix only?
		filter: &'a FilterTree<FilterInterval<D>>,
		from: &'a [u8],
		to: Option<&[u8]>,
	) -> &'a D {
		check_interval_overlap_origins(filter, from, to, false)
	}

	fn check_interval_overlap<D: FilterBound>(
		// TODO use prefix only?
		filter: &FilterTree<FilterInterval<D>>,
		from: &[u8],
		to: Option<&[u8]>,
		inclusive: bool,
	) -> bool {
		let mut iter = filter.0.seek_iter(from).value_iter();
		if let Some((k, value)) = (&mut iter).last() {
			if k == from {
				if !value.start_of.is_empty() {
					return true
				}
			}
		}
		for (k, value) in iter.node_iter().iter().value_iter() {
			if !value.end_of.is_empty() {
				return true
			}
			if to
				.as_ref()
				.map(|end| {
					(inclusive && k.as_slice() > *end) || (!inclusive && k.as_slice() >= *end)
				})
				.unwrap_or(false)
			{
				return false
			}
		}
		!filter.1.end_of.is_empty()
	}

	pub(super) fn check_interval_overlap_origins<'a, D: FilterBound>(
		// TODO use prefix only?
		filter: &'a FilterTree<FilterInterval<D>>,
		from: &'a [u8],
		to: Option<&[u8]>,
		inclusive: bool,
	) -> &'a D {
		let mut iter = filter.0.seek_iter(from).value_iter();
		let _ = (&mut iter).last();
		for (k, value) in iter.node_iter().iter().value_iter() {
			if !value.end_of.is_empty() {
				return &value.end_of
			}
			if to
				.as_ref()
				.map(|end| {
					(inclusive && k.as_slice() > *end) || (!inclusive && k.as_slice() >= *end)
				})
				.unwrap_or(false)
			{
				return &value.end_of
			}
		}
		&filter.1.end_of
	}

	pub(super) fn check_prefix_overlap<D: FilterBound>(
		filter: &FilterTree<FilterInterval<D>>,
		prefix: &[u8],
	) -> bool {
		let end_prefix = crate::overlayed_changes::end_prefix(prefix);
		check_exclusive_interval_overlap::<D>(
			filter,
			prefix,
			end_prefix.as_ref().map(|e| e.as_slice()),
		)
	}

	pub(super) fn check_prefix_into<D: FilterBound>(
		filter: &FilterTree<FilterInterval<D>>,
		prefix: &[u8],
	) -> bool {
		let end_prefix = crate::overlayed_changes::end_prefix(prefix);
		check_interval_into::<D>(filter, prefix, end_prefix.as_ref().map(|e| e.as_slice()), false)
	}

	pub(super) fn check_prefix_overlap_origins<'a, D: FilterBound>(
		filter: &'a FilterTree<FilterInterval<D>>,
		// TODO this 'a is needed due to radix iter needing 'a, but if iter would manage two
		// lifetime one for the iterator and one for the actual iterated value.
		// Then we would not need it this is a bit everywhere.
		prefix: &'a [u8],
	) -> &'a D {
		let end_prefix = crate::overlayed_changes::end_prefix(prefix);
		check_exclusive_interval_overlap_origins::<D>(
			filter,
			prefix,
			end_prefix.as_ref().map(|e| e.as_slice()),
		)
	}

	#[cfg(test)]
	mod test {
		use super::*;
		// TODO fuzzing against list of defined interval could be good.
		#[test]
		fn test_1() {
			let mut filter = FilterTree::<FilterInterval<FilterOrigin>>::default();
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
			assert!(!check_inclusive_interval_overlap(&filter, b"k", b"kd"));
			assert!(check_inclusive_interval_overlap(&filter, b"k", b"ke"));
			assert!(!check_exclusive_interval_overlap(&filter, b"l", None));
			assert!(!check_key_inclusion(&filter, b"a"));
			assert!(check_key_inclusion_origin(&filter, b"a").is_empty());
			assert!(!check_key_inclusion(&filter, b"test"));
			assert!(check_key_inclusion(&filter, b"kenny"));
			assert!(check_key_inclusion(&filter, b"ke"));
			assert!(!check_key_inclusion_origin(&filter, b"ke").is_empty());
			assert!(check_inclusive_interval_into(&filter, b"ke", b"keydddddd"));
			assert!(check_inclusive_interval_into(&filter, b"kea", b"key"));
			assert!(!check_inclusive_interval_into(&filter, b"kea", b"l"));
			assert!(!check_inclusive_interval_into(&filter, b"kd", b"key"));
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
			assert!(check_inclusive_interval_into(&filter, b"kea", b"l"));
			assert!(check_inclusive_interval_into(&filter, b"kea", b"oo"));
			assert!(!check_inclusive_interval_into(&filter, b"kea", b"op"));
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
			assert!(!check_inclusive_interval_into(&filter, b"o", b"p"));
			let decl_4 = LocalAccessDeclaration {
				prefixes_lock: Default::default(),
				keys_lock: Default::default(),
				intervals_lock: vec![(b"o".to_vec(), Some(b"z".to_vec()))],
				all: false,
			};
			set_filter_forbid_internal(&mut filter, &decl_4, 4);
			assert!(check_inclusive_interval_into(&filter, b"o", b"p"));
			remove_filter_forbid_internal(&mut filter, &decl_4, 4);
			assert!(!check_inclusive_interval_into(&filter, b"o", b"p"));
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
			&filters1.filters_allow,
			&filters1.filters_interval_allow_read,
			None,
			&b"key"[..],
			Some(&b"key8"[..]),
			true,
			false,
		));
		assert!(!Filters::guard_allow_interval(
			&filters1.filters_allow,
			&filters1.filters_interval_allow_read,
			None,
			&b"key"[..],
			Some(&b"kez"[..]),
			true,
			false,
		));
		assert!(!Filters::guard_allow_interval(
			&filters1.filters_allow,
			&filters1.filters_interval_allow_read,
			None,
			&b"key"[..],
			None,
			true,
			false,
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
			&filters1.filters_allow,
			&filters1.filters_interval_allow_read,
			None,
			&b"a"[..],
			Some(&b"kez"[..]),
			true,
			false,
		));
		assert!(Filters::guard_allow_interval(
			&filters1.filters_allow,
			&filters1.filters_interval_allow_read,
			None,
			&b""[..],
			None,
			true,
			false,
		));
		assert!(Filters::guard_allow_interval(
			&filters1.filters_allow,
			&filters1.filters_interval_allow_read,
			None,
			&b"key"[..],
			None,
			true,
			false,
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
			&filters1.filters_allow,
			&filters1.filters_interval_allow_read,
			None,
			&b"key"[..],
			Some(&b"kez"[..]),
			true,
			false,
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
			&filters1.filters_allow,
			&filters1.filters_interval_allow_read,
			None,
			&b"kex"[..],
			Some(&b"kez"[..]),
			true,
			false,
		));
	}
}
