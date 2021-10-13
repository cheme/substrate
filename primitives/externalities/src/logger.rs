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
	write_keys: BTreeSet<Vec<u8>>,
	write_prefixes: BTreeSet<Vec<u8>>,
	read_all: bool,
	read_keys: BTreeSet<Vec<u8>>,
	read_intervals: BTreeSet<(Vec<u8>, Option<Vec<u8>>)>,
}
