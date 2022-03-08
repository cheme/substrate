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

//! State access loggers.
//!
//! When optimistic worker is runnig, we logg our externalities access to be able
//! to resolve if the assumptions are correct on `join`.
//!
//! It is plugged in the overlay by commodity, but could be at a higher level.
//!
//! TODO checks method should be rewrite by zip iterating on all sorted components,
//! and maintaining a streaming state machine,
//! this would also require a 'jump_to' instruction on tree iterator.

use super::{retain_map, StorageKey};
use crate::overlayed_changes::radix_trees::AccessTreeWrite;
use sp_core::storage::ChildInfo;
use sp_externalities::{StateLog, TaskId, WorkerResult, WorkerState};
#[cfg(not(feature = "std"))]
use sp_std::collections::btree_map::BTreeMap as Map;
#[cfg(not(feature = "std"))]
use sp_std::collections::btree_set::BTreeSet as Set;
use sp_std::{collections::btree_set::BTreeSet, vec::Vec};
#[cfg(feature = "std")]
use std::collections::HashMap as Map;
#[cfg(feature = "std")]
use std::collections::HashSet as Set;

#[derive(Debug, Clone)]
pub(super) struct AccessLogger {
	/// Log that we access all key in read mode (usually by calling storage root).
	/// TODO this should be implied by parent_log_read??
	read_all: bool,
	/// Keep trace of read logging required to compare with children
	/// logged access.
	log_read: bool,
	/// Keep trace of write logging required to compare with children
	/// logged access.
	log_write: bool,
	/// Access logger for top trie.
	top_logger: StateLogger,
	/// Access logger for children trie.
	children_logger: Map<StorageKey, StateLogger>,
}

impl Default for AccessLogger {
	fn default() -> Self {
		let mut logger = AccessLogger {
			read_all: Default::default(),
			log_read: Default::default(),
			log_write: Default::default(),
			top_logger: Default::default(),
			children_logger: Map::new(),
		};

		logger.log_reads_against();
		logger.log_writes_against();

		logger
	}
}
/// Logger for a given trie state.
#[derive(Debug, Clone, Default)]
struct StateLogger {
	keys_read: Set<Vec<u8>>,
	keys_write: Set<Vec<u8>>,
	// Intervals are inclusive for start and end.
	read_intervals: Set<(Vec<u8>, Option<Vec<u8>>)>,
	// this is roughly clear prefix.
	write_prefixes: AccessTreeWrite,
}

impl StateLogger {
	fn remove_all_read_logs(&mut self) {
		self.keys_read.clear();
		self.read_intervals.clear();
	}
}

impl AccessLogger {
	pub(super) fn log_writes_against(&mut self) {
		self.log_write = true;
	}

	pub(super) fn log_reads_against(&mut self) {
		self.log_read = true;
	}

	pub(super) fn on_worker_result(&mut self, result: &WorkerResult) -> bool {
		true
	}

	pub(super) fn log_read_all(&mut self) {
		if self.log_read {
			self.top_logger.remove_all_read_logs();
			for (_, logger) in self.children_logger.iter_mut() {
				logger.remove_all_read_logs();
			}
			self.read_all = true;
		}
	}

