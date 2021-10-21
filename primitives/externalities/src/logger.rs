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
#[derive(codec::Encode, codec::Decode, Default, Eq, PartialEq, Debug, Clone)]
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
#[derive(codec::Encode, codec::Decode, Default, Eq, PartialEq, Debug, Clone)]
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
#[cfg_attr(feature = "std", derive(serde::Serialize, serde::Deserialize))]
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
		self.top_logger.accumulate_logger(logger.top_logger);
		for (storage_key, logger) in logger.children_logger {
			if let Some(child) = self.children_logger.get_mut(&storage_key) {
				child.accumulate_logger(logger);
			} else {
				self.children_logger.insert(storage_key, logger.into());
			}
		}
	}
/*	/// TODO
	pub fn accumulate(&mut self, logger: Self) {
		for (storage_key, logger) in logger.children_logger {
			if let Some(child) = self.children_logger.get_mut(&storage_key) {
				child.accumulate(logger);
			} else {
				self.children_logger.insert(storage_key, logger);
			}
		}
	}*/
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
/// This is representing parent state.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "std", derive(serde::Serialize, serde::Deserialize))]
struct AccumStateLogger {
	// From children declared writes.
	forbid_read: BTreeSet<Vec<u8>>,
	// From children declared writes.
	forbid_read_prefixes: BTreeSet<Vec<u8>>,
}

impl From<StateLog> for AccumStateLogger {
	fn from(log: StateLog) -> Self {
		AccumStateLogger {
			forbid_read: log.write_keys.into_iter().collect(),
			forbid_read_prefixes: log.write_prefix.into_iter().collect(),
		}
	}
}

/// Get exclusive end interval corresponding to a prefix.
/// TODO duplicated code with overlayed changes.
pub fn end_prefix(prefix: &[u8]) -> Option<Vec<u8>> {
	let mut end_range = prefix.to_vec();
	while let Some(0xff) = end_range.last() {
		end_range.pop();
	}
	if let Some(byte) = end_range.last_mut() {
		*byte += 1;
		Some(end_range)
	} else {
		None
	}
}

