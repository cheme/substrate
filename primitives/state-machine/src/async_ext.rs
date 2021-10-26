// This file is part of Substrate.

// Copyright (C) 2020-2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Externalities for workers.

use crate::{
	ext::guard, overlayed_changes::OverlayedChanges, trace, AsyncBackend, StorageKey, StorageValue,
};
use sp_core::{
	hexdisplay::HexDisplay,
	storage::{ChildInfo, TrackedStorageKey},
};
use sp_externalities::{
	AsyncExternalities, Externalities, TaskId, WorkerDeclaration, WorkerDeclarationKind,
	WorkerResult, WorkerState, WorkerType,
};
use sp_std::{
	any::{Any, TypeId},
	boxed::Box,
	cmp::Ordering,
	vec::Vec,
};

/// Async view on state machine Ext.
/// It contains its own set of state and rules,
/// and returns its changes on `join`.
pub struct AsyncExt {
	kind: WorkerType,
	overlay: OverlayedChanges,
	spawn_id: TaskId,
	backend: Box<dyn AsyncBackend>,
}

#[cfg(feature = "std")]
impl std::fmt::Debug for AsyncExt {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "AsyncExt {:?} at {:?}", self.kind, self.spawn_id)
	}
}

/// Obtain externality for a child worker.
pub fn new_child_worker_async_ext(
	worker_id: u64,
	declaration: WorkerDeclaration,
	backend: Box<dyn AsyncBackend>,
	parent_overlay: &mut OverlayedChanges,
) -> Option<AsyncExt> {
	let mut result = match &declaration.kind {
		WorkerDeclarationKind::Stateless =>
			return Some(AsyncExt {
				kind: WorkerType::Stateless,
				overlay: Default::default(),
				spawn_id: worker_id,
				backend: Box::new(()),
			}),
		WorkerDeclarationKind::ReadLastBlock =>
			return Some(AsyncExt {
				kind: declaration.get_type(),
				overlay: Default::default(),
				spawn_id: worker_id,
				backend,
			}),
		_ => AsyncExt {
			kind: declaration.get_type(),
			overlay: Default::default(),
			spawn_id: worker_id,
			backend,
		},
	};
	parent_overlay.child_worker_overlay(worker_id, declaration).map(move |overlay| {
		result.overlay = overlay;
		result
	})
}

impl AsyncExt {
	fn guard_stateless(&self, panic: &'static str) {
		match self.kind {
			WorkerType::Stateless => {
				panic!("{}", panic)
			},
			_ => (),
		}
	}

	fn guard_read_only(&self, panic: &'static str) {
		if !self.write_access() {
			panic!("{}", panic)
		}
	}

	/// Check if externality can write.
	/// This does not indicate that a given write will
	/// be allowend or will not result in an invalid
	/// worker execution.
	/// TODO check if used
	pub fn write_access(&self) -> bool {
		match self.kind {
			WorkerType::Stateless |
			WorkerType::ReadLastBlock |
			WorkerType::ReadAtSpawn |
			WorkerType::ReadOptimistic => false,
			WorkerType::WriteAtSpawn |
			WorkerType::WriteOptimistic |
			WorkerType::Declarative => true,
		}
	}
}

impl Externalities for AsyncExt {
	fn set_offchain_storage(&mut self, _key: &[u8], _value: Option<&[u8]>) {
		panic!("`set_offchain_storage`: should not be used in async externalities!")
	}

	fn storage(&mut self, key: &[u8]) -> Option<StorageValue> {
		self.guard_stateless("`storage`: should not be used in async externalities!");
		let _guard = guard();
		let result = self
			.overlay
			.storage(key, &self.backend)
			.map(|x| x.map(|x| x.to_vec()))
			.unwrap_or_else(|| self.backend.storage(key));
		trace!(target: "state", "{:?}: Th Get {}={:?}",
			self.spawn_id,
			HexDisplay::from(&key),
			result.as_ref().map(HexDisplay::from)
		);
		result
	}

	fn storage_hash(&mut self, _key: &[u8]) -> Option<Vec<u8>> {
		// TODO currently no hash function to avoid having to move the hasher trait
		// in AsyncExternalities extension.
		panic!("`storage_hash`: should not be used in async externalities!")
	}

	fn child_storage(&mut self, child_info: &ChildInfo, key: &[u8]) -> Option<StorageValue> {
		self.guard_stateless("`child_storage`: should not be used in async externalities!");
		let _guard = guard();
		let result = self
			.overlay
			.child_storage(child_info, key, &self.backend)
			.map(|x| x.map(|x| x.to_vec()))
			.unwrap_or_else(|| self.backend.child_storage(child_info, key));

		trace!(target: "state", "{:?}: Th GetChild({}) {}={:?}",
			self.spawn_id,
			HexDisplay::from(&child_info.storage_key()),
			HexDisplay::from(&key),
			result.as_ref().map(HexDisplay::from)
		);

		result
	}

