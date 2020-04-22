// Copyright 2017-2020 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Substrate state machine implementation.

#![warn(missing_docs)]

use std::{fmt, result, collections::HashMap, panic::UnwindSafe};
use log::{warn, trace};
use hash_db::Hasher;
use codec::{Decode, Encode, Codec};
use sp_core::{
	storage::ChildInfo, NativeOrEncoded, NeverNativeValue,
	traits::{CodeExecutor, CallInWasmExt, RuntimeCode}, hexdisplay::HexDisplay,
};
use overlayed_changes::OverlayedChangeSet;
use sp_externalities::Extensions;

pub mod backend;
mod in_memory_backend;
mod changes_trie;
mod error;
mod ext;
mod testing;
mod basic;
mod overlayed_changes;
mod proving_backend;
mod trie_backend;
mod trie_backend_essence;
mod stats;

pub use sp_trie::{trie_types::{Layout, TrieDBMut}, TrieMut, DBValue, MemoryDB,
	StorageProof, StorageProofKind, ChildrenProofMap, ProofInput, ProofInputKind};
pub use testing::TestExternalities;
pub use basic::BasicExternalities;
pub use ext::Ext;
pub use backend::Backend;
pub use changes_trie::{
	AnchorBlockId as ChangesTrieAnchorBlockId,
	State as ChangesTrieState,
	Storage as ChangesTrieStorage,
	RootsStorage as ChangesTrieRootsStorage,
	InMemoryStorage as InMemoryChangesTrieStorage,
	BuildCache as ChangesTrieBuildCache,
	CacheAction as ChangesTrieCacheAction,
	ConfigurationRange as ChangesTrieConfigurationRange,
	key_changes, key_changes_proof,
	key_changes_proof_check, key_changes_proof_check_with_db,
	prune as prune_changes_tries,
	disabled_state as disabled_changes_trie_state,
	BlockNumber as ChangesTrieBlockNumber,
};
pub use overlayed_changes::{
	OverlayedChanges, StorageChanges, StorageTransactionCache, StorageKey, StorageValue,
	StorageCollection, ChildStorageCollection,
};
pub use proving_backend::{ProofRecorder, ProvingBackend, ProvingBackendRecorder,
	create_proof_check_backend, create_flat_proof_check_backend};
pub use trie_backend_essence::{TrieBackendStorage, Storage};
pub use trie_backend::TrieBackend;
pub use error::{Error, ExecutionError};
pub use in_memory_backend::InMemory as InMemoryBackend;
pub use stats::{UsageInfo, UsageUnit, StateMachineStats};
pub use sp_core::traits::CloneableSpawn;

type CallResult<R, E> = Result<NativeOrEncoded<R>, E>;

/// Default handler of the execution manager.
pub type DefaultHandler<R, E> = fn(CallResult<R, E>, CallResult<R, E>) -> CallResult<R, E>;

/// Type of changes trie transaction.
pub type ChangesTrieTransaction<H, N> = (
	MemoryDB<H>,
	ChangesTrieCacheAction<<H as Hasher>::Out, N>,
);

/// Strategy for executing a call into the runtime.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ExecutionStrategy {
	/// Execute with the native equivalent if it is compatible with the given wasm module; otherwise fall back to the wasm.
	NativeWhenPossible,
	/// Use the given wasm module.
	AlwaysWasm,
	/// Run with both the wasm and the native variant (if compatible). Report any discrepancy as an error.
	Both,
	/// First native, then if that fails or is not possible, wasm.
	NativeElseWasm,
}

/// Storage backend trust level.
#[derive(Debug, Clone)]
pub enum BackendTrustLevel {
	/// Panics from trusted backends are considered justified, and never caught.
	Trusted,
	/// Panics from untrusted backend are caught and interpreted as runtime error.
	/// Untrusted backend may be missing some parts of the trie, so panics are not considered
	/// fatal.
	Untrusted,
}

/// Like `ExecutionStrategy` only it also stores a handler in case of consensus failure.
#[derive(Clone)]
pub enum ExecutionManager<F> {
	/// Execute with the native equivalent if it is compatible with the given wasm module; otherwise fall back to the wasm.
	NativeWhenPossible,
	/// Use the given wasm module. The backend on which code is executed code could be
	/// trusted to provide all storage or not (i.e. the light client cannot be trusted to provide
	/// for all storage queries since the storage entries it has come from an external node).
	AlwaysWasm(BackendTrustLevel),
	/// Run with both the wasm and the native variant (if compatible). Call `F` in the case of any discrepancy.
	Both(F),
	/// First native, then if that fails or is not possible, wasm.
	NativeElseWasm,
}

impl<'a, F> From<&'a ExecutionManager<F>> for ExecutionStrategy {
	fn from(s: &'a ExecutionManager<F>) -> Self {
		match *s {
			ExecutionManager::NativeWhenPossible => ExecutionStrategy::NativeWhenPossible,
			ExecutionManager::AlwaysWasm(_) => ExecutionStrategy::AlwaysWasm,
			ExecutionManager::NativeElseWasm => ExecutionStrategy::NativeElseWasm,
			ExecutionManager::Both(_) => ExecutionStrategy::Both,
		}
	}
}