impl AccumStateLogger {
	/// TODO
	pub fn accumulate_logger(&mut self, logger: StateLog) {
		self.forbid_read.extend(logger.write_keys);
		self.forbid_read_prefixes.extend(logger.write_prefix);
	}
/*	/// TODO
	pub fn accumulate(&mut self, mut logger: Self) {
		self.forbid_read.append(&mut logger.forbid_read);
		self.forbid_read_prefixes.append(&mut logger.forbid_read_prefixes);
	}*/
	/// TODO self is parent
	/// TODO should optimize by runing all iterations concurrently.
	/// Diff is against a following log: so check read capability allowed.
	/// Write check is not needed here (parent allows all for extrinsic)
	pub fn diff(&self, logger: &StateLog, child_trie: Option<&Vec<u8>>, read_all: bool) -> Conflicts {
		let mut conflicts = Vec::new();
		if read_all {
			for key in self.forbid_read.iter() {
				conflicts.push(Conflict {
					child_trie: child_trie.cloned(),
					parent: ConflictOrigin::WriteKey(key.clone()),
					child: ConflictOrigin::ReadAll,
				});
			}
			for prefix in self.forbid_read_prefixes.iter() {
				conflicts.push(Conflict {
					child_trie: child_trie.cloned(),
					parent: ConflictOrigin::WritePrefix(prefix.clone()),
					child: ConflictOrigin::ReadAll,
				});
			}
		}
		for key in logger.read_keys.iter() {
			if self.forbid_read.contains(key) {
				conflicts.push(Conflict {
					child_trie: child_trie.cloned(),
					parent: ConflictOrigin::WriteKey(key.clone()),
					child: ConflictOrigin::ReadKey(key.clone()),
				});
			}
		}
		// checking against wite prefix is done in an inefficent reverse way
		let log_read: BTreeSet<_> = logger.read_keys.iter().cloned().collect();
		for prefix in self.forbid_read_prefixes.iter() {
			let (end, start) = (end_prefix(prefix.as_slice()), prefix);
			let mut iter = log_read.range::<Vec<u8>, _>(start..);
			while let Some(next) = iter.next() {
				if end.as_ref().map(|end| next < end).unwrap_or(true) {
					conflicts.push(Conflict {
						child_trie: child_trie.cloned(),
						parent: ConflictOrigin::WritePrefix(prefix.clone()),
						child: ConflictOrigin::ReadKey(next.clone()),
					})
				} else {
					break
				}
			}
		}
		// inneficient range check.
		let mut iter_prefix = self.forbid_read_prefixes.iter().peekable();
		for (start, end) in logger.read_intervals.iter() {
			let mut iter = self.forbid_read.range::<Vec<u8>, _>(start..);
			while let Some(next) = iter.next() {
				if end.as_ref().map(|end| next <= end).unwrap_or(true) {
					conflicts.push(Conflict {
						child_trie: child_trie.cloned(),
						parent: ConflictOrigin::WriteKey(next.clone()),
						child: ConflictOrigin::ReadInterval((start.clone(), end.clone())),
					})
				} else {
					break
				}
			}
			// assume iter range to never overlap (should not as long
			// as logger do not merge intervals).
			// TODO this part need test
			while let Some(prefix) = iter_prefix.peek() {
				let (write_end, write_start) = (end_prefix(prefix.as_slice()), prefix);
				if write_end.as_ref().map(|write_end| start >= write_end).unwrap_or(false) {
					let _ = iter_prefix.next();
					continue;
				}
				// read end > start
				if end.as_ref().map(|end| write_start <= &end).unwrap_or(true) {
					conflicts.push(Conflict {
						child_trie: child_trie.cloned(),
						parent: ConflictOrigin::WritePrefix(prefix.to_vec()),
						child: ConflictOrigin::ReadInterval((start.clone(), end.clone())),
					});
					if end.is_none() || (write_end.is_some() && &write_end <= end) {
						// can have other prefix in interval 
						let _ = iter_prefix.next();
						continue;
					}
					break;
				} else {
					break;
				}
			}
		}
		Conflicts(conflicts)
	}
}

/// TODO
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "std", derive(serde::Serialize, serde::Deserialize))]
pub struct Conflicts(Vec<Conflict>);

impl Conflicts {
	/// TODO
	pub fn accumulate(&mut self, other: Self) {
		self.0.extend(other.0);
	}
}

/// TODO
#[derive(Debug, Clone, Eq, PartialEq)]
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
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "std", derive(serde::Serialize, serde::Deserialize))]
pub enum ConflictOrigin {
	ReadAll,
	ReadKey(Vec<u8>),
	ReadInterval((Vec<u8>, Option<Vec<u8>>)),
	WriteKey(Vec<u8>),
	WritePrefix(Vec<u8>),
}