	fn child_storage_hash(&mut self, _child_info: &ChildInfo, _key: &[u8]) -> Option<Vec<u8>> {
		panic!("`child_storage_hash`: should not be used in async externalities!")
	}

	fn next_storage_key(&mut self, key: &[u8]) -> Option<StorageKey> {
		self.guard_stateless("`next_storage_key`: should not be used in async externalities!");

		let mut next_backend_key = self.backend.next_storage_key(key);
		let next_backend_key = &mut next_backend_key;
		let backend = &mut self.backend;
		let overlay = &mut self.overlay;
		let result = || -> Option<StorageKey> {
			let mut overlay_changes = overlay.iter_after(key).peekable();

			match (&next_backend_key, overlay_changes.peek()) {
				(_, None) => next_backend_key.clone(),
				(Some(_), Some(_)) => {
					while let Some(overlay_key) = overlay_changes.next() {
						let cmp = next_backend_key.as_deref().map(|v| v.cmp(&overlay_key.0));

						// If `backend_key` is less than the `overlay_key`, we found out next key.
						if cmp == Some(Ordering::Less) {
							return next_backend_key.clone()
						} else if overlay_key.1.has_value() {
							// If there exists a value for the `overlay_key` in the overlay
							// (aka the key is still valid), it means we have found our next key.
							return Some(overlay_key.0.to_vec())
						} else if cmp == Some(Ordering::Equal) {
							// If the `backend_key` and `overlay_key` are equal, it means that we
							// need to search for the next backend key, because the overlay has
							// overwritten this key.
							*next_backend_key = backend.next_storage_key(&overlay_key.0);
						}
					}

					next_backend_key.clone()
				},
				(None, Some(_)) => {
					// Find the next overlay key that has a value attached.
					overlay_changes.find_map(|k| k.1.has_value().then(|| k.0.to_vec()))
				},
			}
		}();
		self.overlay.log_backend_interval(None, key, next_backend_key.as_ref());
		result
	}

	fn next_child_storage_key(&mut self, child_info: &ChildInfo, key: &[u8]) -> Option<StorageKey> {
		self.guard_stateless(
			"`next_child_storage_key`: should not be used in async externalities!",
		);
		let mut next_backend_key = self.backend.next_child_storage_key(child_info, key);
		let next_backend_key = &mut next_backend_key;
		let backend = &mut self.backend;
		let overlay = &mut self.overlay;
		let result = || -> Option<StorageKey> {
			let mut overlay_changes =
				overlay.child_iter_after(child_info.storage_key(), key).peekable();

			match (&next_backend_key, overlay_changes.peek()) {
				(_, None) => next_backend_key.clone(),
				(Some(_), Some(_)) => {
					while let Some(overlay_key) = overlay_changes.next() {
						let cmp = next_backend_key.as_deref().map(|v| v.cmp(&overlay_key.0));

						// If `backend_key` is less than the `overlay_key`, we found out next key.
						if cmp == Some(Ordering::Less) {
							return next_backend_key.clone()
						} else if overlay_key.1.has_value() {
							// If there exists a value for the `overlay_key` in the overlay
							// (aka the key is still valid), it means we have found our next key.
							return Some(overlay_key.0.to_vec())
						} else if cmp == Some(Ordering::Equal) {
							// If the `backend_key` and `overlay_key` are equal, it means that we
							// need to search for the next backend key, because the overlay has
							// overwritten this key.
							*next_backend_key =
								backend.next_child_storage_key(child_info, &overlay_key.0);
						}
					}

					next_backend_key.clone()
				},
				(None, Some(_)) => {
					// Find the next overlay key that has a value attached.
					overlay_changes.find_map(|k| k.1.has_value().then(|| k.0.to_vec()))
				},
			}
		}();
		self.overlay
			.log_backend_interval(Some(child_info), key, next_backend_key.as_ref());
		result
	}

	fn place_storage(&mut self, key: StorageKey, maybe_value: Option<StorageValue>) {
		self.guard_read_only(
			"`place_storage`: should not be used in read only worker externalities!",
		);
		self.overlay.set_storage(key, maybe_value);
	}

	fn place_child_storage(
		&mut self,
		child_info: &ChildInfo,
		key: StorageKey,
		value: Option<StorageValue>,
	) {
		self.guard_read_only(
			"`place_child_storage`: should not be used in read only worker externalities!",
		);
		self.overlay.set_child_storage(child_info, key, value);
	}

	fn kill_child_storage(&mut self, _child_info: &ChildInfo, _limit: Option<u32>) -> (bool, u32) {
		panic!("`kill_child_storage`: should not be used in read only worker externalities!");
	}

