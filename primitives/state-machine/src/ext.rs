// This file is part of Substrate.

// Copyright (C) 2017-2022 Parity Technologies (UK) Ltd.
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

//! Concrete externalities implementation.

#[cfg(feature = "std")]
use crate::overlayed_changes::OverlayedExtensions;
use crate::{backend::Backend, IndexOperation, OverlayedChanges, StorageKey, StorageValue};
use codec::{Decode, Encode, EncodeAppend};
use hash_db::Hasher;
#[cfg(feature = "std")]
use sp_core::hexdisplay::HexDisplay;
use sp_core::{
	storage::{
		transient::{Hash32Algorithm, Root32Structure},
		well_known_keys::is_child_storage_key,
		ChildInfo, StateVersion, TrackedStorageKey,
	},
	Hashers,
};
use sp_externalities::{
	result_from_cow, result_from_slice, result_from_vec, Extension, ExtensionStore, Externalities,
	MultiRemovalResults,
};
use sp_trie::{empty_child_trie_root, LayoutV1};

use crate::{log_error, trace, warn, StorageTransactionCache};
use sp_std::{
	any::{Any, TypeId},
	boxed::Box,
	cmp::Ordering,
	vec,
	vec::Vec,
};
#[cfg(feature = "std")]
use std::error;

const EXT_NOT_ALLOWED_TO_FAIL: &str = "Externalities not allowed to fail within runtime";
const BENCHMARKING_FN: &str = "\
	This is a special fn only for benchmarking where a database commit happens from the runtime.
	For that reason client started transactions before calling into runtime are not allowed.
	Without client transactions the loop condition garantuees the success of the tx close.";

#[cfg(feature = "std")]
fn guard() -> sp_panic_handler::AbortGuard {
	sp_panic_handler::AbortGuard::force_abort()
}

#[cfg(not(feature = "std"))]
fn guard() -> () {
	()
}

/// Errors that can occur when interacting with the externalities.
#[cfg(feature = "std")]
#[derive(Debug, Copy, Clone)]
pub enum Error<B, E> {
	/// Failure to load state data from the backend.
	#[allow(unused)]
	Backend(B),
	/// Failure to execute a function.
	#[allow(unused)]
	Executor(E),
}

#[cfg(feature = "std")]
impl<B: std::fmt::Display, E: std::fmt::Display> std::fmt::Display for Error<B, E> {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
		match *self {
			Error::Backend(ref e) => write!(f, "Storage backend error: {}", e),
			Error::Executor(ref e) => write!(f, "Sub-call execution error: {}", e),
		}
	}
}

#[cfg(feature = "std")]
impl<B: error::Error, E: error::Error> error::Error for Error<B, E> {
	fn description(&self) -> &str {
		match *self {
			Error::Backend(..) => "backend error",
			Error::Executor(..) => "executor error",
		}
	}
}

/// Wraps a read-only backend, call executor, and current overlayed changes.
pub struct Ext<'a, H, B>
where
	H: Hashers,
	B: 'a + Backend<H>,
{
	/// The overlayed changes to write to.
	overlay: &'a mut OverlayedChanges,
	/// The storage backend to read from.
	backend: &'a B,
	/// The cache for the storage transactions.
	storage_transaction_cache: &'a mut StorageTransactionCache<B::Transaction, H>,
	/// Pseudo-unique id used for tracing.
	pub id: u16,
	/// Extensions registered with this instance.
	#[cfg(feature = "std")]
	extensions: Option<OverlayedExtensions<'a>>,
}

impl<'a, H, B> Ext<'a, H, B>
where
	H: Hashers,
	B: Backend<H>,
{
	/// Create a new `Ext`.
	#[cfg(not(feature = "std"))]
	pub fn new(
		overlay: &'a mut OverlayedChanges,
		storage_transaction_cache: &'a mut StorageTransactionCache<B::Transaction, H>,
		backend: &'a B,
	) -> Self {
		Ext { overlay, backend, id: 0, storage_transaction_cache }
	}

	/// Create a new `Ext` from overlayed changes and read-only backend
	#[cfg(feature = "std")]
	pub fn new(
		overlay: &'a mut OverlayedChanges,
		storage_transaction_cache: &'a mut StorageTransactionCache<B::Transaction, H>,
		backend: &'a B,
		extensions: Option<&'a mut sp_externalities::Extensions>,
	) -> Self {
		Self {
			overlay,
			backend,
			storage_transaction_cache,
			id: rand::random(),
			extensions: extensions.map(OverlayedExtensions::new),
		}
	}

	/// Invalidates the currently cached storage root and the db transaction.
	///
	/// Called when there are changes that likely will invalidate the storage root.
	fn mark_dirty(&mut self) {
		self.storage_transaction_cache.reset();
	}
}

#[cfg(test)]
impl<'a, H, B> Ext<'a, H, B>
where
	H: Hashers,
	H::Out: Ord + 'static,
	B: 'a + Backend<H>,
{
	pub fn storage_pairs(&self) -> Vec<(StorageKey, StorageValue)> {
		use std::collections::HashMap;

		self.backend
			.pairs()
			.iter()
			.map(|&(ref k, ref v)| (k.to_vec(), Some(v.to_vec())))
			.chain(self.overlay.changes().map(|(k, v)| (k.clone(), v.value().cloned())))
			.collect::<HashMap<_, _>>()
			.into_iter()
			.filter_map(|(k, maybe_val)| maybe_val.map(|val| (k, val)))
			.collect()
	}
}