impl ExecutionStrategy {
	/// Gets the corresponding manager for the execution strategy.
	pub fn get_manager<E: fmt::Debug, R: Decode + Encode>(
		self,
	) -> ExecutionManager<DefaultHandler<R, E>> {
		match self {
			ExecutionStrategy::AlwaysWasm => ExecutionManager::AlwaysWasm(BackendTrustLevel::Trusted),
			ExecutionStrategy::NativeWhenPossible => ExecutionManager::NativeWhenPossible,
			ExecutionStrategy::NativeElseWasm => ExecutionManager::NativeElseWasm,
			ExecutionStrategy::Both => ExecutionManager::Both(|wasm_result, native_result| {
				warn!(
					"Consensus error between wasm {:?} and native {:?}. Using wasm.",
					wasm_result,
					native_result,
				);
				warn!("   Native result {:?}", native_result);
				warn!("   Wasm result {:?}", wasm_result);
				wasm_result
			}),
		}
	}
}

/// Evaluate to ExecutionManager::NativeElseWasm, without having to figure out the type.
pub fn native_else_wasm<E, R: Decode>() -> ExecutionManager<DefaultHandler<R, E>> {
	ExecutionManager::NativeElseWasm
}

/// Evaluate to ExecutionManager::AlwaysWasm with trusted backend, without having to figure out the type.
fn always_wasm<E, R: Decode>() -> ExecutionManager<DefaultHandler<R, E>> {
	ExecutionManager::AlwaysWasm(BackendTrustLevel::Trusted)
}

/// Evaluate ExecutionManager::AlwaysWasm with untrusted backend, without having to figure out the type.
fn always_untrusted_wasm<E, R: Decode>() -> ExecutionManager<DefaultHandler<R, E>> {
	ExecutionManager::AlwaysWasm(BackendTrustLevel::Untrusted)
}

/// The substrate state machine.
pub struct StateMachine<'a, B, H, N, Exec>
	where
		H: Hasher,
		B: Backend<H>,
		N: ChangesTrieBlockNumber,
{
	backend: &'a B,
	exec: &'a Exec,
	method: &'a str,
	call_data: &'a [u8],
	overlay: &'a mut OverlayedChanges,
	extensions: Extensions,
	changes_trie_state: Option<ChangesTrieState<'a, H, N>>,
	storage_transaction_cache: Option<&'a mut StorageTransactionCache<B::Transaction, H, N>>,
	runtime_code: &'a RuntimeCode<'a>,
	stats: StateMachineStats,
}

impl<'a, B, H, N, Exec> Drop for StateMachine<'a, B, H, N, Exec> where
	H: Hasher,
	B: Backend<H>,
	N: ChangesTrieBlockNumber,
{
	fn drop(&mut self) {
		self.backend.register_overlay_stats(&self.stats);
	}
}