/// Try to group extrinsic that can run in parallel.
///
/// # strat 1
///
/// Strategy for parallel runing of consecutive chunk of code (extrinsics):
/// A -> B -> C
///
/// With this order we must ensure by our declaration that reads in N+1 do touch something
/// written in N.
/// Write A disjoint with Read B.
/// Write A disjoint with Read C.
/// Write B disjoint with Read C.
///
/// A B and C are spawned consecutely and join in same order.
/// Parent apply read 0 access all along, but an aggregated one is built.
///
/// Then we apply rules (check is precondition, force means we can modify current rules):
/// - spawn A:
///		- check A write decl included in parent write access
///		- check A read decl included in parent read 0 access
///		- check/force A write decl disjoint with parent read 1 capability.
/// - spawn B:
///		- check B write decl included in parent write access
///		- check B read decl included in parent read 1 access
///		- check/force B write decl disjoint with parent read 2 capability.
///
/// Write A disjoint with parent read 1.
/// Read B included in parent read 1.
/// So Write A disjoint with Read B.
///
/// - spawn C:
///		- check C write decl included in parent write access
///		- check C read decl included in parent read 2 access
///		- check/force C write decl disjoint with parent read 3 capability.
///
/// Write B disjoint with parent read 2.
/// Read C included in parent read 2.
/// So Write B disjoint with Read C.
///
/// Write A disjoint with parent read 1.
/// parent read 2 include in parent read 1.
/// So Write A disjoint with parent read 2.
/// Read C included in parent read 2.
/// So Write A disjoint with Read C.
/// 
/// By recurrence, ok for more code part.
///
///
/// So rule is:
///  - check child declaration r/w included in parent declaration r/w
///  - then restrict parent read declaration with child write declaration
///
/// # strat 2
///
/// Strategy for parallel runing of consecutive chunk of code (extrinsics), and
/// using parent code for the first one (so writes on join erase overide it).
/// A -> B -> C
///
/// A is parent, then spawn B then spawn C, run A, join B, join C.
/// This is same as strat 1 ordering with first in parent.
///
/// Again to check (B write disjoint with C already checked):
/// A write disjoint with Read B.
/// A write disjoint with Read C.
///
/// For this we just need to:
///		- check/force B write decl disjoint with parent read 1 capability.
///
///	# grouping algo:
///
///	Just apply rules until conflict between child read and parent read.
///	If A is parent, after each include check read against parent read: if fail do not include.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "std", derive(serde::Serialize, serde::Deserialize))]
pub struct BuildGroups {
	/// all running processes, by order of execution.
	state_logs: Vec<AccessLog>,
	/// Number of allows conflicts, should be 0, but can
	/// be good to allow a few and analyze if code could change.
	conflict_threshold: usize,
	/// Keep trace of multiple best solution. TODO unimplemented
	/// This avoid n2 processing, but also makes solution suboptimal.
	nb_solution: usize,
	/// Pending best groups (solutions).
	solutions: BuildSolution,
}

/// We always tries to minimizie parent Vec size and maximize inner vec.
/// content is index of state log.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "std", derive(serde::Serialize, serde::Deserialize))]
struct BuildSolution(Vec<BuildGroup>);

#[derive(Debug, Clone)]
#[cfg_attr(feature = "std", derive(serde::Serialize, serde::Deserialize))]
struct BuildGroup {
	content: Vec<usize>,
	conflicts: Vec<Conflicts>,
	nb_conflicts: usize,
	#[cfg_attr(feature = "std", serde(skip_serializing))]
	accum: AccumLogger,
}

impl BuildGroups {
	/// TODO
	pub fn new(conflict_threshold: usize, nb_solution: usize) -> Self {
		BuildGroups {
			state_logs: Default::default(),
			conflict_threshold,
			nb_solution,
			solutions: BuildSolution(vec![]),
		}
	}

	/// TODO
	pub fn clear(&mut self) {
		self.state_logs.clear();
		self.solutions.0.clear();
	}