impl<'a, H, B> Externalities for Ext<'a, H, B>
where
	H: Hashers,
	H::Out: Ord + 'static + codec::Codec,
	B: Backend<H>,
{
	fn set_offchain_storage(&mut self, key: &[u8], value: Option<&[u8]>) {
		self.overlay.set_offchain_storage(key, value)
	}

	fn storage(&self, key: &[u8], start: u32, limit: Option<u32>) -> Option<StorageValue> {
		let _guard = guard();
		let result = self
			.overlay
			.storage(key)
			.map(|x| result_from_slice(x, start, limit))
			.unwrap_or_else(|| {
				result_from_vec(
					self.backend.storage(key).expect(EXT_NOT_ALLOWED_TO_FAIL),
					start,
					limit,
				)
			});

		// NOTE: be careful about touching the key names – used outside substrate!
		trace!(
			target: "state",
			method = "Get",
			ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
			key = %HexDisplay::from(&key),
			result = ?result.as_ref().map(HexDisplay::from),
			result_encoded = %HexDisplay::from(
				&result
					.as_ref()
					.map(|v| EncodeOpaqueValue(v.clone()))
					.encode()
			),
		);

		result
	}

	fn storage_hash(&self, key: &[u8]) -> Option<Vec<u8>> {
		let _guard = guard();
		let result = self
			.overlay
			.storage(key)
			.map(|x| x.map(|x| H::hash(x)))
			.unwrap_or_else(|| self.backend.storage_hash(key).expect(EXT_NOT_ALLOWED_TO_FAIL));

		trace!(
			target: "state",
			method = "Hash",
			ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
			key = %HexDisplay::from(&key),
			?result,
		);
		result.map(|r| r.encode())
	}

	fn child_storage(
		&self,
		child_info: &ChildInfo,
		key: &[u8],
		start: u32,
		limit: Option<u32>,
	) -> Option<StorageValue> {
		let _guard = guard();
		let result = self
			.overlay
			.child_storage(child_info, key, start, limit)
			.map(|x| x.map(|x| x.to_vec()))
			.unwrap_or_else(|| {
				result_from_vec(
					self.backend.child_storage(child_info, key).expect(EXT_NOT_ALLOWED_TO_FAIL),
					start,
					limit,
				)
			});

		trace!(
			target: "state",
			method = "ChildGet",
			ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
			child_info = %HexDisplay::from(&child_info.storage_key()),
			key = %HexDisplay::from(&key),
			result = ?result.as_ref().map(HexDisplay::from)
		);

		result
	}

	fn child_value_size(&self, child_info: &ChildInfo, key: &[u8]) -> Option<u32> {
		let _guard = guard();
		let result = self.overlay.child_storage_len(child_info, key).unwrap_or_else(|| {
			self.backend
				.child_storage(child_info, key)
				.expect(EXT_NOT_ALLOWED_TO_FAIL)
				.map(|v| v.len() as u32)
		});

		trace!(
			target: "state",
			method = "ChildGetLen",
			ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
			child_info = %HexDisplay::from(&child_info.storage_key()),
			key = %HexDisplay::from(&key),
			result = result,
		);

		result
	}

	fn child_storage_hash(&self, child_info: &ChildInfo, key: &[u8]) -> Option<Vec<u8>> {
		let _guard = guard();
		let result = self
			.overlay
			.child_storage(child_info, key, 0, None)
			.map(|x| x.map(|x| H::hash(x.as_ref())))
			.unwrap_or_else(|| {
				self.backend.child_storage_hash(child_info, key).expect(EXT_NOT_ALLOWED_TO_FAIL)
			});

		trace!(
			target: "state",
			method = "ChildHash",
			ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
			child_info = %HexDisplay::from(&child_info.storage_key()),
			key = %HexDisplay::from(&key),
			?result,
		);

		result.map(|r| r.encode())
	}

	fn exists_storage(&self, key: &[u8]) -> bool {
		let _guard = guard();
		let result = match self.overlay.storage(key) {
			Some(x) => x.is_some(),
			_ => self.backend.exists_storage(key).expect(EXT_NOT_ALLOWED_TO_FAIL),
		};

		trace!(
			target: "state",
			method = "Exists",
			ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
			key = %HexDisplay::from(&key),
			%result,
		);

		result
	}

	fn exists_child_storage(&self, child_info: &ChildInfo, key: &[u8]) -> bool {
		let _guard = guard();

		let result = match self.overlay.child_storage(child_info, key, 0, Some(0)) {
			Some(x) => x.is_some(),
			_ => self
				.backend
				.exists_child_storage(child_info, key)
				.expect(EXT_NOT_ALLOWED_TO_FAIL),
		};

		trace!(
			target: "state",
			method = "ChildExists",
			ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
			child_info = %HexDisplay::from(&child_info.storage_key()),
			key = %HexDisplay::from(&key),
			%result,
		);
		result
	}

	fn next_storage_key(&self, key: &[u8]) -> Option<StorageKey> {
		let mut next_backend_key =
			self.backend.next_storage_key(key).expect(EXT_NOT_ALLOWED_TO_FAIL);
		let mut overlay_changes = self.overlay.iter_after(key).peekable();

		match (&next_backend_key, overlay_changes.peek()) {
			(_, None) => next_backend_key,
			(Some(_), Some(_)) => {
				for overlay_key in overlay_changes {
					let cmp = next_backend_key.as_deref().map(|v| v.cmp(overlay_key.0));

					// If `backend_key` is less than the `overlay_key`, we found out next key.
					if cmp == Some(Ordering::Less) {
						return next_backend_key
					} else if overlay_key.1.value().is_some() {
						// If there exists a value for the `overlay_key` in the overlay
						// (aka the key is still valid), it means we have found our next key.
						return Some(overlay_key.0.to_vec())
					} else if cmp == Some(Ordering::Equal) {
						// If the `backend_key` and `overlay_key` are equal, it means that we need
						// to search for the next backend key, because the overlay has overwritten
						// this key.
						next_backend_key = self
							.backend
							.next_storage_key(overlay_key.0)
							.expect(EXT_NOT_ALLOWED_TO_FAIL);
					}
				}

				next_backend_key
			},
			(None, Some(_)) => {
				// Find the next overlay key that has a value attached.
				overlay_changes.find_map(|k| k.1.value().as_ref().map(|_| k.0.to_vec()))
			},
		}
	}

	fn next_child_storage_key(
		&self,
		child_info: &ChildInfo,
		key: &[u8],
		count: u32,
	) -> Option<Vec<StorageKey>> {
		match child_info {
			ChildInfo::BTree(_) => {
				let iter = self.overlay.child_iter_after(child_info, key)?;

				let mut result = Vec::with_capacity(count as usize);
				for (k, v) in iter {
					if v.value().is_some() {
						result.push(k.to_vec());
						if result.len() == count as usize {
							break
						}
					}
				}
				return Some(result)
			},
			ChildInfo::Blob(_) => return None,
			ChildInfo::ParentKeyId(_) => (),
		}

		let mut overlay_changes = self.overlay.child_iter_after(child_info, key)?;

		let mut result = Vec::<StorageKey>::with_capacity(count as usize);
		let mut next_overlay = overlay_changes.next();
		loop {
			if result.len() == count as usize {
				break
			}

			let mut next_backend_key = self
				.backend
				.next_child_storage_key(
					child_info,
					result.last().map(|r| r.as_slice()).unwrap_or(key),
				)
				.expect(EXT_NOT_ALLOWED_TO_FAIL);

			// TODO recheck this diff
			let next = match (&next_backend_key, &next_overlay) {
				(_, None) => next_backend_key,
				(Some(_), Some(_)) => {
					// TODO this is incorrect  as overlay change bigger  we will call next on next
					// value...
					while let Some(overlay_key) = next_overlay.as_ref() {
						let cmp = next_backend_key.as_deref().map(|v| v.cmp(overlay_key.0));

						if cmp == Some(Ordering::Less) {
							// If `backend_key` is less than the `overlay_key`, we found out next
							// key.
							break
						} else if overlay_key.1.value().is_some() {
							// If there exists a value for the `overlay_key` in the overlay
							// (aka the key is still valid), it means we have found our next key.
							result.push(overlay_key.0.to_vec());
							if result.len() == count as usize {
								return Some(result)
							}
						} else if cmp == Some(Ordering::Equal) {
							// If the `backend_key` and `overlay_key` are equal, it means that we
							// need to search for the next backend key, because the overlay has
							// overwritten this key.
							next_backend_key = self
								.backend
								.next_child_storage_key(child_info, overlay_key.0)
								.expect(EXT_NOT_ALLOWED_TO_FAIL);
						}
						next_overlay = overlay_changes.next();
					}

					next_backend_key
				},
				(None, Some(_)) => {
					// Find the next overlay key that has a value attached.
					overlay_changes.find_map(|k| k.1.value().as_ref().map(|_| k.0.to_vec()))
				},
			};
			if let Some(key) = next {
				result.push(key);
			} else {
				break
			}
		}
		Some(result)
	}

	fn place_storage(&mut self, key: StorageKey, value: Option<StorageValue>) {
		let _guard = guard();
		if is_child_storage_key(&key) {
			warn!(target: "trie", "Refuse to directly set child storage key");
			return
		}

		// NOTE: be careful about touching the key names – used outside substrate!
		trace!(
			target: "state",
			method = "Put",
			ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
			key = %HexDisplay::from(&key),
			value = ?value.as_ref().map(HexDisplay::from),
			value_encoded = %HexDisplay::from(
				&value
					.as_ref()
					.map(|v| EncodeOpaqueValue(v.clone()))
					.encode()
			),
		);

		self.mark_dirty();
		self.overlay.set_storage(key, value);
	}

	fn push_storage(&mut self, child_info: &ChildInfo, value: StorageValue) {
		let _guard = guard();
		trace!(
			target: "state",
			method = "ChildPush",
			ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
			child_info = %HexDisplay::from(&child_info.storage_key()),
			value = %HexDisplay::from(&value),
		);

		self.mark_dirty();
		let fetch_size_mmr = || {
			let storage_key = child_info.storage_key();
			if let Ok(Some(encoded)) = self.backend.storage(storage_key) {
				let mut buf = [0u8; 8];
				if encoded.len() < 8 {
					0
				} else {
					buf.copy_from_slice(&encoded[..8]);
					u64::from_le_bytes(buf)
				}
			} else {
				0
			}
		};
		self.overlay.push_storage(child_info, value, fetch_size_mmr);
	}

	fn place_child_storage(
		&mut self,
		child_info: &ChildInfo,
		key: &[u8],
		value: Option<&[u8]>,
	) -> bool {
		trace!(
			target: "state",
			method = "ChildPut",
			ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
			child_info = %HexDisplay::from(&child_info.storage_key()),
			key = %HexDisplay::from(&key),
			value = ?value.as_ref().map(HexDisplay::from),
		);
		let _guard = guard();

		self.mark_dirty();
		self.overlay.set_child_storage(child_info, key, value)
	}

	fn kill_child_storage(
		&mut self,
		child_info: &ChildInfo,
		maybe_limit: Option<u32>,
		maybe_cursor: Option<&[u8]>,
	) -> MultiRemovalResults {
		trace!(
			target: "state",
			method = "ChildKill",
			ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
			child_info = %HexDisplay::from(&child_info.storage_key()),
		);
		let _guard = guard();
		self.mark_dirty();
		let overlay = self.overlay.clear_child_storage(child_info);
		let (maybe_cursor, backend, loops) =
			self.limit_remove_from_backend(Some(child_info), None, maybe_limit, maybe_cursor);
		MultiRemovalResults { maybe_cursor, backend, unique: overlay + backend, loops }
	}

	fn clear_prefix(
		&mut self,
		prefix: &[u8],
		maybe_limit: Option<u32>,
		maybe_cursor: Option<&[u8]>,
	) -> MultiRemovalResults {
		trace!(
			target: "state",
			method = "ClearPrefix",
			ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
			prefix = %HexDisplay::from(&prefix),
		);
		let _guard = guard();

		if sp_core::storage::well_known_keys::starts_with_child_storage_key(prefix) {
			warn!(
				target: "trie",
				"Refuse to directly clear prefix that is part or contains of child storage key",
			);
			return MultiRemovalResults { maybe_cursor: None, backend: 0, unique: 0, loops: 0 }
		}

		self.mark_dirty();
		let overlay = self.overlay.clear_prefix(prefix);
		let (maybe_cursor, backend, loops) =
			self.limit_remove_from_backend(None, Some(prefix), maybe_limit, maybe_cursor);
		MultiRemovalResults { maybe_cursor, backend, unique: overlay + backend, loops }
	}

	fn clear_child_prefix(
		&mut self,
		child_info: &ChildInfo,
		prefix: &[u8],
		maybe_limit: Option<u32>,
		maybe_cursor: Option<&[u8]>,
	) -> MultiRemovalResults {
		trace!(
			target: "state",
			method = "ChildClearPrefix",
			ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
			child_info = %HexDisplay::from(&child_info.storage_key()),
			prefix = %HexDisplay::from(&prefix),
		);
		let _guard = guard();

		self.mark_dirty();
		let overlay = self.overlay.clear_child_prefix(child_info, prefix);
		let (maybe_cursor, backend, loops) = self.limit_remove_from_backend(
			Some(child_info),
			Some(prefix),
			maybe_limit,
			maybe_cursor,
		);
		MultiRemovalResults { maybe_cursor, backend, unique: overlay + backend, loops }
	}

	fn storage_append(&mut self, key: Vec<u8>, value: Vec<u8>) {
		trace!(
			target: "state",
			method = "Append",
			ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
			key = %HexDisplay::from(&key),
			value = %HexDisplay::from(&value),
		);

		let _guard = guard();
		self.mark_dirty();

		let backend = &mut self.backend;
		let current_value = self.overlay.value_mut_or_insert_with(&key, || {
			backend.storage(&key).expect(EXT_NOT_ALLOWED_TO_FAIL).unwrap_or_default()
		});
		StorageAppend::new(current_value).append(value);
	}

	fn storage_root(&mut self, state_version: StateVersion) -> Vec<u8> {
		let _guard = guard();
		if let Some(ref root) = self.storage_transaction_cache.transaction_storage_root {
			trace!(
				target: "state",
				method = "StorageRoot",
				ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
				storage_root = %HexDisplay::from(&root.as_ref()),
				cached = true,
			);
			return root.encode()
		}

		let root =
			self.overlay
				.storage_root(self.backend, self.storage_transaction_cache, state_version);
		trace!(
			target: "state",
			method = "StorageRoot",
			ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
			storage_root = %HexDisplay::from(&root.as_ref()),
			cached = false,
		);
		root.encode()
	}

	fn child_storage_root(
		&mut self,
		child_info: &ChildInfo,
		state_version: StateVersion,
	) -> Vec<u8> {
		let _guard = guard();
		let storage_key = child_info.storage_key();
		let prefixed_storage_key = child_info.prefixed_storage_key();

		if let ChildInfo::BTree(..) = child_info {
			let result = if let Some((data, info)) =
				self.overlay.btree_changes(child_info.storage_key())
			{
				if let Some(root) = info.root.as_ref() {
					// cached
					return root.encode()
				}
				match info.infos.structure {
					Some(Root32Structure::SubstrateDefault) => {
						let delta = data.into_iter().filter_map(|(k, v)| v.value().map(|v| (k, v)));
						sp_trie::child_trie_root::<LayoutV1<H>, _, _, _>(delta).encode()
					},
					None => Vec::new(),
				}
			} else {
				// TODO change function proto to return None?? we check before calling it but
				// still
				Vec::new()
			};

			self.overlay.cache_btree_root(child_info.storage_key(), result.clone());
			return result
		}

		if let ChildInfo::Blob(..) = child_info {
			// TODO root calc from overlay
			let root: Option<(Vec<u8>, bool)> = if let Some((changes, info)) =
				self.overlay.blob_changes(storage_key)
			{
				if let Some(hash) = info.hash {
					return hash.into()
				}
				// TODO maybe backend implement storage root directly (hash type shoudl be in info
				unimplemented!("hash of blob and cache it in overlay");
			//Some(self.backend.child_mmr_root(info, changes))
			} else {
				None
			};

			return if let Some((root, is_empty)) = root {
				trace!(
					target: "state",
					method = "ChildStorageRoot",
					ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
					child_info = %HexDisplay::from(&storage_key),
					storage_root = %HexDisplay::from(&root.as_ref()),
					cached = false,
				);

				root
			} else {
				// no blob initialized.
				unimplemented!(
					"TODO change to return None or do hash empty but this
					conflict with no blob"
				);
				/*trace!(
					target: "state",
					method = "ChildStorageRoot",
					ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
					child_info = %HexDisplay::from(&storage_key),
					storage_root = %HexDisplay::from(&root.1.as_ref()),
					cached = false,
				);*/
			}
		}

		if self.storage_transaction_cache.transaction_storage_root.is_some() {
			let root = self
				.storage(prefixed_storage_key.as_slice(), 0, None)
				.and_then(|k| Decode::decode(&mut &k[..]).ok())
				// V1 is equivalent to V0 on empty root.
				.unwrap_or_else(empty_child_trie_root::<LayoutV1<H>>);
			trace!(
				target: "state",
				method = "ChildStorageRoot",
				ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
				child_info = %HexDisplay::from(&storage_key),
				storage_root = %HexDisplay::from(&root.as_ref()),
				cached = true,
			);
			root.encode()
		} else {
			let root = if let Some((changes, info)) = self.overlay.child_changes(storage_key) {
				let delta = changes.map(|(k, v)| (k.as_ref(), v.value().map(AsRef::as_ref)));
				Some(self.backend.child_storage_root(info, delta, state_version))
			} else {
				None
			};

			if let Some((root, is_empty, _)) = root {
				let root = root.encode();
				// We store update in the overlay in order to be able to use
				// 'self.storage_transaction' cache. This is brittle as it rely on Ext only querying
				// the trie backend for storage root.
				// A better design would be to manage 'child_storage_transaction' in a
				// similar way as 'storage_transaction' but for each child trie.
				if is_empty {
					self.overlay.set_storage(prefixed_storage_key.into_inner(), None);
				} else {
					self.overlay.set_storage(prefixed_storage_key.into_inner(), Some(root.clone()));
				}

				trace!(
					target: "state",
					method = "ChildStorageRoot",
					ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
					child_info = %HexDisplay::from(&storage_key),
					storage_root = %HexDisplay::from(&root.as_ref()),
					cached = false,
				);

				root
			} else {
				// empty overlay
				let root = self
					.storage(prefixed_storage_key.as_slice(), 0, None)
					.and_then(|k| Decode::decode(&mut &k[..]).ok())
					// V1 is equivalent to V0 on empty root.
					.unwrap_or_else(empty_child_trie_root::<LayoutV1<H>>);

				trace!(
					target: "state",
					method = "ChildStorageRoot",
					ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
					child_info = %HexDisplay::from(&storage_key),
					storage_root = %HexDisplay::from(&root.as_ref()),
					cached = false,
				);

				root.encode()
			}
		}
	}

	fn storage_index_transaction(&mut self, index: u32, hash: &[u8], size: u32) {
		trace!(
			target: "state",
			method = "IndexTransaction",
			ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
			%index,
			tx_hash = %HexDisplay::from(&hash),
			%size,
		);

		self.overlay.add_transaction_index(IndexOperation::Insert {
			extrinsic: index,
			hash: hash.to_vec(),
			size,
		});
	}

	/// Renew existing piece of data storage.
	fn storage_renew_transaction_index(&mut self, index: u32, hash: &[u8]) {
		trace!(
			target: "state",
			method = "RenewTransactionIndex",
			ext_id = %HexDisplay::from(&self.id.to_le_bytes()),
			%index,
			tx_hash = %HexDisplay::from(&hash),
		);

		self.overlay
			.add_transaction_index(IndexOperation::Renew { extrinsic: index, hash: hash.to_vec() });
	}

	fn storage_start_transaction(&mut self) {
		self.overlay.start_transaction()
	}

	fn storage_rollback_transaction(&mut self) -> Result<(), ()> {
		self.mark_dirty();
		self.overlay.rollback_transaction().map_err(|_| ())
	}

	fn storage_commit_transaction(&mut self) -> Result<(), ()> {
		self.overlay.commit_transaction().map_err(|_| ())
	}

	fn wipe(&mut self) {
		for _ in 0..self.overlay.transaction_depth() {
			self.overlay.rollback_transaction().expect(BENCHMARKING_FN);
		}
		self.overlay
			.drain_storage_changes(
				self.backend,
				self.storage_transaction_cache,
				Default::default(), // using any state
			)
			.expect(EXT_NOT_ALLOWED_TO_FAIL);
		self.backend.wipe().expect(EXT_NOT_ALLOWED_TO_FAIL);
		self.mark_dirty();
		self.overlay
			.enter_runtime()
			.expect("We have reset the overlay above, so we can not be in the runtime; qed");
	}

	fn commit(&mut self) {
		// Bench always use latest state.
		let state_version = StateVersion::default();
		for _ in 0..self.overlay.transaction_depth() {
			self.overlay.commit_transaction().expect(BENCHMARKING_FN);
		}
		let changes = self
			.overlay
			.drain_storage_changes(self.backend, self.storage_transaction_cache, state_version)
			.expect(EXT_NOT_ALLOWED_TO_FAIL);
		self.backend
			.commit(
				changes.transaction_storage_root,
				changes.transaction,
				changes.main_storage_changes,
				changes.child_storage_changes,
			)
			.expect(EXT_NOT_ALLOWED_TO_FAIL);
		self.mark_dirty();
		self.overlay
			.enter_runtime()
			.expect("We have reset the overlay above, so we can not be in the runtime; qed");
	}

	fn read_write_count(&self) -> (u32, u32, u32, u32) {
		self.backend.read_write_count()
	}

	fn reset_read_write_count(&mut self) {
		self.backend.reset_read_write_count()
	}

	fn get_whitelist(&self) -> Vec<TrackedStorageKey> {
		self.backend.get_whitelist()
	}

	fn set_whitelist(&mut self, new: Vec<TrackedStorageKey>) {
		self.backend.set_whitelist(new)
	}

	fn proof_size(&self) -> Option<u32> {
		self.backend.proof_size()
	}

	fn get_read_and_written_keys(&self) -> Vec<(Vec<u8>, u32, u32, bool)> {
		self.backend.get_read_and_written_keys()
	}

	fn storage_exists(&self, info: &ChildInfo) -> bool {
		// At this point no storage requires to check backend.
		self.overlay.storage_exists(info)
	}

	fn update_storage_info(&mut self, info: ChildInfo, clear_existing: bool) -> bool {
		// At this point no storage requires to clear on backend.
		self.overlay.update_storage_info(info, clear_existing)
	}

	fn storage_clone(&mut self, info: &ChildInfo, target_name: &[u8]) -> bool {
		// At this point no supported storage requires to clone backend.
		self.overlay.storage_copy(info, target_name)
	}

	fn storage_move(&mut self, info: &ChildInfo, target_name: &[u8]) -> bool {
		// At this point no supported storage requires to clone backend.
		self.overlay.storage_move(info, target_name)
	}

	fn map_count(&self, name: &[u8]) -> Option<u32> {
		// At this point no supported storage requires to clone backend.
		self.overlay.map_count(name)
	}

	fn map_item_hash32(&self, name: &[u8], key: &[u8], algo: Hash32Algorithm) -> Option<[u8; 32]> {
		let info = sp_externalities::btree_info_from_name(name);
		self.overlay
			.child_storage(&info, key, 0, None)
			.flatten()
			.map(|v| H::hash_with(v.as_ref(), algo))
	}

	fn map_dump(&self, name: &[u8]) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
		self.overlay.btree_changes(name).map(|(data, _info)| {
			data.filter_map(|(k, v)| v.value_ref().as_ref().map(|v| (k.clone(), v.clone())))
				.collect()
		})
	}

	fn map_dump_hashed(
		&self,
		name: &[u8],
		algorithm: Hash32Algorithm,
	) -> Option<Vec<([u8; 32], [u8; 32])>> {
		self.overlay.btree_changes(name).map(|(data, _info)| {
			data.filter_map(|(k, v)| {
				v.value_ref()
					.as_ref()
					.map(|v| (H::hash_with(k, algorithm), H::hash_with(v.as_ref(), algorithm)))
			})
			.collect()
		})
	}

	fn blob_len(&self, name: &[u8]) -> Option<u32> {
		// At this point no supported storage requires to clone backend.
		self.overlay.blob_len(name)
	}

	fn blob_hash32(&mut self, name: &[u8], algorithm: Hash32Algorithm) -> Option<[u8; 32]> {
		let (nb_chunk, mut chunks) = self.overlay.blob_chunks(name)?;
		let mut state = if let Some(chunk) = chunks.next() {
			if nb_chunk == 1 {
				return Some(H::hash_with(&chunk[..], algorithm))
			}
			H::hash_state_with(chunk, algorithm)
		} else {
			return Some(H::hash_with(&[], algorithm))
		};
		for chunk in chunks {
			H::hash_update(&chunk[..], &mut state);
		}
		Some(H::hash_finalize(state))
	}

	fn blob_set(&mut self, name: &[u8], value: &[u8], offset: u32) -> bool {
		self.overlay.blob_set(name, value, offset)
	}

	fn blob_truncate(&mut self, name: &[u8], new_size: u32) -> bool {
		self.overlay.blob_truncate(name, new_size)
	}
}