impl<'a, B, H, N, Exec> StateMachine<'a, B, H, N, Exec> where
	H: Hasher,
	H::Out: Ord + 'static + codec::Codec,
	Exec: CodeExecutor + Clone + 'static,
	B: Backend<H>,
	N: crate::changes_trie::BlockNumber,
{
	/// Creates new substrate state machine.
	pub fn new(
		backend: &'a B,
		changes_trie_state: Option<ChangesTrieState<'a, H, N>>,
		overlay: &'a mut OverlayedChanges,
		exec: &'a Exec,
		method: &'a str,
		call_data: &'a [u8],
		mut extensions: Extensions,
		runtime_code: &'a RuntimeCode,
		spawn_handle: Box<dyn CloneableSpawn>,
	) -> Self {
		extensions.register(CallInWasmExt::new(exec.clone()));
		extensions.register(sp_core::traits::TaskExecutorExt::new(spawn_handle));

		Self {
			backend,
			exec,
			method,
			call_data,
			extensions,
			overlay,
			changes_trie_state,
			storage_transaction_cache: None,
			runtime_code,
			stats: StateMachineStats::default(),
		}
	}

	/// Use given `cache` as storage transaction cache.
	///
	/// The cache will be used to cache storage transactions that can be build while executing a
	/// function in the runtime. For example, when calculating the storage root a transaction is
	/// build that will be cached.
	pub fn with_storage_transaction_cache(
		mut self,
		cache: Option<&'a mut StorageTransactionCache<B::Transaction, H, N>>,
	) -> Self {
		self.storage_transaction_cache = cache;
		self
	}

	/// Execute a call using the given state backend, overlayed changes, and call executor.
	///
	/// On an error, no prospective changes are written to the overlay.
	///
	/// Note: changes to code will be in place if this call is made again. For running partial
	/// blocks (e.g. a transaction at a time), ensure a different method is used.
	///
	/// Returns the SCALE encoded result of the executed function.
	pub fn execute(&mut self, strategy: ExecutionStrategy) -> Result<Vec<u8>, Box<dyn Error>> {
		// We are not giving a native call and thus we are sure that the result can never be a native
		// value.
		self.execute_using_consensus_failure_handler::<_, NeverNativeValue, fn() -> _>(
			strategy.get_manager(),
			None,
		).map(NativeOrEncoded::into_encoded)
	}

	fn execute_aux<R, NC>(
		&mut self,
		use_native: bool,
		native_call: Option<NC>,
	) -> (
		CallResult<R, Exec::Error>,
		bool,
	) where
		R: Decode + Encode + PartialEq,
		NC: FnOnce() -> result::Result<R, String> + UnwindSafe,
	{
		let mut cache = StorageTransactionCache::default();

		let cache = match self.storage_transaction_cache.as_mut() {
			Some(cache) => cache,
			None => &mut cache,
		};

		let mut ext = Ext::new(
			self.overlay,
			cache,
			self.backend,
			self.changes_trie_state.clone(),
			Some(&mut self.extensions),
		);

		let id = ext.id;
		trace!(
			target: "state-trace", "{:04x}: Call {} at {:?}. Input={:?}",
			id,
			self.method,
			self.backend,
			HexDisplay::from(&self.call_data),
		);

		let (result, was_native) = self.exec.call(
			&mut ext,
			self.runtime_code,
			self.method,
			self.call_data,
			use_native,
			native_call,
		);

		trace!(
			target: "state-trace", "{:04x}: Return. Native={:?}, Result={:?}",
			id,
			was_native,
			result,
		);

		(result, was_native)
	}

	fn execute_call_with_both_strategy<Handler, R, NC>(
		&mut self,
		mut native_call: Option<NC>,
		orig_prospective: OverlayedChangeSet,
		on_consensus_failure: Handler,
	) -> CallResult<R, Exec::Error>
		where
			R: Decode + Encode + PartialEq,
			NC: FnOnce() -> result::Result<R, String> + UnwindSafe,
			Handler: FnOnce(
				CallResult<R, Exec::Error>,
				CallResult<R, Exec::Error>,
			) -> CallResult<R, Exec::Error>
	{
		let (result, was_native) = self.execute_aux(true, native_call.take());

		if was_native {
			self.overlay.prospective = orig_prospective.clone();
			let (wasm_result, _) = self.execute_aux(
				false,
				native_call,
			);

			if (result.is_ok() && wasm_result.is_ok()
				&& result.as_ref().ok() == wasm_result.as_ref().ok())
				|| result.is_err() && wasm_result.is_err()
			{
				result
			} else {
				on_consensus_failure(wasm_result, result)
			}
		} else {
			result
		}
	}

	fn execute_call_with_native_else_wasm_strategy<R, NC>(
		&mut self,
		mut native_call: Option<NC>,
		orig_prospective: OverlayedChangeSet,
	) -> CallResult<R, Exec::Error>
		where
			R: Decode + Encode + PartialEq,
			NC: FnOnce() -> result::Result<R, String> + UnwindSafe,
	{
		let (result, was_native) = self.execute_aux(
			true,
			native_call.take(),
		);

		if !was_native || result.is_ok() {
			result
		} else {
			self.overlay.prospective = orig_prospective.clone();
			let (wasm_result, _) = self.execute_aux(
				false,
				native_call,
			);
			wasm_result
		}
	}

	/// Execute a call using the given state backend, overlayed changes, and call executor.
	///
	/// On an error, no prospective changes are written to the overlay.
	///
	/// Note: changes to code will be in place if this call is made again. For running partial
	/// blocks (e.g. a transaction at a time), ensure a different method is used.
	///
	/// Returns the result of the executed function either in native representation `R` or
	/// in SCALE encoded representation.
	pub fn execute_using_consensus_failure_handler<Handler, R, NC>(
		&mut self,
		manager: ExecutionManager<Handler>,
		mut native_call: Option<NC>,
	) -> Result<NativeOrEncoded<R>, Box<dyn Error>>
		where
			R: Decode + Encode + PartialEq,
			NC: FnOnce() -> result::Result<R, String> + UnwindSafe,
			Handler: FnOnce(
				CallResult<R, Exec::Error>,
				CallResult<R, Exec::Error>,
			) -> CallResult<R, Exec::Error>
	{
		let changes_tries_enabled = self.changes_trie_state.is_some();
		self.overlay.set_collect_extrinsics(changes_tries_enabled);

		let result = {
			let orig_prospective = self.overlay.prospective.clone();

			match manager {
				ExecutionManager::Both(on_consensus_failure) => {
					self.execute_call_with_both_strategy(
						native_call.take(),
						orig_prospective,
						on_consensus_failure,
					)
				},
				ExecutionManager::NativeElseWasm => {
					self.execute_call_with_native_else_wasm_strategy(
						native_call.take(),
						orig_prospective,
					)
				},
				ExecutionManager::AlwaysWasm(trust_level) => {
					let _abort_guard = match trust_level {
						BackendTrustLevel::Trusted => None,
						BackendTrustLevel::Untrusted => Some(sp_panic_handler::AbortGuard::never_abort()),
					};
					self.execute_aux(false, native_call).0
				},
				ExecutionManager::NativeWhenPossible => {
					self.execute_aux(true, native_call).0
				},
			}
		};

		result.map_err(|e| Box::new(e) as _)
	}
}

