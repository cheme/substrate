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

use super::StorageKey;
use failure::{DeclFailureHandling, FailureHandlers};
use sp_core::storage::{ChildInfo, ChildType, PrefixedStorageKey};
use sp_externalities::{
	AccessDeclaration, LocalAccessDeclaration, DeclarationFailureHandling, TaskId, WorkerDeclarationKind, WorkerResult,
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

type FilterTreesAllowReadInterval = super::radix_trees::FilterTrees<FilterInterval<bool>>;
type FilterTreesAllowWriteInterval = super::radix_trees::FilterTrees<FilterInterval<bool>>;

// Forbid for parent worker. Forbid always override allow.
type FilterTreesForbid = super::radix_trees::FilterTrees<Filter<FilterOrigin>>;
type FilterTreeForbid = super::radix_trees::FilterTree<Filter<FilterOrigin>>;

type FilterTreesForbidReadInterval = super::radix_trees::FilterTrees<FilterInterval<FilterOrigin>>;
type FilterTreeForbidWriteInterval = super::radix_trees::FilterTree<FilterInterval<FilterOrigin>>;

// TODO same consuming function?
fn prefixed_storage_key_to_child_info(prefixed_storage_key: &Vec<u8>) -> ChildInfo {
	let prefixed_key = PrefixedStorageKey::new_ref(prefixed_storage_key);
	match ChildType::from_prefixed_key(prefixed_key) {
		Some((ChildType::ParentKeyId, storage_key)) =>
			ChildInfo::new_default(storage_key),
		None => panic!("Unsupported child key"), // actually should be unreachable.
	}
}

/// Main filter state.
#[derive(Debug, Clone)]
pub(super) struct Filters {
	failure_handlers: FailureHandlers,
	allow_read_active: bool,
	allow_write_active: bool,
	allow_append_active: bool,
	/* TODO instead of commented code, have a Allow child by prefix and forbid child by prefix
	 * for parent: then all are allowed like if empty prefix defined in them.
	 * Currently allow enable means we need to specify each child but we may not know.
	/// if true all child state are forbid.
	allow_read_child_active: bool,
	allow_write_child_active: bool,
	allow_append_child_active: bool,
	*/
	/// accesses allowed by parent.
	filters_allow: FilterTreesAllow,
	/// accesses forbid due to child.
	filters_forbid: FilterTreesForbid,
	/// keeping history to remove constraint on join or dismiss.
	/// It contains constraint for the parent (child do not need
	/// to remove contraint), indexed by its relative child id.
	changes: BTreeMap<TaskId, Vec<(Option<StorageKey>, WorkerDeclarationKind)>>,
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
			allow_append_active: Default::default(),
			/*allow_read_child_active: Default::default(),
			allow_write_child_active: Default::default(),
			allow_append_child_active: Default::default(),*/
			filters_allow: Default::default(),
			filters_forbid: Default::default(),
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
		append: bool,
	) {
		Self::remove_filter_internal(&mut tree.top, filter.top, from_child, read, write, append);
		for (prefixed_storage_key, filter_child) in filter.children {
			if let Some(child) = tree.children.get_mut(&prefixed_storage_key) {
				Self::remove_filter_internal(child, filter_child, from_child, read, write, append);
			}
		}
	}

	fn remove_filter_internal(
		tree: &mut FilterTreeForbid,
		filter: LocalAccessDeclaration,
		from_child: TaskId,
		read: bool,
		write: bool,
		append: bool,
	) {
		for prefix in filter.prefixes_lock {
			if let Some(filter) = tree.get_mut(prefix.as_slice()) {
				if read {
					filter.read_prefix.remove(from_child);
				}
				if write {
					filter.write_prefix.remove(from_child);
				}
				if append {
					filter.append_prefix.remove(from_child);
				}
				if filter.is_empty() {
					tree.remove(prefix.as_slice());
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
				if append {
					filter.append_key.remove(from_child);
				}
				if filter.is_empty() {
					tree.remove(key.as_slice());
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
		append: bool,
	) {
		Self::set_filter_forbid_internal(&mut tree.top, filter.top, from_child, read, write, append);
		for (prefixed_storage_key, filter_child) in filter.children {
			let mut child = tree.children.entry(prefixed_storage_key).or_default();
			Self::set_filter_forbid_internal(&mut child, filter_child, from_child, read, write, append);
		}
	}

	fn set_filter_forbid_internal(
		tree: &mut FilterTreeForbid,
		filter: LocalAccessDeclaration,
		from_child: TaskId,
		read: bool,
		write: bool,
		append: bool,
	) {
		let set = |filter: &mut Filter<FilterOrigin>| {
			if read {
				filter.read_prefix.set(from_child);
			}
			if write {
				filter.write_prefix.set(from_child);
			}
			if append {
				filter.append_prefix.set(from_child);
			}
		};
		for prefix in filter.prefixes_lock {
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
			if append {
				filter.append_key.set(from_child);
			}
		};
		for key in filter.keys_lock {
			if let Some(filter) = tree.get_mut(key.as_slice()) {
				set(filter);
			} else {
				// new entry
				let mut filter = Filter::<FilterOrigin>::default();
				set(&mut filter);
				tree.insert(key.as_slice(), filter);
			}
		}
	}

	fn set_filter_allow(
		tree: &mut FilterTreesAllow,
		filter: AccessDeclaration,
		read: bool,
		write: bool,
		append: bool,
	) {
		Self::set_filter_allow_internal(&mut tree.top, filter.top, read, write, append);
		for (prefixed_storage_key, filter_child) in filter.children {
			let mut child = tree.children.entry(prefixed_storage_key).or_default();
			Self::set_filter_allow_internal(&mut child, filter_child, read, write, append);
		}
	}

	fn set_filter_allow_internal(
		tree: &mut FilterTreeAllow,
		filter: LocalAccessDeclaration,
		read: bool,
		write: bool,
		append: bool,
	) {
		let set = |filter: &mut Filter<bool>| {
			if read {
				filter.read_prefix = read;
			}
			if write {
				filter.write_prefix = write;
			}
			if append {
				filter.append_prefix = append
			}
		};
		for prefix in filter.prefixes_lock {
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
			if append {
				filter.append_key = append
			}
		};
		for key in filter.keys_lock {
			if let Some(filter) = tree.get_mut(key.as_slice()) {
				set(filter);
			} else {
				// new entry
				let mut filter = Filter::<bool>::default();
				set(&mut filter);
				tree.insert(key.as_slice(), filter);
			}
		}
	}

	pub(super) fn allow_appends(&mut self, filter: AccessDeclaration) {
		self.allow_append_active = true;
		Self::set_filter_allow(&mut self.filters_allow, filter, false, false, true);
	}

	pub(super) fn allow_writes(&mut self, filter: AccessDeclaration) {
		self.allow_write_active = true;
		// not defining write implies read (write do forbid all which include read forbid
		// set too).
		Self::set_filter_allow(&mut self.filters_allow, filter, true, true, false);
	}

	pub(super) fn allow_reads(&mut self, filter: AccessDeclaration) {
		self.allow_read_active = true;
		Self::set_filter_allow(&mut self.filters_allow, filter, true, false, false);
	}

	pub(super) fn add_change(&mut self, decl: WorkerDeclarationKind, from_child: TaskId) {
		// TODO is there something else than None variant bellow?
		self.changes.insert(from_child, vec![(None, decl)]);
	}

	pub(super) fn forbid_read_writes(&mut self, filter: AccessDeclaration, from_child: TaskId) {
		Self::set_filter_forbid(&mut self.filters_forbid, filter, from_child, true, true, false);
	}

	fn remove_forbid_read_writes(&mut self, filter: AccessDeclaration, from_child: TaskId) {
		Self::remove_filter(&mut self.filters_forbid, filter, from_child, true, true, false);
	}

	pub(super) fn forbid_all(&mut self, filter: AccessDeclaration, from_child: TaskId) {
		Self::set_filter_forbid(&mut self.filters_forbid, filter, from_child, true, true, true);
	}

	fn remove_forbid_all(&mut self, filter: AccessDeclaration, from_child: TaskId) {
		Self::remove_filter(&mut self.filters_forbid, filter, from_child, true, true, true);
	}

	pub(super) fn forbid_write_appends(&mut self, filter: AccessDeclaration, from_child: TaskId) {
		Self::set_filter_forbid(&mut self.filters_forbid, filter, from_child, false, true, true);
	}

	pub(super) fn remove_forbid_write_appends(
		&mut self,
		filter: AccessDeclaration,
		from_child: TaskId,
	) {
		Self::remove_filter(&mut self.filters_forbid, filter, from_child, false, true, true);
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

	fn guard_declare_child_filter_read_inner(&mut self, child_info: Option<&ChildInfo>, filter: &LocalAccessDeclaration) -> bool {
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

	fn guard_declare_child_filter_write_inner(&mut self, child_info: Option<&ChildInfo>, filter: &LocalAccessDeclaration) -> bool {
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
		true
	}

	pub(super) fn guard_declare_child_filter_append(&mut self, filter: &AccessDeclaration) -> bool {
		if !self.guard_declare_child_filter_append_inner(None, &filter.top) {
			return false
		}
		for (prefixed_storage_key, filter) in filter.children.iter() {
			let child_info = prefixed_storage_key_to_child_info(prefixed_storage_key);
			if !self.guard_declare_child_filter_append_inner(Some(&child_info), filter) {
				return false
			}
		}
		true
	}

	fn guard_declare_child_filter_append_inner(&mut self, child_info: Option<&ChildInfo>, filter: &LocalAccessDeclaration) -> bool {
		for _prefix in filter.prefixes_lock.iter() {
			// prefix for append got no use case (we got prefix for delete only and interval for read
			// only).
			return false;
			/* if !self.guard_append_prefix_inner(None, top_prefix, true) {
				return false
			}*/
		}
		for key in filter.keys_lock.iter() {
			if !self.guard_append_inner(child_info, key, true) {
				return false
			}
		}
		true
	}

	pub(super) fn on_worker_result(&mut self, result: &WorkerResult) -> bool {
		let did_fail = self.failure_handlers.did_fail();
		match result {
			WorkerResult::CallAt(_result, _delta, marker) => {
				self.remove_worker(*marker);
			},
			WorkerResult::Optimistic(_result, _delta, marker, ..) => {
				// This is actually a noops since optimistic don't
				// use filter.
				self.remove_worker(*marker);
			},
			// When using filter there is no directly valid case.
			WorkerResult::Valid(..) => (),
			// When using filter there is no invalid case.
			WorkerResult::Invalid => (),
			// will panic on resolve so no need to clean filter
			WorkerResult::RuntimePanic | WorkerResult::HardPanic => (),
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
		let blocked = Self::key_read_forbid(&self.filters_forbid, child_info, key);
		for origin in blocked {
			if ret_result {
				return false
			}
			self.failure_handlers.invalid_accesses(origin);
		}
		if self.allow_read_active {
			if !Self::guard_read_allow(&self.filters_allow, child_info, key) {
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
		let blocked = Self::key_write_forbid(&self.filters_forbid, child_info, key);
		for origin in blocked {
			if ret_result {
				return false
			}
			self.failure_handlers.invalid_accesses(origin);
		}
		if self.allow_write_active {
			if !Self::guard_write_allow(&self.filters_allow, child_info, key) {
				if ret_result {
					return false
				}
				self.failure_handlers.invalid_allowed_access();
			}
		}
		true
	}

	/// Guard invalid access for append at key.
	pub(super) fn guard_append(&mut self, child_info: Option<&ChildInfo>, key: &[u8]) {
		let _ = self.guard_append_inner(child_info, key, false);
	}

	fn guard_append_inner(
		&mut self,
		child_info: Option<&ChildInfo>,
		key: &[u8],
		ret_result: bool,
	) -> bool {
		let blocked = Self::key_append_forbid(&self.filters_forbid, child_info, key);
		for origin in blocked {
			if ret_result {
				return false
			}
			self.failure_handlers.invalid_accesses(origin);
		}
		if self.allow_append_active {
			if !Self::guard_append_allow(&self.filters_allow, child_info, key) {
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
		child_info: Option<&ChildInfo>,
		key: &'a [u8],
	) -> Vec<&'a FilterOrigin> {
		let mut blocked = Vec::new();
		let filters = if let Some(filter) = filters.filter(child_info) {
			filter
		} else {
			return blocked
		};
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
		child_info: Option<&ChildInfo>,
		key: &'a [u8],
	) -> Vec<&'a FilterOrigin> {
		let mut blocked = Vec::new();
		let filters = if let Some(filter) = filters.filter(child_info) {
			filter
		} else {
			return blocked
		};
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
		blocked
	}

	// TODO factor with key_write_forbid
	fn key_append_forbid<'a>(
		filters: &'a FilterTreesForbid,
		child_info: Option<&ChildInfo>,
		key: &'a [u8],
	) -> Vec<&'a FilterOrigin> {
		let mut blocked = Vec::new();
		let filters = if let Some(filter) = filters.filter(child_info) {
			filter
		} else {
			return blocked
		};
		let len = key.len();
		for (key, value) in filters.seek_iter(key).value_iter() {
			if value.append_prefix.is_defined() {
				blocked.push(&value.append_prefix);
			}
			if len == key.len() {
				if value.append_key.is_defined() {
					blocked.push(&value.read_key);
				}
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
		child_info: Option<&ChildInfo>,
		key: &'a [u8],
	) -> Vec<&'a FilterOrigin> {
		let mut blocked = Vec::new();
		let filters = if let Some(filter) = filters.filter(child_info) {
			filter
		} else {
			return blocked
		};
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
		child_info: Option<&ChildInfo>,
		key: &'a [u8],
	) -> Vec<&'a FilterOrigin> {
		let mut blocked = Vec::new();
		let filters = if let Some(filter) = filters.filter(child_info) {
			filter
		} else {
			return blocked
		};
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

	/*
	fn key_append_forbid_prefix<'a>(
		filters: &'a FilterTreesForbid,
		child_info: Option<&ChildInfo>,
		key: &'a [u8],
	) -> Vec<&'a FilterOrigin> {
		let mut blocked = Vec::new();
		let filters = if let Some(filter) = filters.filter(child_info) {
			filter
		} else {
			return blocked
		};
		let len = key.len();
		let mut iter = filters.seek_iter(key).value_iter();
		while let Some((key, value)) = iter.next() {
			if value.append_prefix.is_defined() {
				blocked.push(&value.append_prefix);
			}
			if len == key.len() {
				if value.append_key.is_defined() {
					blocked.push(&value.append_key);
				}
			}
		}
		for (_key, value) in iter.node_iter().iter_prefix().value_iter() {
			if value.append_prefix.is_defined() {
				blocked.push(&value.append_prefix);
			}
			if value.append_key.is_defined() {
				blocked.push(&value.append_key);
			}
		}
		blocked
	}
	*/

	fn key_read_forbid_interval<'a>(
		filters: &'a FilterTreesForbid,
		child_info: Option<&ChildInfo>,
		key: &'a [u8],
		key_end: Option<&'a [u8]>,
	) -> Vec<&'a FilterOrigin> {
		let mut blocked = Vec::new();
		let filters = if let Some(filter) = filters.filter(child_info) {
			filter
		} else {
			return blocked
		};
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
		for (key, value) in iter.node_iter().iter().value_iter() {
			if key_end.map(|end| key.as_slice() <= end).unwrap_or(true) {
				if value.read_prefix.is_defined() {
					blocked.push(&value.read_prefix);
				}
				if value.read_key.is_defined() {
					blocked.push(&value.read_key);
				}
			} else {
				break
			}
		}

		blocked
	}

	/// allow iteration is only allowing interval definition.
	fn guard_read_allow_interval(
		filters: &FilterTreesAllow,
		child_info: Option<&ChildInfo>,
		key: &[u8],
		key_end: Option<&[u8]>,
	) -> bool {
		let filters = if let Some(filter) = filters.filter(child_info) {
			filter
		} else {
			return false;
		};
		let mut iter = filters.seek_iter(key).value_iter();
		let mut start_interval = None;
		while let Some((key, value)) = iter.next() {
			// forbid write implies forbid read.
			if value.read_prefix {
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
			if value.read_prefix {
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

	/// Return false if blocked.
	fn guard_read_allow(
		filters: &FilterTreesAllow,
		child_info: Option<&ChildInfo>,
		key: &[u8],
	) -> bool {
		let filters = if let Some(filter) = filters.filter(child_info) {
			filter
		} else {
			return false;
		};
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

	//TODO factor with guard_read_alow
	/// Return false if blocked.
	fn guard_write_allow(
		filters: &FilterTreesAllow,
		child_info: Option<&ChildInfo>,
		key: &[u8],
	) -> bool {
		let filters = if let Some(filter) = filters.filter(child_info) {
			filter
		} else {
			return false;
		};
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

	//TODO factor with guard_read_alow
	/// Return false if blocked.
	fn guard_append_allow(
		filters: &FilterTreesAllow,
		child_info: Option<&ChildInfo>,
		key: &[u8],
	) -> bool {
		let filters = if let Some(filter) = filters.filter(child_info) {
			filter
		} else {
			return false;
		};
		let len = key.len();
		for (key, value) in filters.seek_iter(key).value_iter() {
			// forbid write implies forbid read.
			if value.append_prefix {
				return true
			}
			if len == key.len() {
				if value.append_key {
					return true
				}
			}
		}
		false
	}

	fn guard_read_allow_prefix(
		filters: &FilterTreesAllow,
		child_info: Option<&ChildInfo>,
		key: &[u8],
	) -> bool {
		let filters = if let Some(filter) = filters.filter(child_info) {
			filter
		} else {
			return false;
		};
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
		child_info: Option<&ChildInfo>,
		key: &[u8],
	) -> bool {
		let filters = if let Some(filter) = filters.filter(child_info) {
			filter
		} else {
			return false;
		};
		for (_key, value) in filters.seek_iter(key).value_iter() {
			if value.write_prefix {
				return true
			}
		}
		false
	}

	/*
	fn guard_append_allow_prefix(
		filters: &FilterTreesAllow,
		child_info: Option<&ChildInfo>,
		key: &[u8],
	) -> bool {
		let filters = if let Some(filter) = filters.filter(child_info) {
			filter
		} else {
			return true
		};
		for (_key, value) in filters.seek_iter(key).value_iter() {
			if value.append_prefix {
				return true
			}
		}
		false
	}
	*/

	fn guard_read_prefix(&mut self, child_info: Option<&ChildInfo>, key: &[u8]) {
		let _ = self.guard_read_prefix_inner(child_info, key, false);
	}

	fn guard_read_prefix_inner(
		&mut self,
		child_info: Option<&ChildInfo>,
		key: &[u8],
		ret_result: bool,
	) -> bool {
		let blocked = Self::key_read_forbid_prefix(&self.filters_forbid, child_info, key);
		for origin in blocked {
			if ret_result {
				return false
			}
			self.failure_handlers.invalid_accesses(origin);
		}
		if self.allow_write_active {
			if !Self::guard_read_allow_prefix(&self.filters_allow, child_info, key) {
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
		let blocked = Self::key_write_forbid_prefix(&self.filters_forbid, child_info, key);
		for origin in blocked {
			if ret_result {
				return false
			}
			self.failure_handlers.invalid_accesses(origin);
		}
		if self.allow_write_active {
			if !Self::guard_write_allow_prefix(&self.filters_allow, child_info, key) {
				if ret_result {
					return false
				}
				self.failure_handlers.invalid_allowed_access();
			}
		}
		true
	}

	/*
	pub(super) fn guard_append_prefix(&mut self, child_info: Option<&ChildInfo>, key: &[u8]) {
		let _ = self.guard_append_prefix_inner(child_info, key, false);
	}

	fn guard_append_prefix_inner(
		&mut self,
		child_info: Option<&ChildInfo>,
		key: &[u8],
		ret_result: bool,
	) -> bool {
		let blocked = Self::key_append_forbid_prefix(&self.filters_forbid, child_info, key);
		for origin in blocked {
			if ret_result {
				return false
			}
			self.failure_handlers.invalid_accesses(origin);
		}
		if self.allow_write_active {
			if !Self::guard_append_allow_prefix(&self.filters_allow, child_info, key) {
				if ret_result {
					return false
				}
				self.failure_handlers.invalid_allowed_access();
			}
		}
		true
	}
	*/

	pub(super) fn guard_read_interval(
		&mut self,
		child_info: Option<&ChildInfo>,
		key: &[u8],
		key_end: Option<&[u8]>,
	) {
		let blocked =
			Self::key_read_forbid_interval(&self.filters_forbid, child_info, key, key_end);
		for origin in blocked {
			self.failure_handlers.invalid_accesses(origin);
		}
		if self.allow_read_active {
			if !Self::guard_read_allow_interval(&self.filters_allow, child_info, key, key_end) {
				self.failure_handlers.invalid_allowed_access();
			}
		}
	}

	pub(super) fn remove_worker(&mut self, task_id: TaskId) {
		if let Some(decl) = self.changes.remove(&task_id) {
			for (_child_storage, declaration) in decl.into_iter() {
				match declaration {
					WorkerDeclarationKind::Stateless |
					WorkerDeclarationKind::ReadLastBlock |
					WorkerDeclarationKind::ReadAtSpawn |
					WorkerDeclarationKind::WriteAtSpawn => (),
					WorkerDeclarationKind::ReadOptimistic => (),
					WorkerDeclarationKind::WriteLightOptimistic => (),
					WorkerDeclarationKind::WriteOptimistic => (),
					WorkerDeclarationKind::ReadDeclarative(filter, _failure) => {
						// undo a `set_parent_declaration` call.
						self.failure_handlers.remove(task_id);
						self.remove_forbid_write_appends(filter, task_id);
					},
					WorkerDeclarationKind::WriteLightDeclarative(filter, _failure) => {
						self.failure_handlers.remove(task_id);
						self.remove_forbid_all(filter, task_id);
					},
					WorkerDeclarationKind::WriteDeclarative(filters, _failure) => {
						self.failure_handlers.remove(task_id);
						self.remove_forbid_write_appends(filters.read_only, task_id);
						self.remove_forbid_all(filters.write, task_id);
						self.remove_forbid_read_writes(filters.append, task_id);
					},
				}
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
	append_prefix: F,
	append_key: F,
}


/// TODO bad def, need a quick check algo.
#[derive(Debug, Clone, Default)]
pub struct FilterInterval<F: Debug + Clone + Default> {
	start_of: Option<(F, u32)>,
	end_of: Option<(F, u32)>,
	key: F,
}

impl Filter<FilterOrigin> {
	fn is_empty(&self) -> bool {
		self.read_prefix.is_empty() &&
			self.read_key.is_empty() &&
			self.write_prefix.is_empty() &&
			self.write_key.is_empty() &&
			self.append_prefix.is_empty() &&
			self.append_key.is_empty()
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

#[cfg(test)]
mod test {
	use super::*;

	#[test]
	fn test_guard_read_allow_interval() {
		let mut filters1 = Filters::default();
		filters1.allow_writes(AccessDeclaration {
			top: LocalAccessDeclaration {
				prefixes_lock: vec![b"key".to_vec()],
				keys_lock: vec![],
			},
			children: Default::default(),
		});
		assert!(Filters::guard_read_allow_interval(
			&filters1.filters_allow,
			None,
			&b"key"[..],
			Some(&b"key8"[..]),
		));
		assert!(!Filters::guard_read_allow_interval(
			&filters1.filters_allow,
			None,
			&b"key"[..],
			Some(&b"kez"[..]),
		));
		assert!(!Filters::guard_read_allow_interval(
			&filters1.filters_allow,
			None,
			&b"key"[..],
			None,
		));
		//let mut filters1 = Filters::default();
		filters1.allow_writes(AccessDeclaration{
			top: LocalAccessDeclaration {
				prefixes_lock: vec![b"".to_vec()],
				keys_lock: vec![],
			},
			children: Default::default(),
		});
		assert!(Filters::guard_read_allow_interval(
			&filters1.filters_allow,
			None,
			&b"a"[..],
			Some(&b"kez"[..]),
		));
		assert!(Filters::guard_read_allow_interval(&filters1.filters_allow, None, &b""[..], None,));
		assert!(Filters::guard_read_allow_interval(
			&filters1.filters_allow,
			None,
			&b"key"[..],
			None,
		));

		let mut filters1 = Filters::default();
		filters1.allow_writes(AccessDeclaration {
			top: LocalAccessDeclaration {
				prefixes_lock: vec![b"key".to_vec(), b"kez".to_vec()],
				keys_lock: vec![],
			},
			children: Default::default(),
		});
		assert!(Filters::guard_read_allow_interval(
			&filters1.filters_allow,
			None,
			&b"key"[..],
			Some(&b"kez"[..]),
		));

		let mut filters1 = Filters::default();
		filters1.allow_writes(AccessDeclaration {
			top: LocalAccessDeclaration {
				prefixes_lock: vec![b"kex".to_vec(), b"kez".to_vec()],
				keys_lock: vec![],
			},
			children: Default::default(),
		});
		assert!(!Filters::guard_read_allow_interval(
			&filters1.filters_allow,
			None,
			&b"kex"[..],
			Some(&b"kez"[..]),
		));
	}
}