impl<'a, H, B> Ext<'a, H, B>
where
	H: Hashers,
	H::Out: Ord + 'static + codec::Codec,
	B: Backend<H>,
{
	fn limit_remove_from_backend(
		&mut self,
		maybe_child: Option<&ChildInfo>,
		maybe_prefix: Option<&[u8]>,
		maybe_limit: Option<u32>,
		maybe_cursor: Option<&[u8]>,
	) -> (Option<Vec<u8>>, u32, u32) {
		let mut delete_count: u32 = 0;
		let mut loop_count: u32 = 0;
		let mut maybe_next_key = None;
		self.backend
			.apply_to_keys_while(maybe_child, maybe_prefix, maybe_cursor, |key| {
				if maybe_limit.map_or(false, |limit| loop_count == limit) {
					maybe_next_key = Some(key.to_vec());
					return false
				}
				let has_overlay = match maybe_child {
					Some(child_info) =>
						self.overlay.child_storage(child_info, key, 0, Some(0)).map(|o| o.is_some()),
					None => self.overlay.storage(key).map(|o| o.is_some()),
				};
				if !matches!(has_overlay, Some(false)) {
					// not pending deletion from the backend - delete it.
					if let Some(child_info) = maybe_child {
						self.overlay.set_child_storage(child_info, key, None);
					} else {
						self.overlay.set_storage(key.to_vec(), None);
					}
					delete_count = delete_count.saturating_add(1);
				}
				loop_count = loop_count.saturating_add(1);
				true
			});
		(maybe_next_key, delete_count, loop_count)
	}
}