/// Prove execution using the given state backend, overlayed changes, and call executor.
pub fn prove_execution<B, H, N, Exec>(
	mut backend: B,
	overlay: &mut OverlayedChanges,
	exec: &Exec,
	spawn_handle: Box<dyn CloneableSpawn>,
	method: &str,
	call_data: &[u8],
	kind: StorageProofKind,
	runtime_code: &RuntimeCode,
) -> Result<(Vec<u8>, StorageProof), Box<dyn Error>>
where
	B: Backend<H>,
	H: Hasher,
	H::Out: Ord + 'static + codec::Codec,
	Exec: CodeExecutor + Clone + 'static,
	N: crate::changes_trie::BlockNumber,
{
	let trie_backend = backend.as_trie_backend()
		.ok_or_else(|| Box::new(ExecutionError::UnableToGenerateProof) as Box<dyn Error>)?;
	prove_execution_on_trie_backend::<_, _, N, _>(
		trie_backend,
		overlay,
		exec,
		spawn_handle,
		method,
		call_data,
		kind,
		runtime_code,
	)
}

/// Prove execution using the given trie backend, overlayed changes, and call executor.
/// Produces a state-backend-specific "transaction" which can be used to apply the changes
/// to the backing store, such as the disk.
/// Execution proof is the set of all 'touched' storage DBValues from the backend.
///
/// On an error, no prospective changes are written to the overlay.
///
/// Note: changes to code will be in place if this call is made again. For running partial
/// blocks (e.g. a transaction at a time), ensure a different method is used.
pub fn prove_execution_on_trie_backend<S, H, N, Exec>(
	trie_backend: &TrieBackend<S, H>,
	overlay: &mut OverlayedChanges,
	exec: &Exec,
	spawn_handle: Box<dyn CloneableSpawn>,
	method: &str,
	call_data: &[u8],
	kind: StorageProofKind,
	runtime_code: &RuntimeCode,
) -> Result<(Vec<u8>, StorageProof), Box<dyn Error>>
where
	S: trie_backend_essence::TrieBackendStorage<H>,
	H: Hasher,
	H::Out: Ord + 'static + codec::Codec,
	Exec: CodeExecutor + 'static + Clone,
	N: crate::changes_trie::BlockNumber,
{
	let mut proving_backend = proving_backend::ProvingBackend::new(
		trie_backend,
		kind,
	);
	let result = {
		let mut sm = StateMachine::<_, H, N, Exec>::new(
			&proving_backend,
			None,
			overlay,
			exec,
			method,
			call_data,
			Extensions::default(),
			runtime_code,
			spawn_handle,
		);

		sm.execute_using_consensus_failure_handler::<_, NeverNativeValue, fn() -> _>(
			always_wasm(),
			None,
		)?
	};
	let proof = proving_backend.extract_proof()
		.map_err(|e| Box::new(e) as Box<dyn Error>)?;
	Ok((result.into_encoded(), proof))
}

/// Check execution proof, generated by `prove_execution` call.
pub fn execution_proof_check<H, N, Exec>(
	root: H::Out,
	proof: StorageProof,
	overlay: &mut OverlayedChanges,
	exec: &Exec,
	spawn_handle: Box<dyn CloneableSpawn>,
	method: &str,
	call_data: &[u8],
	runtime_code: &RuntimeCode,
) -> Result<Vec<u8>, Box<dyn Error>>
where
	H: Hasher,
	Exec: CodeExecutor + Clone + 'static,
	H::Out: Ord + 'static + codec::Codec,
	N: crate::changes_trie::BlockNumber,
{
	match proof.kind().use_full_partial_db() {
		Some(true) => {
			let trie_backend = create_proof_check_backend::<H>(root.into(), proof)?;
			execution_proof_check_on_trie_backend::<_, N, _>(
				&trie_backend,
				overlay,
				exec,
				spawn_handle,
				method,
				call_data,
				runtime_code,
			)
		},
		Some(false) => {
			let trie_backend = create_flat_proof_check_backend::<H>(root.into(), proof)?;
			execution_flat_proof_check_on_trie_backend::<_, N, _>(
				&trie_backend,
				overlay,
				exec,
				spawn_handle,
				method,
				call_data,
				runtime_code,
			)
		},
		None => {
			return Err(Box::new("This kind of proof need to use a verify method"));
		},
	}
}

/// Check execution proof on proving backend, generated by `prove_execution` call.
pub fn execution_flat_proof_check_on_trie_backend<H, N, Exec>(
	trie_backend: &TrieBackend<MemoryDB<H>, H>,
	overlay: &mut OverlayedChanges,
	exec: &Exec,
	spawn_handle: Box<dyn CloneableSpawn>,
	method: &str,
	call_data: &[u8],
	runtime_code: &RuntimeCode,
) -> Result<Vec<u8>, Box<dyn Error>>
where
	H: Hasher,
	H::Out: Ord + 'static + codec::Codec,
	Exec: CodeExecutor + Clone + 'static,
	N: crate::changes_trie::BlockNumber,
{
	let mut sm = StateMachine::<_, H, N, Exec>::new(
		trie_backend,
		None,
		overlay,
		exec,
		method,
		call_data,
		Extensions::default(),
		runtime_code,
		spawn_handle,
	);

	sm.execute_using_consensus_failure_handler::<_, NeverNativeValue, fn() -> _>(
		always_untrusted_wasm(),
		None,
	).map(NativeOrEncoded::into_encoded)
}