	/// Add following code to try to paralellize.
	pub fn add_next(&mut self, log: AccessLog) {
		let index = self.state_logs.len();
		self.state_logs.push(log);
		let log = self.state_logs.last().expect("pushed above");
		let mut added = false;
		if let Some(group) = self.solutions.0.last_mut() {
			let new_conflicts = group.accum.diff(log);
			if new_conflicts.0.len() + group.nb_conflicts < self.conflict_threshold {
				group.content.push(index);
				group.nb_conflicts += new_conflicts.0.len();
				group.conflicts.push(new_conflicts);
				group.accum.accumulate_logger(log.clone());
				added = true;
			}
		}
		if !added {
			self.solutions.0.push(BuildGroup {
				content: vec![index],
				conflicts: vec![Conflicts(vec![])],
				nb_conflicts: 0,
				accum: log.clone().into(),
			})
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_prev_next() {
		// test between child read interval and parent prefix writes
		let parent: AccumStateLogger = StateLog {
			read_keys: vec![],
			write_keys: vec![],
			write_prefix: vec![
				b"b".to_vec(),
				b"h".to_vec(),
				b"o".to_vec(),
			],
			
			read_intervals: vec![],
		}.into();
		let child = StateLog {
			read_keys: vec![],
			write_keys: vec![],
			write_prefix: vec![],
			read_intervals: vec![
				(b"".to_vec(), b"abc".to_vec().into()),
				(b"abc".to_vec(), b"de".to_vec().into()),
				(b"fzh".to_vec(), b"fzhzzzzz".to_vec().into()),
				(b"ha".to_vec(), b"hb".to_vec().into()),
				(b"hb".to_vec(), b"hc".to_vec().into()),
				(b"hz".to_vec(), b"i".to_vec().into()),
				(b"m".to_vec(), b"z".to_vec().into()),
			],
		};

		// TODO debug test broken, missing first only from diff
		assert_eq!(parent.diff(&child, None, false), Conflicts(vec![
			Conflict {
				child_trie: None,
				parent: ConflictOrigin::WritePrefix(b"b".to_vec()),
				child: ConflictOrigin::ReadInterval((b"abc".to_vec(), b"de".to_vec().into())),
			},
			Conflict {
				child_trie: None,
				parent: ConflictOrigin::WritePrefix(b"h".to_vec()),
				child: ConflictOrigin::ReadInterval((b"ha".to_vec(), b"hb".to_vec().into())),
			},
			Conflict {
				child_trie: None,
				parent: ConflictOrigin::WritePrefix(b"h".to_vec()),
				child: ConflictOrigin::ReadInterval((b"hb".to_vec(), b"hc".to_vec().into())),
			},
			Conflict {
				child_trie: None,
				parent: ConflictOrigin::WritePrefix(b"h".to_vec()),
				child: ConflictOrigin::ReadInterval((b"hz".to_vec(), b"i".to_vec().into())),
			},
			Conflict {
				child_trie: None,
				parent: ConflictOrigin::WritePrefix(b"o".to_vec()),
				child: ConflictOrigin::ReadInterval((b"m".to_vec(), b"z".to_vec().into())),
			},
		]));

		let parent: AccumStateLogger = StateLog {
			read_keys: vec![],
			write_keys: vec![],
			write_prefix: vec![
				b"b".to_vec(),
				b"m".to_vec(),
				b"no".to_vec(),
			],
			read_intervals: vec![],
		}.into();
		let child = StateLog {
			read_keys: vec![],
			write_keys: vec![],
			write_prefix: vec![],
			read_intervals: vec![
				(b"m".to_vec(), None),
			],
		};

		assert_eq!(parent.diff(&child, None, false), Conflicts(vec![
			Conflict {
				child_trie: None,
				parent: ConflictOrigin::WritePrefix(b"m".to_vec()),
				child: ConflictOrigin::ReadInterval((b"m".to_vec(), None)),
			},
			Conflict {
				child_trie: None,
				parent: ConflictOrigin::WritePrefix(b"no".to_vec()),
				child: ConflictOrigin::ReadInterval((b"m".to_vec(), None)),
			},
		]));

		let parent: AccumStateLogger = StateLog {
			read_keys: vec![],
			write_keys: vec![
				b"b".to_vec(),
				b"h".to_vec(),
				b"o".to_vec(),
			],
			write_prefix: vec![
				b"i".to_vec(),
			],
			read_intervals: vec![],
		}.into();
		let child = StateLog {
			write_keys: vec![],
			write_prefix: vec![],
			read_intervals: vec![],
			read_keys: vec![
				b"".to_vec(),
				b"b".to_vec(),
				b"ba".to_vec(),
				b"ia".to_vec(),
			],
		};

		// TODO debug test broken, missing first only from diff
		assert_eq!(parent.diff(&child, None, false), Conflicts(vec![
			Conflict {
				child_trie: None,
				parent: ConflictOrigin::WriteKey(b"b".to_vec()),
				child: ConflictOrigin::ReadKey(b"b".to_vec()),
			},
			Conflict {
				child_trie: None,
				parent: ConflictOrigin::WritePrefix(b"i".to_vec()),
				child: ConflictOrigin::ReadKey(b"ia".to_vec()),
			},
		]));
	}
}