	fn clear_prefix(&mut self, _prefix: &[u8], _limit: Option<u32>) -> (bool, u32) {
		panic!("`clear_prefix`: should not be used in read only worker externalities!");
	}

	fn clear_child_prefix(
		&mut self,
		_child_info: &ChildInfo,
		_prefix: &[u8],
		_limit: Option<u32>,
	) -> (bool, u32) {
		panic!("`clear_child_prefix`: should not be used in read only worker externalities!");
	}

	fn storage_append(&mut self, key: Vec<u8>, value: Vec<u8>) {
		self.guard_read_only(
			"`storage_append`: should not be used in read only worker externalities!",
		);
		self.overlay.append_storage(key, value);
	}

	fn storage_root(&mut self) -> Vec<u8> {
		// TODO currently no storage_root function to avoid having to move the hasher trait
		// in AsyncExternalities extension.
		panic!("`storage_root`: should not be used in async externalities!")
	}

	fn child_storage_root(&mut self, _child_info: &ChildInfo) -> Vec<u8> {
		// TODO currently no storage_root function to avoid having to move the hasher trait
		// in AsyncExternalities extension.
		panic!("`child_storage_root`: should not be used in async externalities!")
	}

	fn storage_changes_root(&mut self, _parent: &[u8]) -> Result<Option<Vec<u8>>, ()> {
		panic!("`storage_changes_root`: should not be used in async externalities!")
	}

	fn storage_start_transaction(&mut self) {
		self.guard_read_only(
			"`storage_start_transaction`: should not be used in read only worker externalities!",
		);
		self.overlay.start_transaction()
	}

	fn storage_rollback_transaction(&mut self) -> Result<Vec<TaskId>, ()> {
		self.guard_read_only(
			"`storage_rollback_transaction`: should not be used in read only worker externalities!",
		);
		self.overlay.rollback_transaction().map_err(|_| ())
	}

	fn storage_commit_transaction(&mut self) -> Result<Vec<TaskId>, ()> {
		self.guard_read_only(
			"`storage_commit_transaction`: should not be used in read only worker externalities!",
		);
		self.overlay.commit_transaction().map_err(|_| ())
	}

	fn wipe(&mut self) {}

	fn commit(&mut self) {}

	fn read_write_count(&self) -> (u32, u32, u32, u32) {
		unimplemented!("read_write_count is not supported in AsyncExternalities")
	}

	fn reset_read_write_count(&mut self) {
		unimplemented!("reset_read_write_count is not supported in AsyncExternalities")
	}

	fn get_whitelist(&self) -> Vec<TrackedStorageKey> {
		unimplemented!("get_whitelist is not supported in AsyncExternalities")
	}

	fn set_whitelist(&mut self, _: Vec<TrackedStorageKey>) {
		unimplemented!("set_whitelist is not supported in AsyncExternalities")
	}

	fn get_worker_externalities(
		&mut self,
		worker_id: TaskId,
		declaration: WorkerDeclaration,
	) -> Option<Box<dyn AsyncExternalities>> {
		let backend = self.backend.async_backend();
		if self.kind.guard_compatible_child_workers(declaration.get_type()) {
			if let Some(result) = crate::async_ext::new_child_worker_async_ext(
				worker_id,
				declaration,
				backend,
				&mut self.overlay,
			) {
				return Some(Box::new(result))
			}
		}
		None
	}

	fn resolve_worker_result(&mut self, state_update: WorkerResult) -> Option<Vec<u8>> {
		self.overlay.resolve_worker_result(state_update)
	}

	fn dismiss_worker(&mut self, id: TaskId) {
		self.overlay.dismiss_worker(id)
	}

	fn get_read_and_written_keys(&self) -> Vec<(Vec<u8>, u32, u32, bool)> {
		unimplemented!("get_read_and_written_keys is not supported in AsyncExternalities")
	}
}

impl sp_externalities::ExtensionStore for AsyncExt {
	fn extension_by_type_id(&mut self, _type_id: TypeId) -> Option<&mut dyn Any> {
		None
	}

	fn register_extension_with_type_id(
		&mut self,
		_type_id: TypeId,
		_extension: Box<dyn sp_externalities::Extension>,
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

impl AsyncExternalities for AsyncExt {
	fn extract_delta(&mut self) -> Option<sp_externalities::StateDelta> {
		if self.write_access() {
			Some(self.overlay.extract_delta())
		} else {
			None
		}
	}

	fn extract_state(&mut self) -> WorkerState {
		if !self.kind.need_resolve() {
			return WorkerState::Valid
		}
		if self.overlay.did_fail() {
			return WorkerState::Invalid
		}
		if let Some(optimistic) = self.overlay.extract_access_log() {
			return WorkerState::Optimistic(optimistic)
		}
		WorkerState::NeedResolve
	}
}
