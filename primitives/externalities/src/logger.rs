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

//! State access logs primitives.

use sp_std::{
	vec::Vec,
	collections::btree_set::BTreeSet,
	collections::btree_map::BTreeMap,
};

/// Log of a given worker call.
#[derive(codec::Encode, codec::Decode, Default, Eq, PartialEq)]
#[cfg_attr(feature = "std", derive(serde::Serialize, serde::Deserialize))]
pub struct AccessLog {
	/// Worker did iterate over the full state.
	/// (in practice it did not iterate but uses
	/// merkle hash over the full state when calculating
	/// a storage root).
	pub read_all: bool,
	/// Log of access for main state.
	pub top_logger: StateLog,
	/// Log of access for individual child state.
	/// Note that the child root isn't logged in top_logger
	/// because it is always written with its right state
	/// at the end (actually triggers read_all that supersed
	/// the other fields).
	pub children_logger: Vec<(Vec<u8>, StateLog)>,
}

impl AccessLog {
	/// Return true if a read related information was logged.
	pub fn has_read(&self) -> bool {
		if self.read_all {
			return true
		}
		if self.top_logger.has_read() {
			return true
		}
		for (_key, logger) in self.children_logger.iter() {
			if logger.has_read() {
				return true
			}
		}
		false
	}

	/// Return true if a write related information was logged.
	pub fn has_write(&self) -> bool {
		if self.top_logger.has_write() {
			return true
		}
		for (_key, logger) in self.children_logger.iter() {
			if logger.has_write() {
				return true
			}
		}
		false
	}
}