/// Implement `Encode` by forwarding the stored raw vec.
struct EncodeOpaqueValue(Vec<u8>);

impl Encode for EncodeOpaqueValue {
	fn using_encoded<R, F: FnOnce(&[u8]) -> R>(&self, f: F) -> R {
		f(&self.0)
	}
}

/// Auxialiary structure for appending a value to a storage item.
pub(crate) struct StorageAppend<'a>(&'a mut Vec<u8>);

impl<'a> StorageAppend<'a> {
	/// Create a new instance using the given `storage` reference.
	pub fn new(storage: &'a mut Vec<u8>) -> Self {
		Self(storage)
	}

	/// Append the given `value` to the storage item.
	///
	/// If appending fails, `[value]` is stored in the storage item.
	pub fn append(&mut self, value: Vec<u8>) {
		let value = vec![EncodeOpaqueValue(value)];

		let item = sp_std::mem::take(self.0);

		*self.0 = match Vec::<EncodeOpaqueValue>::append_or_new(item, &value) {
			Ok(item) => item,
			Err(_) => {
				log_error!(
					target: "runtime",
					"Failed to append value, resetting storage item to `[value]`.",
				);
				value.encode()
			},
		};
	}
}

#[cfg(not(feature = "std"))]
impl<'a, H, B> ExtensionStore for Ext<'a, H, B>
where
	H: Hashers,
	H::Out: Ord + 'static + codec::Codec,
	B: Backend<H>,
{
	fn extension_by_type_id(&mut self, _type_id: TypeId) -> Option<&mut dyn Any> {
		None
	}

	fn register_extension_with_type_id(
		&mut self,
		_type_id: TypeId,
		_extension: Box<dyn Extension>,
	) -> Result<(), sp_externalities::Error> {
		Err(sp_externalities::Error::ExtensionsAreNotSupported)
	}

	fn deregister_extension_by_type_id(
		&mut self,
		_type_id: TypeId,
	) -> Result<(), sp_externalities::Error> {
		Err(sp_externalities::Error::ExtensionsAreNotSupported)
	}
}