/// Check execution proof on proving backend, generated by `prove_execution` call.
pub fn execution_proof_check_on_trie_backend<H, N, Exec>(
	trie_backend: &TrieBackend<ChildrenProofMap<MemoryDB<H>>, H>,
	overlay: &mut OverlayedChanges,
	exec: &Exec,
	spawn_handle: Box<dyn CloneableSpawn>,
	method: &str,
	call_data: &[u8],
	runtime_code: &RuntimeCode,
) -> Result<Vec<u8>, Box<dyn Error>>
where
	H: Hasher,
	H::Out: Ord + 'static + codec::Codec,
	Exec: CodeExecutor + Clone + 'static,
	N: crate::changes_trie::BlockNumber,
{
	let mut sm = StateMachine::<_, H, N, Exec>::new(
		trie_backend,
		None,
		overlay,
		exec,
		method,
		call_data,
		Extensions::default(),
		runtime_code,
		spawn_handle,
	);

	sm.execute_using_consensus_failure_handler::<_, NeverNativeValue, fn() -> _>(
		always_untrusted_wasm(),
		None,
	).map(NativeOrEncoded::into_encoded)
}

/// Generate storage read proof.
pub fn prove_read<B, H, I>(
	mut backend: B,
	keys: I,
	kind: StorageProofKind,
) -> Result<StorageProof, Box<dyn Error>>
where
	B: Backend<H>,
	H: Hasher,
	H::Out: Ord + Codec,
	I: IntoIterator,
	I::Item: AsRef<[u8]>,
{
	let trie_backend = backend.as_trie_backend()
		.ok_or_else(
			|| Box::new(ExecutionError::UnableToGenerateProof) as Box<dyn Error>
		)?;
	prove_read_on_trie_backend(trie_backend, keys, kind)
}

/// Generate child storage read proof.
pub fn prove_child_read<B, H, I>(
	mut backend: B,
	child_info: &ChildInfo,
	keys: I,
	kind: StorageProofKind,
) -> Result<StorageProof, Box<dyn Error>>
where
	B: Backend<H>,
	H: Hasher,
	H::Out: Ord + Codec,
	I: IntoIterator,
	I::Item: AsRef<[u8]>,
{
	let trie_backend = backend.as_trie_backend()
		.ok_or_else(|| Box::new(ExecutionError::UnableToGenerateProof) as Box<dyn Error>)?;
	prove_child_read_on_trie_backend(trie_backend, child_info, keys, kind)
}

/// Generate storage read proof on pre-created trie backend.
pub fn prove_read_on_trie_backend<S, H, I>(
	trie_backend: &TrieBackend<S, H>,
	keys: I,
	kind: StorageProofKind,
) -> Result<StorageProof, Box<dyn Error>>
where
	S: trie_backend_essence::TrieBackendStorage<H>,
	H: Hasher,
	H::Out: Ord + Codec,
	I: IntoIterator,
	I::Item: AsRef<[u8]>,
{
	let mut proving_backend = proving_backend::ProvingBackend::<_, H>::new(
		trie_backend,
		kind,
	);
	for key in keys.into_iter() {
		proving_backend
			.storage(key.as_ref())
			.map_err(|e| Box::new(e) as Box<dyn Error>)?;
	}
	Ok(proving_backend.extract_proof()
		.map_err(|e| Box::new(e) as Box<dyn Error>)?)
}

/// Generate storage read proof on pre-created trie backend.
pub fn prove_child_read_on_trie_backend<S, H, I>(
	trie_backend: &TrieBackend<S, H>,
	child_info: &ChildInfo,
	keys: I,
	kind: StorageProofKind,
) -> Result<StorageProof, Box<dyn Error>>
where
	S: trie_backend_essence::TrieBackendStorage<H>,
	H: Hasher,
	H::Out: Ord + Codec,
	I: IntoIterator,
	I::Item: AsRef<[u8]>,
{
	let mut proving_backend = proving_backend::ProvingBackend::<_, H>::new(trie_backend, kind);
	for key in keys.into_iter() {
		proving_backend
			.child_storage(child_info, key.as_ref())
			.map_err(|e| Box::new(e) as Box<dyn Error>)?;
	}
	Ok(proving_backend.extract_proof()
		.map_err(|e| Box::new(e) as Box<dyn Error>)?)
}

/// Check storage read proof, generated by `prove_read` call.
/// WARNING this method rebuild a full memory backend and should
/// be call only once per proof checks.
pub fn read_proof_check<H, I>(
	root: H::Out,
	proof: StorageProof,
	keys: I,
) -> Result<HashMap<Vec<u8>, Option<Vec<u8>>>, Box<dyn Error>>
where
	H: Hasher,
	H::Out: Ord + Codec,
	I: IntoIterator,
	I::Item: AsRef<[u8]>,
{
	let mut result = HashMap::new();
	match proof.kind().use_full_partial_db() {
		Some(true) => {
			let proving_backend = create_proof_check_backend::<H>(root, proof)?;
			for key in keys.into_iter() {
				let value = read_proof_check_on_proving_backend(&proving_backend, key.as_ref())?;
				result.insert(key.as_ref().to_vec(), value);
			}
		},
		Some(false) => {
			let proving_backend = create_flat_proof_check_backend::<H>(root, proof)?;
			for key in keys.into_iter() {
				let value = read_proof_check_on_flat_proving_backend(&proving_backend, key.as_ref())?;
				result.insert(key.as_ref().to_vec(), value);
			}
		},
		None => {
			return Err(Box::new("This kind of proof need to use a verify method"));
		},
	}
	Ok(result)
}

