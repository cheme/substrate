// This file is part of Substrate.

// Copyright (C) 2020-2021 Parity Technologies (UK) Ltd.
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

//! Read-only version of Externalities.

use std::{
	any::{TypeId, Any},
	marker::PhantomData,
};
use crate::{Backend, StorageKey, StorageValue};
use hash_db::Hasher;
use sp_core::{
	storage::{ChildInfo, TrackedStorageKey},
	traits::Externalities, Blake2Hasher,
};
use codec::Encode;
use sp_externalities::{TaskId, WorkerResult, WorkerDeclaration, AsyncExternalities};

/// Trait for inspecting state in any backend.
///
/// Implemented for any backend.
pub trait InspectState<H: Hasher, B: Backend<H>> {
	/// Inspect state with a closure.
	///
	/// Self will be set as read-only externalities and inspection
	/// closure will be run against it.
	///
	/// Returns the result of the closure.
	fn inspect_state<F: FnOnce() -> R, R>(&self, f: F) -> R;
}

impl<H: Hasher, B: Backend<H>> InspectState<H, B> for B {
	fn inspect_state<F: FnOnce() -> R, R>(&self, f: F) -> R {
		ReadOnlyExternalities::from(self).execute_with(f)
	}
}

/// Simple read-only externalities for any backend.
///
/// To be used in test for state inspection. Will panic if something writes
/// to the storage.
#[derive(Debug)]
pub struct ReadOnlyExternalities<'a, H: Hasher, B: 'a + Backend<H>> {
	backend: &'a B,
	// Note that overlay is only here to manage worker declaration
	// and will never contain changes.
	overlay: crate::overlayed_changes::OverlayedChanges,
	_phantom: PhantomData<H>,
}

impl<'a, H: Hasher, B: 'a + Backend<H>> From<&'a B> for ReadOnlyExternalities<'a, H, B> {
	fn from(backend: &'a B) -> Self {
		ReadOnlyExternalities { backend, overlay: Default::default(), _phantom: PhantomData }
	}
}

impl<'a, H: Hasher, B: 'a + Backend<H>> ReadOnlyExternalities<'a, H, B> {
	/// Execute the given closure while `self` is set as externalities.
	///
	/// Returns the result of the given closure.
	pub fn execute_with<R>(&mut self, f: impl FnOnce() -> R) -> R {
		sp_externalities::set_and_run_with_externalities(self, f)
	}
}