#[cfg(feature = "std")]
impl<'a, H, B> ExtensionStore for Ext<'a, H, B>
where
	H: Hashers,
	B: 'a + Backend<H>,
{
	fn extension_by_type_id(&mut self, type_id: TypeId) -> Option<&mut dyn Any> {
		self.extensions.as_mut().and_then(|exts| exts.get_mut(type_id))
	}

	fn register_extension_with_type_id(
		&mut self,
		type_id: TypeId,
		extension: Box<dyn Extension>,
	) -> Result<(), sp_externalities::Error> {
		if let Some(ref mut extensions) = self.extensions {
			extensions.register(type_id, extension)
		} else {
			Err(sp_externalities::Error::ExtensionsAreNotSupported)
		}
	}

	fn deregister_extension_by_type_id(
		&mut self,
		type_id: TypeId,
	) -> Result<(), sp_externalities::Error> {
		if let Some(ref mut extensions) = self.extensions {
			if extensions.deregister(type_id) {
				Ok(())
			} else {
				Err(sp_externalities::Error::ExtensionIsNotRegistered(type_id))
			}
		} else {
			Err(sp_externalities::Error::ExtensionsAreNotSupported)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::InMemoryBackend;
	use codec::Encode;
	use sp_core::{
		map,
		storage::{Storage, StorageChild},
		Blake2Hasher,
	};

	type TestBackend = InMemoryBackend<Blake2Hasher>;
	type TestExt<'a> = Ext<'a, Blake2Hasher, TestBackend>;

	#[test]
	fn next_storage_key_works() {
		let mut cache = StorageTransactionCache::default();
		let mut overlay = OverlayedChanges::default();
		overlay.set_storage(vec![20], None);
		overlay.set_storage(vec![30], Some(vec![31]));
		let backend = (
			Storage {
				top: map![
					vec![10] => vec![10],
					vec![20] => vec![20],
					vec![40] => vec![40]
				],
				children_default: map![],
				btree_storages: map![],
				blob_storages: map![],
			},
			StateVersion::default(),
		)
			.into();

		let ext = TestExt::new(&mut overlay, &mut cache, &backend, None);

		// next_backend < next_overlay
		assert_eq!(ext.next_storage_key(&[5]), Some(vec![10]));

		// next_backend == next_overlay but next_overlay is a delete
		assert_eq!(ext.next_storage_key(&[10]), Some(vec![30]));

		// next_overlay < next_backend
		assert_eq!(ext.next_storage_key(&[20]), Some(vec![30]));

		// next_backend exist but next_overlay doesn't exist
		assert_eq!(ext.next_storage_key(&[30]), Some(vec![40]));

		drop(ext);
		overlay.set_storage(vec![50], Some(vec![50]));
		let ext = TestExt::new(&mut overlay, &mut cache, &backend, None);

		// next_overlay exist but next_backend doesn't exist
		assert_eq!(ext.next_storage_key(&[40]), Some(vec![50]));
	}

	#[test]
	fn next_storage_key_works_with_a_lot_empty_values_in_overlay() {
		let mut cache = StorageTransactionCache::default();
		let mut overlay = OverlayedChanges::default();
		overlay.set_storage(vec![20], None);
		overlay.set_storage(vec![21], None);
		overlay.set_storage(vec![22], None);
		overlay.set_storage(vec![23], None);
		overlay.set_storage(vec![24], None);
		overlay.set_storage(vec![25], None);
		overlay.set_storage(vec![26], None);
		overlay.set_storage(vec![27], None);
		overlay.set_storage(vec![28], None);
		overlay.set_storage(vec![29], None);
		let backend = (
			Storage {
				top: map![
					vec![30] => vec![30]
				],
				children_default: map![],
				btree_storages: map![],
				blob_storages: map![],
			},
			StateVersion::default(),
		)
			.into();

		let ext = TestExt::new(&mut overlay, &mut cache, &backend, None);

		assert_eq!(ext.next_storage_key(&[5]), Some(vec![30]));

		drop(ext);
	}

	#[test]
	fn next_child_storage_key_works() {
		let child_info = ChildInfo::new_default(b"Child1");
		let child_info = &child_info;

		let mut cache = StorageTransactionCache::default();
		let mut overlay = OverlayedChanges::default();
		overlay.set_child_storage(child_info, vec![20], None);
		overlay.set_child_storage(child_info, vec![30], Some(vec![31]));
		let backend = (
			Storage {
				top: map![],
				children_default: map![
					child_info.storage_key().to_vec() => StorageChild {
						data: map![
							vec![10] => vec![10],
							vec![20] => vec![20],
							vec![40] => vec![40]
						],
						child_info: child_info.to_owned(),
					}
				],
				btree_storages: map![],
				blob_storages: map![],
			},
			StateVersion::default(),
		)
			.into();

		let ext = TestExt::new(&mut overlay, &mut cache, &backend, None);

		// next_backend < next_overlay
		assert_eq!(ext.next_child_storage_key(child_info, &[5]), Some(vec![10]));

		// next_backend == next_overlay but next_overlay is a delete
		assert_eq!(ext.next_child_storage_key(child_info, &[10]), Some(vec![30]));

		// next_overlay < next_backend
		assert_eq!(ext.next_child_storage_key(child_info, &[20]), Some(vec![30]));

		// next_backend exist but next_overlay doesn't exist
		assert_eq!(ext.next_child_storage_key(child_info, &[30]), Some(vec![40]));

		drop(ext);
		overlay.set_child_storage(child_info, vec![50], Some(vec![50]));
		let ext = TestExt::new(&mut overlay, &mut cache, &backend, None);

		// next_overlay exist but next_backend doesn't exist
		assert_eq!(ext.next_child_storage_key(child_info, &[40]), Some(vec![50]));
	}

	#[test]
	fn child_storage_works() {
		let child_info = ChildInfo::new_default(b"Child1");
		let child_info = &child_info;
		let mut cache = StorageTransactionCache::default();
		let mut overlay = OverlayedChanges::default();
		overlay.set_child_storage(child_info, vec![20], None);
		overlay.set_child_storage(child_info, vec![30], Some(vec![31]));
		let backend = (
			Storage {
				top: map![],
				children_default: map![
					child_info.storage_key().to_vec() => StorageChild {
						data: map![
							vec![10] => vec![10],
							vec![20] => vec![20],
							vec![30] => vec![40]
						],
						child_info: child_info.to_owned(),
					}
				],
				btree_storages: map![],
				blob_storages: map![],
			},
			StateVersion::default(),
		)
			.into();

		let ext = TestExt::new(&mut overlay, &mut cache, &backend, None);

		assert_eq!(ext.child_storage(child_info, &[10]), Some(vec![10]));
		assert_eq!(
			ext.child_storage_hash(child_info, &[10]),
			Some(Blake2Hasher::hash(&[10]).as_ref().to_vec()),
		);

		assert_eq!(ext.child_storage(child_info, &[20]), None);
		assert_eq!(ext.child_storage_hash(child_info, &[20]), None);

		assert_eq!(ext.child_storage(child_info, &[30]), Some(vec![31]));
		assert_eq!(
			ext.child_storage_hash(child_info, &[30]),
			Some(Blake2Hasher::hash(&[31]).as_ref().to_vec()),
		);
	}

	#[test]
	fn clear_prefix_cannot_delete_a_child_root() {
		let child_info = ChildInfo::new_default(b"Child1");
		let child_info = &child_info;
		let mut cache = StorageTransactionCache::default();
		let mut overlay = OverlayedChanges::default();
		let backend = (
			Storage {
				top: map![],
				children_default: map![
					child_info.storage_key().to_vec() => StorageChild {
						data: map![
							vec![30] => vec![40]
						],
						child_info: child_info.to_owned(),
					}
				],
				btree_storages: map![],
				blob_storages: map![],
			},
			StateVersion::default(),
		)
			.into();

		let ext = TestExt::new(&mut overlay, &mut cache, &backend, None);

		use sp_core::storage::well_known_keys;
		let mut ext = ext;
		let mut not_under_prefix = well_known_keys::CHILD_STORAGE_KEY_PREFIX.to_vec();
		not_under_prefix[4] = 88;
		not_under_prefix.extend(b"path");
		ext.set_storage(not_under_prefix.clone(), vec![10]);

		let _ = ext.clear_prefix(&[], None, None);
		let _ = ext.clear_prefix(&well_known_keys::CHILD_STORAGE_KEY_PREFIX[..4], None, None);
		let mut under_prefix = well_known_keys::CHILD_STORAGE_KEY_PREFIX.to_vec();
		under_prefix.extend(b"path");
		let _ = ext.clear_prefix(&well_known_keys::CHILD_STORAGE_KEY_PREFIX[..4], None, None);
		assert_eq!(ext.child_storage(child_info, &[30]), Some(vec![40]));
		assert_eq!(ext.storage(not_under_prefix.as_slice()), Some(vec![10]));
		let _ = ext.clear_prefix(&not_under_prefix[..5], None, None);
		assert_eq!(ext.storage(not_under_prefix.as_slice()), None);
	}

	#[test]
	fn storage_append_works() {
		let mut data = Vec::new();
		let mut append = StorageAppend::new(&mut data);
		append.append(1u32.encode());
		append.append(2u32.encode());
		drop(append);

		assert_eq!(Vec::<u32>::decode(&mut &data[..]).unwrap(), vec![1, 2]);

		// Initialize with some invalid data
		let mut data = vec![1];
		let mut append = StorageAppend::new(&mut data);
		append.append(1u32.encode());
		append.append(2u32.encode());
		drop(append);

		assert_eq!(Vec::<u32>::decode(&mut &data[..]).unwrap(), vec![1, 2]);
	}
}