/// Check child storage read proof, generated by `prove_child_read` call.
pub fn read_child_proof_check<H, I>(
	root: H::Out,
	proof: StorageProof,
	child_info: &ChildInfo,
	keys: I,
) -> Result<HashMap<Vec<u8>, Option<Vec<u8>>>, Box<dyn Error>>
where
	H: Hasher,
	H::Out: Ord + Codec,
	I: IntoIterator,
	I::Item: AsRef<[u8]>,
{
	let mut result = HashMap::new();
	match proof.kind().use_full_partial_db() {
		Some(true) => {
			let proving_backend = create_proof_check_backend::<H>(root, proof)?;
			for key in keys.into_iter() {
				let value = read_child_proof_check_on_proving_backend(
					&proving_backend,
					child_info,
					key.as_ref(),
				)?;
				result.insert(key.as_ref().to_vec(), value);
			}
		},
		Some(false) => {
			let proving_backend = create_flat_proof_check_backend::<H>(root, proof)?;
			for key in keys.into_iter() {
				let value = read_child_proof_check_on_flat_proving_backend(
					&proving_backend,
					child_info,
					key.as_ref(),
				)?;
				result.insert(key.as_ref().to_vec(), value);
			}
		},
		None => {
			return Err(Box::new("This kind of proof need to use a verify method"));
		},
	}
	Ok(result)
}

/// Check storage read proof on pre-created flat proving backend.
pub fn read_proof_check_on_flat_proving_backend<H>(
	proving_backend: &TrieBackend<MemoryDB<H>, H>,
	key: &[u8],
) -> Result<Option<Vec<u8>>, Box<dyn Error>>
where
	H: Hasher,
	H::Out: Ord + Codec,
{
	proving_backend.storage(key).map_err(|e| Box::new(e) as Box<dyn Error>)
}

/// Check storage read proof on pre-created proving backend.
pub fn read_proof_check_on_proving_backend<H>(
	proving_backend: &TrieBackend<ChildrenProofMap<MemoryDB<H>>, H>,
	key: &[u8],
) -> Result<Option<Vec<u8>>, Box<dyn Error>>
where
	H: Hasher,
	H::Out: Ord + Codec,
{
	proving_backend.storage(key).map_err(|e| Box::new(e) as Box<dyn Error>)
}

/// Check child storage read proof on pre-created flat proving backend.
pub fn read_child_proof_check_on_flat_proving_backend<H>(
	proving_backend: &TrieBackend<MemoryDB<H>, H>,
	child_info: &ChildInfo,
	key: &[u8],
) -> Result<Option<Vec<u8>>, Box<dyn Error>>
where
	H: Hasher,
	H::Out: Ord + Codec,
{
	proving_backend.child_storage(child_info, key)
		.map_err(|e| Box::new(e) as Box<dyn Error>)
}

