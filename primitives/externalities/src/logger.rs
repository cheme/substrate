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
	/// TODO should optimize by runing all iterations concurrently.
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
		}
		// checking against wite prefix is done in an inefficent reverse way
		let log_read: BTreeSet<_> = logger.read_keys.iter().cloned().collect();
		for prefix in self.write_prefixes.iter() {
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
		let mut iter_prefix = self.write_prefixes.iter().peekable();
		for (start, end) in logger.read_intervals.iter() {
			let mut iter = self.write_keys.range::<Vec<u8>, _>(start..);
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
			}
			// checking against read interval is done in a reverse way
			// to avoid using special interval storage, but it is
			// inefficient
			for (start, end) in self.read_intervals.iter() {
				let log_write: BTreeSet<_> = logger.write_keys.iter().cloned().collect();
				let mut iter = log_write.range::<Vec<u8>, _>(start..);
				while let Some(next) = iter.next() {
					if end.as_ref().map(|end| next <= end).unwrap_or(true) {
						conflicts.push(Conflict {
							child_trie: child_trie.cloned(),
							parent: ConflictOrigin::ReadInterval((start.clone(), end.clone())),
							child: ConflictOrigin::WriteKey(next.clone()),
						})
					} else {
						break
					}
				}
			}

			// inneficient range check.
			let mut iter_range = self.read_intervals.iter().peekable();
			for prefix in logger.write_prefix.iter() {
				let (end, start) = (end_prefix(prefix.as_slice()), prefix);
				let mut iter = self.read_keys.range::<Vec<u8>, _>(start..);
				while let Some(next) = iter.next() {
					if end.as_ref().map(|end| next < end).unwrap_or(true) {
						conflicts.push(Conflict {
							child_trie: child_trie.cloned(),
							parent: ConflictOrigin::ReadKey(next.clone()),
							child: ConflictOrigin::WritePrefix(prefix.clone()),
						})
					} else {
						break
					}
				}
				// assume iter range to never overlap (should not as long
				// as logger do not merge intervals).
				// TODO this part need test
				while let Some((read_start, read_end)) = iter_range.peek() {
					if read_end.as_ref().map(|read_end| start > read_end).unwrap_or(false) {
						let _ = iter_range.next();
						continue;
					}
					// read end > start
					if end.as_ref().map(|end| read_start < end).unwrap_or(true) {
						conflicts.push(Conflict {
							child_trie: child_trie.cloned(),
							parent: ConflictOrigin::ReadInterval((read_start.clone(), read_end.clone())),
							child: ConflictOrigin::WritePrefix(prefix.clone()),
						});
						if end.is_none() || (read_end.is_some() && read_end < &end) {
							// can have other interval in prefix
							let _ = iter_range.next();
							continue;
						}
						break;
					} else {
						break;
					}
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_interval_prefix() {
		// test between parent read interval and child prefix writes
		let parent: AccumStateLogger = StateLog {
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
		}.into();
		let child = StateLog {
			read_keys: vec![],
			write_keys: vec![],
			write_prefix: vec![
				b"b".to_vec(),
				b"h".to_vec(),
				b"o".to_vec(),
			],
			read_intervals: vec![],
		};

		// TODO debug test broken, missing first only from diff
		assert_eq!(parent.diff(&child, None, false), Conflicts(vec![
			Conflict {
				child_trie: None,
				parent: ConflictOrigin::ReadInterval((b"abc".to_vec(), b"de".to_vec().into())),
				child: ConflictOrigin::WritePrefix(b"b".to_vec()),
			},
			Conflict {
				child_trie: None,
				parent: ConflictOrigin::ReadInterval((b"ha".to_vec(), b"hb".to_vec().into())),
				child: ConflictOrigin::WritePrefix(b"h".to_vec()),
			},
			Conflict {
				child_trie: None,
				parent: ConflictOrigin::ReadInterval((b"hb".to_vec(), b"hc".to_vec().into())),
				child: ConflictOrigin::WritePrefix(b"h".to_vec()),
			},
			Conflict {
				child_trie: None,
				parent: ConflictOrigin::ReadInterval((b"hz".to_vec(), b"i".to_vec().into())),
				child: ConflictOrigin::WritePrefix(b"h".to_vec()),
			},
			Conflict {
				child_trie: None,
				parent: ConflictOrigin::ReadInterval((b"m".to_vec(), b"z".to_vec().into())),
				child: ConflictOrigin::WritePrefix(b"o".to_vec()),
			},
		]));

		let parent: AccumStateLogger = StateLog {
			read_keys: vec![],
			write_keys: vec![],
			write_prefix: vec![],
			read_intervals: vec![
				(b"m".to_vec(), None),
			],
		}.into();
		let child = StateLog {
			read_keys: vec![],
			write_keys: vec![],
			write_prefix: vec![
				b"b".to_vec(),
				b"m".to_vec(),
				b"no".to_vec(),
			],
			read_intervals: vec![],
		};

		assert_eq!(parent.diff(&child, None, false), Conflicts(vec![
			Conflict {
				child_trie: None,
				parent: ConflictOrigin::ReadInterval((b"m".to_vec(), None)),
				child: ConflictOrigin::WritePrefix(b"m".to_vec()),
			},
			Conflict {
				child_trie: None,
				parent: ConflictOrigin::ReadInterval((b"m".to_vec(), None)),
				child: ConflictOrigin::WritePrefix(b"no".to_vec()),
			},
		]));
	}

	#[test]
	fn test_prefix_interval() {
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
	}

}