	fn logger_mut<'a>(
		top_logger: &'a mut StateLogger,
		children_logger: &'a mut Map<StorageKey, StateLogger>,
		storage_key: Option<&[u8]>,
	) -> &'a mut StateLogger {
		if let Some(storage_key) = storage_key {
			children_logger.entry(storage_key.to_vec()).or_insert_with(Default::default)
		} else {
			top_logger
		}
	}

	pub(super) fn log_read(&mut self, child_info: Option<&ChildInfo>, key: &[u8]) {
		self.log_read_storage_key(child_info.map(|child_info| child_info.storage_key()), key)
	}

	fn log_read_storage_key(&mut self, storage_key: Option<&[u8]>, key: &[u8]) {
		if self.log_read && !self.read_all {
			let logger = if let Some(storage_key) = storage_key {
				if !self.children_logger.contains_key(storage_key) {
					self.children_logger.insert(storage_key.to_vec(), Default::default());
				}
				self.children_logger.get_mut(storage_key).expect("lazy init above")
			} else {
				&mut self.top_logger
			};
			logger.keys_read.insert(key.to_vec());
		}
	}

	pub(super) fn log_read_interval(
		&mut self,
		child_info: Option<&ChildInfo>,
		key: &[u8],
		key_end: Option<&[u8]>,
	) {
		self.log_read_interval_storage_key(
			child_info.map(|child_info| child_info.storage_key()),
			key,
			key_end,
		)
	}

	fn log_read_interval_storage_key(
		&mut self,
		storage_key: Option<&[u8]>,
		key: &[u8],
		key_end: Option<&[u8]>,
	) {
		if self.log_read && !self.read_all {
			let logger = if let Some(storage_key) = storage_key {
				if !self.children_logger.contains_key(storage_key) {
					self.children_logger.insert(storage_key.to_vec(), Default::default());
				}
				self.children_logger.get_mut(storage_key).expect("lazy init above")
			} else {
				&mut self.top_logger
			};
			// TODO try merge interval!!
			let mut entry =
				logger.read_intervals.insert((key.to_vec(), key_end.map(|end| end.to_vec())));
		}
	}

	//	fn guard_write(&self, child_info: Option<&ChildInfo>, key: &[u8]) {
	pub(super) fn log_write(&mut self, child_info: Option<&ChildInfo>, key: &[u8]) {
		self.log_write_storage_key(child_info.map(|child_info| child_info.storage_key()), key)
	}

	fn log_write_storage_key(&mut self, storage_key: Option<&[u8]>, key: &[u8]) {
		if self.log_write {
			let logger =
				Self::logger_mut(&mut self.top_logger, &mut self.children_logger, storage_key);
			logger.keys_write.insert(key.to_vec());
		}
	}

	//	fn guard_write_prefix(&self, child_info: Option<&ChildInfo>, key: &[u8]) {
	pub(super) fn log_write_prefix(&mut self, child_info: Option<&ChildInfo>, key: &[u8]) {
		self.log_write_prefix_storage_key(
			child_info.map(|child_info| child_info.storage_key()),
			key,
		)
	}

	fn log_write_prefix_storage_key(&mut self, storage_key: Option<&[u8]>, key: &[u8]) {
		if self.log_write {
			// TODO replace by interval usage (remove radix trie dep).
			let logger =
				Self::logger_mut(&mut self.top_logger, &mut self.children_logger, storage_key);
			logger.write_prefixes.insert(key, ());
		}
	}

	pub(super) fn extract_parent_log(&mut self) -> Option<sp_externalities::AccessLog> {
		if !self.log_read && !self.log_write {
			return None
		}

		let mut result = sp_externalities::AccessLog::default();

		result.read_all = self.read_all;
		self.read_all = false;

		let read = !result.read_all && self.log_read;
		let write = self.log_write;
		if read {
			result.top_logger.read_keys =
				sp_std::mem::take(&mut self.top_logger.keys_read).into_iter().collect();
		}
		if write {
			result.top_logger.write_keys =
				sp_std::mem::take(&mut self.top_logger.keys_write).into_iter().collect();
		}
		if read {
			result.top_logger.read_intervals =
				sp_std::mem::take(&mut self.top_logger.read_intervals)
					.into_iter()
					.map(|key| key)
					.collect();
		}
		if write {
			result.top_logger.write_prefix =
				self.top_logger.write_prefixes.iter().value_iter().map(|(key, _)| key).collect();
			self.top_logger.write_prefixes.clear();
		}
		result.children_logger = sp_std::mem::take(&mut self.children_logger)
			.into_iter()
			.map(|(storage_key, mut logger)| {
				let mut log = StateLog::default();
				if read {
					log.read_keys = sp_std::mem::take(&mut logger.keys_read).into_iter().collect();
				}
				if write {
					log.write_keys =
						sp_std::mem::take(&mut logger.keys_write).into_iter().collect();
				}
				if read {
					log.read_intervals = sp_std::mem::take(&mut logger.read_intervals)
						.into_iter()
						.map(|key| key)
						.collect();
				}
				if write {
					log.write_prefix =
						logger.write_prefixes.iter().value_iter().map(|(key, _)| key).collect();
					logger.write_prefixes.clear();
				}

				(storage_key.clone(), log)
			})
			.collect();

		Some(result)
	}
}