/// Check child storage read proof on pre-created proving backend.
pub fn read_child_proof_check_on_proving_backend<H>(
	proving_backend: &TrieBackend<ChildrenProofMap<MemoryDB<H>>, H>,
	child_info: &ChildInfo,
	key: &[u8],
) -> Result<Option<Vec<u8>>, Box<dyn Error>>
where
	H: Hasher,
	H::Out: Ord + Codec,
{
	proving_backend.child_storage(child_info, key)
		.map_err(|e| Box::new(e) as Box<dyn Error>)
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;
	use codec::Encode;
	use overlayed_changes::OverlayedValue;
	use super::*;
	use super::ext::Ext;
	use super::changes_trie::Configuration as ChangesTrieConfig;
	use sp_core::{map, traits::{Externalities, RuntimeCode}};
	use sp_runtime::traits::BlakeTwo256;

	#[derive(Clone)]
	struct DummyCodeExecutor {
		change_changes_trie_config: bool,
		native_available: bool,
		native_succeeds: bool,
		fallback_succeeds: bool,
	}

	impl CodeExecutor for DummyCodeExecutor {
		type Error = u8;

		fn call<
			R: Encode + Decode + PartialEq,
			NC: FnOnce() -> result::Result<R, String>,
		>(
			&self,
			ext: &mut dyn Externalities,
			_: &RuntimeCode,
			_method: &str,
			_data: &[u8],
			use_native: bool,
			_native_call: Option<NC>,
		) -> (CallResult<R, Self::Error>, bool) {
			if self.change_changes_trie_config {
				ext.place_storage(
					sp_core::storage::well_known_keys::CHANGES_TRIE_CONFIG.to_vec(),
					Some(
						ChangesTrieConfig {
							digest_interval: 777,
							digest_levels: 333,
						}.encode()
					)
				);
			}

			let using_native = use_native && self.native_available;
			match (using_native, self.native_succeeds, self.fallback_succeeds) {
				(true, true, _) | (false, _, true) => {
					(
						Ok(
							NativeOrEncoded::Encoded(
								vec![
									ext.storage(b"value1").unwrap()[0] +
									ext.storage(b"value2").unwrap()[0]
								]
							)
						),
						using_native
					)
				},
				_ => (Err(0), using_native),
			}
		}
	}

	impl sp_core::traits::CallInWasm for DummyCodeExecutor {
		fn call_in_wasm(
			&self,
			_: &[u8],
			_: Option<Vec<u8>>,
			_: &str,
			_: &[u8],
			_: &mut dyn Externalities,
		) -> std::result::Result<Vec<u8>, String> {
			unimplemented!("Not required in tests.")
		}
	}

	#[test]
	fn execute_works() {
		let backend = trie_backend::tests::test_trie();
		let mut overlayed_changes = Default::default();
		let wasm_code = RuntimeCode::empty();

		let mut state_machine = StateMachine::new(
			&backend,
			changes_trie::disabled_state::<_, u64>(),
			&mut overlayed_changes,
			&DummyCodeExecutor {
				change_changes_trie_config: false,
				native_available: true,
				native_succeeds: true,
				fallback_succeeds: true,
			},
			"test",
			&[],
			Default::default(),
			&wasm_code,
			sp_core::tasks::executor(),
		);

		assert_eq!(
			state_machine.execute(ExecutionStrategy::NativeWhenPossible).unwrap(),
			vec![66],
		);
	}


	#[test]
	fn execute_works_with_native_else_wasm() {
		let backend = trie_backend::tests::test_trie();
		let mut overlayed_changes = Default::default();
		let wasm_code = RuntimeCode::empty();

		let mut state_machine = StateMachine::new(
			&backend,
			changes_trie::disabled_state::<_, u64>(),
			&mut overlayed_changes,
			&DummyCodeExecutor {
				change_changes_trie_config: false,
				native_available: true,
				native_succeeds: true,
				fallback_succeeds: true,
			},
			"test",
			&[],
			Default::default(),
			&wasm_code,
			sp_core::tasks::executor(),
		);

		assert_eq!(state_machine.execute(ExecutionStrategy::NativeElseWasm).unwrap(), vec![66]);
	}

	#[test]
	fn dual_execution_strategy_detects_consensus_failure() {
		let mut consensus_failed = false;
		let backend = trie_backend::tests::test_trie();
		let mut overlayed_changes = Default::default();
		let wasm_code = RuntimeCode::empty();

		let mut state_machine = StateMachine::new(
			&backend,
			changes_trie::disabled_state::<_, u64>(),
			&mut overlayed_changes,
			&DummyCodeExecutor {
				change_changes_trie_config: false,
				native_available: true,
				native_succeeds: true,
				fallback_succeeds: false,
			},
			"test",
			&[],
			Default::default(),
			&wasm_code,
			sp_core::tasks::executor(),
		);

		assert!(
			state_machine.execute_using_consensus_failure_handler::<_, NeverNativeValue, fn() -> _>(
				ExecutionManager::Both(|we, _ne| {
					consensus_failed = true;
					we
				}),
				None,
			).is_err()
		);
		assert!(consensus_failed);
	}

	#[test]
	fn prove_execution_and_proof_check_works() {
		prove_execution_and_proof_check_works_inner(StorageProofKind::Flatten);
		prove_execution_and_proof_check_works_inner(StorageProofKind::Full);
		prove_execution_and_proof_check_works_inner(StorageProofKind::TrieSkipHashesFull);
		prove_execution_and_proof_check_works_inner(StorageProofKind::TrieSkipHashes);
	}

	fn prove_execution_and_proof_check_works_inner(kind: StorageProofKind) {
		let executor = DummyCodeExecutor {
			change_changes_trie_config: false,
			native_available: true,
			native_succeeds: true,
			fallback_succeeds: true,
		};

		// fetch execution proof from 'remote' full node
		let remote_backend = trie_backend::tests::test_trie();
		let remote_root = remote_backend.storage_root(std::iter::empty()).0;
		let (remote_result, remote_proof) = prove_execution::<_, _, u64, _>(
			remote_backend,
			&mut Default::default(),
			&executor,
			sp_core::tasks::executor(),
			"test",
			&[],
			kind,
			&RuntimeCode::empty(),
		).unwrap();

		// check proof locally
		let local_result = execution_proof_check::<BlakeTwo256, u64, _>(
			remote_root,
			remote_proof,
			&mut Default::default(),
			&executor,
			sp_core::tasks::executor(),
			"test",
			&[],
			&RuntimeCode::empty(),
		).unwrap();

		// check that both results are correct
		assert_eq!(remote_result, vec![66]);
		assert_eq!(remote_result, local_result);
	}

	#[test]
	fn clear_prefix_in_ext_works() {
		let initial: BTreeMap<_, _> = map![
			b"aaa".to_vec() => b"0".to_vec(),
			b"abb".to_vec() => b"1".to_vec(),
			b"abc".to_vec() => b"2".to_vec(),
			b"bbb".to_vec() => b"3".to_vec()
		];
		let mut state = InMemoryBackend::<BlakeTwo256>::from(initial);
		let backend = state.as_trie_backend().unwrap();
		let mut overlay = OverlayedChanges {
			committed: map![
				b"aba".to_vec() => OverlayedValue::from(Some(b"1312".to_vec())),
				b"bab".to_vec() => OverlayedValue::from(Some(b"228".to_vec()))
			],
			prospective: map![
				b"abd".to_vec() => OverlayedValue::from(Some(b"69".to_vec())),
				b"bbd".to_vec() => OverlayedValue::from(Some(b"42".to_vec()))
			],
			..Default::default()
		};

		{
			let mut cache = StorageTransactionCache::default();
			let mut ext = Ext::new(
				&mut overlay,
				&mut cache,
				backend,
				changes_trie::disabled_state::<_, u64>(),
				None,
			);
			ext.clear_prefix(b"ab");
		}
		overlay.commit_prospective();

		assert_eq!(
			overlay.committed,
			map![
				b"abc".to_vec() => None.into(),
				b"abb".to_vec() => None.into(),
				b"aba".to_vec() => None.into(),
				b"abd".to_vec() => None.into(),

				b"bab".to_vec() => Some(b"228".to_vec()).into(),
				b"bbd".to_vec() => Some(b"42".to_vec()).into()
			],
		);
	}

	#[test]
	fn set_child_storage_works() {
		let child_info = ChildInfo::new_default(b"sub1");
		let child_info = &child_info;
		let mut state = InMemoryBackend::<BlakeTwo256>::default();
		let backend = state.as_trie_backend().unwrap();
		let mut overlay = OverlayedChanges::default();
		let mut cache = StorageTransactionCache::default();
		let mut ext = Ext::new(
			&mut overlay,
			&mut cache,
			backend,
			changes_trie::disabled_state::<_, u64>(),
			None,
		);

		ext.set_child_storage(
			child_info,
			b"abc".to_vec(),
			b"def".to_vec()
		);
		assert_eq!(
			ext.child_storage(
				child_info,
				b"abc"
			),
			Some(b"def".to_vec())
		);
		ext.kill_child_storage(
			child_info,
		);
		assert_eq!(
			ext.child_storage(
				child_info,
				b"abc"
			),
			None
		);
	}

	#[test]
	fn prove_read_and_proof_check_works() {
		prove_read_and_proof_check_works_inner(StorageProofKind::Full);
		prove_read_and_proof_check_works_inner(StorageProofKind::Flatten);
		prove_read_and_proof_check_works_inner(StorageProofKind::TrieSkipHashesFull);
		prove_read_and_proof_check_works_inner(StorageProofKind::TrieSkipHashes);
	}

	fn prove_read_and_proof_check_works_inner(kind: StorageProofKind) {

		let child_info = ChildInfo::new_default(b"sub1");
		let child_info = &child_info;
		// fetch read proof from 'remote' full node
		let remote_backend = trie_backend::tests::test_trie();
		let remote_root = remote_backend.storage_root(::std::iter::empty()).0;
		let remote_proof = prove_read(remote_backend, &[b"value2"], kind).unwrap();
 		// check proof locally
		let local_result1 = read_proof_check::<BlakeTwo256, _>(
			remote_root,
			remote_proof.clone(),
			&[b"value2"],
		).unwrap();
		let local_result2 = read_proof_check::<BlakeTwo256, _>(
			remote_root,
			remote_proof.clone(),
			&[&[0xff]],
		).is_ok();
 		// check that results are correct
		assert_eq!(
			local_result1.into_iter().collect::<Vec<_>>(),
			vec![(b"value2".to_vec(), Some(vec![24]))],
		);
		assert_eq!(local_result2, false);
		// on child trie
		let remote_backend = trie_backend::tests::test_trie();
		let remote_root = remote_backend.storage_root(::std::iter::empty()).0;
		let remote_proof = prove_child_read(
			remote_backend,
			child_info,
			&[b"value3"],
			kind,
		).unwrap();
		let local_result1 = read_child_proof_check::<BlakeTwo256, _>(
			remote_root,
			remote_proof.clone(),
			child_info,
			&[b"value3"],
		).unwrap();
		let local_result2 = read_child_proof_check::<BlakeTwo256, _>(
			remote_root,
			remote_proof.clone(),
			child_info,
			&[b"value2"],
		).unwrap();
		assert_eq!(
			local_result1.into_iter().collect::<Vec<_>>(),
			vec![(b"value3".to_vec(), Some(vec![142]))],
		);
		assert_eq!(
			local_result2.into_iter().collect::<Vec<_>>(),
			vec![(b"value2".to_vec(), None)],
		);
	}

	#[test]
	fn child_storage_uuid() {

		let child_info_1 = ChildInfo::new_default(b"sub_test1");
		let child_info_2 = ChildInfo::new_default(b"sub_test2");

		use crate::trie_backend::tests::test_trie;
		let mut overlay = OverlayedChanges::default();

		let mut transaction = {
			let backend = test_trie();
			let mut cache = StorageTransactionCache::default();
			let mut ext = Ext::new(
				&mut overlay,
				&mut cache,
				&backend,
				changes_trie::disabled_state::<_, u64>(),
				None,
			);
			ext.set_child_storage(&child_info_1, b"abc".to_vec(), b"def".to_vec());
			ext.set_child_storage(&child_info_2, b"abc".to_vec(), b"def".to_vec());
			ext.storage_root();
			cache.transaction.unwrap()
		};
		let mut duplicate = false;
		for (k, (value, rc)) in transaction.drain().iter() {
			// look for a key inserted twice: transaction rc is 2
			if *rc == 2 {
				duplicate = true;
				println!("test duplicate for {:?} {:?}", k, value);
			}
		}
		assert!(!duplicate);
	}
}