/// Log of a given trie state.
#[derive(codec::Encode, codec::Decode, Default, Eq, PartialEq)]
#[cfg_attr(feature = "std", derive(serde::Serialize, serde::Deserialize))]
pub struct StateLog {
	/// Read only access to a key.
	pub read_keys: Vec<Vec<u8>>,
	/// Key write with no read access.
	pub write_keys: Vec<Vec<u8>>,
	/// Write access to a whole prefix (eg key removal
	/// by prefix).
	pub write_prefix: Vec<Vec<u8>>,
	/// Worker did iterate over a given interval.
	/// Interval is a pair of inclusive start and end key.
	pub read_intervals: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl StateLog {
	/// Return true if a read related information was logged.
	pub fn has_read(&self) -> bool {
		!self.read_keys.is_empty() || !self.read_intervals.is_empty()
	}
	/// Return true if a write related information was logged.
	pub fn has_write(&self) -> bool {
		!self.write_keys.is_empty() || !self.write_prefix.is_empty()
	}
}

/// Accumulator for logger states.
#[derive(Debug, Clone, Default)]
struct AccumLogger {
	read_all: bool,
	top_logger: AccumStateLogger,
	children_logger: BTreeMap<Vec<u8>, AccumStateLogger>,
}

impl From<AccessLog> for AccumLogger {
	fn from(log: AccessLog) -> Self {
		AccumLogger {
			read_all: log.read_all,
			top_logger: log.top_logger.into(),
			children_logger: log.children_logger.into_iter()
				.map(|(storage_key, logger)| (storage_key, logger.into()))
				.collect(),
		}
	}
}

impl AccumLogger {
	/// TODO
	pub fn accumulate_logger(&mut self, logger: AccessLog) {
		self.read_all |= logger.read_all;
		self.top_logger.accumulate_logger(logger.top_logger);
		for (storage_key, logger) in logger.children_logger {
			if let Some(child) = self.children_logger.get_mut(&storage_key) {
				child.accumulate_logger(logger);
			} else {
				self.children_logger.insert(storage_key, logger.into());
			}
		}
	}
	/// TODO
	pub fn accumulate(&mut self, logger: Self) {
		self.read_all |= logger.read_all;
		self.top_logger.accumulate(logger.top_logger);
		for (storage_key, logger) in logger.children_logger {
			if let Some(child) = self.children_logger.get_mut(&storage_key) {
				child.accumulate(logger);
			} else {
				self.children_logger.insert(storage_key, logger);
			}
		}
	}
	/// TODO
	pub fn diff(&self, logger: &AccessLog) -> Conflicts {
		let mut conflicts = self.top_logger.diff(&logger.top_logger, None, self.read_all);
		for (storage_key, logger) in logger.children_logger.iter() {
			if let Some(child) = self.children_logger.get(storage_key) {
				conflicts.accumulate(child.diff(logger, Some(storage_key), self.read_all));
			} // else child access cannot conflict
		}
		conflicts
	}
}

/// Accumulator for logger states.
#[derive(Debug, Clone, Default)]
struct AccumStateLogger {
	write_keys: BTreeSet<Vec<u8>>,
	write_prefixes: BTreeSet<Vec<u8>>,
	read_keys: BTreeSet<Vec<u8>>,
	read_intervals: BTreeSet<(Vec<u8>, Option<Vec<u8>>)>,
}

impl From<StateLog> for AccumStateLogger {
	fn from(log: StateLog) -> Self {
		AccumStateLogger {
			read_keys: log.read_keys.into_iter().collect(),
			write_keys: log.write_keys.into_iter().collect(),
			write_prefixes: log.write_prefix.into_iter().collect(),
			read_intervals: log.read_intervals.into_iter().collect(),
		}
	}
}

impl AccumStateLogger {
	/// TODO
	pub fn accumulate_logger(&mut self, logger: StateLog) {
		self.read_keys.extend(logger.read_keys);
		self.write_keys.extend(logger.write_keys);
		self.write_prefixes.extend(logger.write_prefix);
		self.read_intervals.extend(logger.read_intervals);
	}
	/// TODO
	pub fn accumulate(&mut self, mut logger: Self) {
		self.read_keys.append(&mut logger.read_keys);
		self.write_keys.append(&mut logger.write_keys);
		self.write_prefixes.append(&mut logger.write_prefixes);
		self.read_intervals.append(&mut logger.read_intervals);
	}
	/// TODO self is parent
	pub fn diff(&self, logger: &StateLog, child_trie: Option<&Vec<u8>>, read_all: bool) -> Conflicts {
		let mut conflicts = Vec::new();
		for key in logger.read_keys.iter() {
			if self.write_keys.contains(key) {
				conflicts.push(Conflict {
					child_trie: child_trie.cloned(),
					parent: ConflictOrigin::WriteKey(key.clone()),
					child: ConflictOrigin::ReadKey(key.clone()),
				});
			}
			unimplemented!("checking write prefixes needs radix-tree");
		}
		for (start, end) in logger.read_intervals.iter() {
			unimplemented!("checking write key in intervals");
			unimplemented!("checking prefix write in intervals");
		}
		if read_all {
			for key in logger.write_keys.iter() {
				conflicts.push(Conflict {
					child_trie: child_trie.cloned(),
					parent: ConflictOrigin::ReadAll,
					child: ConflictOrigin::WriteKey(key.clone()),
				});
			}
			for prefix in logger.write_prefix.iter() {
				conflicts.push(Conflict {
					child_trie: child_trie.cloned(),
					parent: ConflictOrigin::ReadAll,
					child: ConflictOrigin::WritePrefix(prefix.clone()),
				});
			}
		} else {
			for key in logger.write_keys.iter() {
				if self.read_keys.contains(key) {
					conflicts.push(Conflict {
						child_trie: child_trie.cloned(),
						parent: ConflictOrigin::ReadKey(key.clone()),
						child: ConflictOrigin::WriteKey(key.clone()),
					});
				}
				unimplemented!("TODO check read intervals");
			}
			for prefix in logger.write_prefix.iter() {
				unimplemented!("checking read key bellow prefix");
				unimplemented!("checking prefix write in intervals");
			}
		}
		Conflicts(conflicts)
	}
}

/// TODO
#[derive(Debug, Clone)]
#[cfg_attr(feature = "std", derive(serde::Serialize, serde::Deserialize))]
pub struct Conflicts(Vec<Conflict>);

impl Conflicts {
	/// TODO
	pub fn accumulate(&mut self, other: Self) {
		self.0.extend(other.0);
	}
}

/// TODO
#[derive(Debug, Clone)]
#[cfg_attr(feature = "std", derive(serde::Serialize, serde::Deserialize))]
pub struct Conflict {
	/// TODO
	pub child_trie: Option<Vec<u8>>,
	/// TODO
	pub parent: ConflictOrigin,
	// for sequential: child is the one called after:
	// when joining it checks against a parent accum build
	// previously joined children.
	/// TODO
	pub child: ConflictOrigin,
}

/// TODO
#[derive(Debug, Clone)]
#[cfg_attr(feature = "std", derive(serde::Serialize, serde::Deserialize))]
pub enum ConflictOrigin {
	ReadAll,
	ReadKey(Vec<u8>),
	ReadInterval((Vec<u8>, Option<Vec<u8>>)),
	WriteKey(Vec<u8>),
	WritePrefix(Vec<u8>),
}