impl<'a, H: Hasher, B: 'a + Backend<H>> Externalities for ReadOnlyExternalities<'a, H, B> {
	fn set_offchain_storage(&mut self, _key: &[u8], _value: Option<&[u8]>) {
		panic!("Should not be used in read-only externalities!")
	}

	fn storage(&mut self, key: &[u8]) -> Option<StorageValue> {
		self.backend.storage(key).expect("Backed failed for storage in ReadOnlyExternalities")
	}

	fn storage_hash(&mut self, key: &[u8]) -> Option<Vec<u8>> {
		self.storage(key).map(|v| Blake2Hasher::hash(&v).encode())
	}

	fn child_storage(
		&mut self,
		child_info: &ChildInfo,
		key: &[u8],
	) -> Option<StorageValue> {
		self.backend.child_storage(child_info, key).expect("Backed failed for child_storage in ReadOnlyExternalities")
	}

	fn child_storage_hash(
		&mut self,
		child_info: &ChildInfo,
		key: &[u8],
	) -> Option<Vec<u8>> {
		self.child_storage(child_info, key).map(|v| Blake2Hasher::hash(&v).encode())
	}

	fn next_storage_key(&mut self, key: &[u8]) -> Option<StorageKey> {
		self.backend.next_storage_key(key).expect("Backed failed for next_storage_key in ReadOnlyExternalities")
	}

	fn next_child_storage_key(
		&mut self,
		child_info: &ChildInfo,
		key: &[u8],
	) -> Option<StorageKey> {
		self.backend.next_child_storage_key(child_info, key)
			.expect("Backed failed for next_child_storage_key in ReadOnlyExternalities")
	}

	fn place_storage(&mut self, _key: StorageKey, _maybe_value: Option<StorageValue>) {
		unimplemented!("place_storage not supported in ReadOnlyExternalities")
	}

	fn place_child_storage(
		&mut self,
		_child_info: &ChildInfo,
		_key: StorageKey,
		_value: Option<StorageValue>,
	) {
		unimplemented!("place_child_storage not supported in ReadOnlyExternalities")
	}

	fn kill_child_storage(
		&mut self,
		_child_info: &ChildInfo,
		_limit: Option<u32>,
	) -> (bool, u32) {
		unimplemented!("kill_child_storage is not supported in ReadOnlyExternalities")
	}

	fn clear_prefix(&mut self, _prefix: &[u8]) {
		unimplemented!("clear_prefix is not supported in ReadOnlyExternalities")
	}

	fn clear_child_prefix(
		&mut self,
		_child_info: &ChildInfo,
		_prefix: &[u8],
	) {
		unimplemented!("clear_child_prefix is not supported in ReadOnlyExternalities")
	}

	fn storage_append(
		&mut self,
		_key: Vec<u8>,
		_value: Vec<u8>,
	) {
		unimplemented!("storage_append is not supported in ReadOnlyExternalities")
	}

	fn storage_root(&mut self) -> Vec<u8> {
		unimplemented!("storage_root is not supported in ReadOnlyExternalities")
	}

	fn child_storage_root(
		&mut self,
		_child_info: &ChildInfo,
	) -> Vec<u8> {
		unimplemented!("child_storage_root is not supported in ReadOnlyExternalities")
	}

	fn storage_changes_root(&mut self, _parent: &[u8]) -> Result<Option<Vec<u8>>, ()> {
		unimplemented!("storage_changes_root is not supported in ReadOnlyExternalities")
	}

	fn storage_start_transaction(&mut self) {
		unimplemented!("Transactions are not supported by ReadOnlyExternalities");
	}

	fn storage_rollback_transaction(&mut self) -> Result<Vec<TaskId>, ()> {
		unimplemented!("Transactions are not supported by ReadOnlyExternalities");
	}

	fn storage_commit_transaction(&mut self) -> Result<Vec<TaskId>, ()> {
		unimplemented!("Transactions are not supported by ReadOnlyExternalities");
	}

	fn wipe(&mut self) {}

	fn commit(&mut self) {}

	fn read_write_count(&self) -> (u32, u32, u32, u32) {
		unimplemented!("read_write_count is not supported in ReadOnlyExternalities")
	}

	fn reset_read_write_count(&mut self) {
		unimplemented!("reset_read_write_count is not supported in ReadOnlyExternalities")
	}

	fn get_whitelist(&self) -> Vec<TrackedStorageKey> {
		unimplemented!("get_whitelist is not supported in ReadOnlyExternalities")
	}

	fn set_whitelist(&mut self, _: Vec<TrackedStorageKey>) {
		unimplemented!("set_whitelist is not supported in ReadOnlyExternalities")
	}

	fn get_worker_externalities(
		&mut self,
		worker_id: u64,
		declaration: WorkerDeclaration,
	) -> Option<Box<dyn AsyncExternalities>> {
		let async_backend = self.backend.async_backend();
		if let Some(result) = crate::async_ext::new_child_worker_async_ext(
			worker_id,
			declaration,
			async_backend,
			&mut self.overlay, // No current overlay state since read only.
		) {
			return Some(Box::new(result));
		}
		None
	}

	fn resolve_worker_result(&mut self, state_update: WorkerResult) -> Option<Vec<u8>> {
		match state_update {
			WorkerResult::CallAt(result, None, ..)
			| WorkerResult::Optimistic(result, None, ..)
			| WorkerResult::Valid(result, None, ..) => Some(result),
			WorkerResult::CallAt(result, Some(delta), ..)
			| WorkerResult::Optimistic(result, Some(delta), ..)
			| WorkerResult::Valid(result, Some(delta), ..) => {
				if delta.is_empty() {
					Some(result)
				} else {
					unimplemented!("Storing change is not supported in ReadOnlyExternalities");
				}
			},
			WorkerResult::Invalid => None,
			WorkerResult::RuntimePanic => {
				panic!("Runtime panic from a worker.")
			},
			WorkerResult::HardPanic => {
				panic!("Hard panic runing a worker.")
			},
		}
	}

	fn dismiss_worker(&mut self, _id: TaskId) {
	}
}

impl<'a, H: Hasher, B: 'a + Backend<H>> sp_externalities::ExtensionStore for ReadOnlyExternalities<'a, H, B> {
	fn extension_by_type_id(&mut self, _type_id: TypeId) -> Option<&mut dyn Any> {
		unimplemented!("extension_by_type_id is not supported in ReadOnlyExternalities")
	}

	fn register_extension_with_type_id(
		&mut self,
		_type_id: TypeId,
		_extension: Box<dyn sp_externalities::Extension>,
	) -> Result<(), sp_externalities::Error> {
		unimplemented!("register_extension_with_type_id is not supported in ReadOnlyExternalities")
	}

	fn deregister_extension_by_type_id(&mut self, _type_id: TypeId) -> Result<(), sp_externalities::Error> {
		unimplemented!("deregister_extension_by_type_id is not supported in ReadOnlyExternalities")
	}
}
