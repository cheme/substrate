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

//! Inline RuntimeSpawn implementation.
//!
//! This is a minimal implementation to support runtime workers.
//!
//! As a minimal implementation it can run in no_std (with alloc), but do not
//! actually spawn threads, all execution is done inline in the parent thread.

use sp_tasks::{new_inline_only_externalities, AsyncExt, AsyncStateType};
use sp_core::traits::RuntimeSpawn;
use sp_externalities::{WorkerResult, Externalities};
use sp_std::rc::Rc;
use sp_std::cell::RefCell;
use sp_std::collections::btree_map::BTreeMap;
use sp_std::sync::Arc;
use sp_std::boxed::Box;
use sp_std::vec::Vec;
use sp_std::marker::PhantomData;
#[cfg(feature = "std")]
use crate::wasm_runtime::{WasmInstance, WasmModule, InvokeMethod};
#[cfg(feature = "std")]
use crate::error::Error;
#[cfg(feature = "std")]
use parking_lot::Mutex;
#[cfg(feature = "std")]
use std::panic::{AssertUnwindSafe, UnwindSafe};
#[cfg(feature = "std")]
pub use log::error as log_error;

/// In no_std we skip log, this macro
/// is a noops.
#[cfg(not(feature = "std"))]
macro_rules! log_error {
	(target: $target:expr, $($arg:tt)+) => (
		()
	);
	($($arg:tt)+) => (
		()
	);
}

/// Indicate if this run as a local
/// function without runtime boundaries.
/// If it does, it is safe to assume
/// that a wasm pointer can be call directly.
pub trait HostLocalFunction: Copy + 'static {
	/// Associated boolean constant indicating if
	/// a struct is being use in the hosting context.
	///
	/// It defaults to false.
	const HOST_LOCAL: bool = false;
}

impl HostLocalFunction for () { }

/// `HostLocalFunction` implementation that
/// indicate we are using hosted runtime.
#[derive(Clone, Copy)]
pub struct HostLocal;

impl HostLocalFunction for HostLocal {
	const HOST_LOCAL: bool = true;
}

/// Helper inner struct to implement `RuntimeSpawn` extension.
/// TODO maybe RunningTask param is useless
pub struct RuntimeInstanceSpawn<HostLocalFunction = ()> {
	#[cfg(feature = "std")]
	module: Option<Box<dyn WasmModule>>,
	#[cfg(feature = "std")]
	instance: Option<AssertUnwindSafe<Box<dyn WasmInstance>>>,
	tasks: BTreeMap<u64, PendingTask>,
	counter: u64,
	_ph: PhantomData<HostLocalFunction>,
}

/// Set up the externalities and safe calling environment to execute runtime calls.
///
/// If the inner closure panics, it will be caught and return an error.
#[cfg(feature = "std")]
pub fn with_externalities_safe<F, U>(ext: &mut dyn Externalities, f: F) -> Result<U, Error>
	where F: UnwindSafe + FnOnce() -> U
{
	// TODO here externalities is used as environmental and inherently is
	// making the `AssertUnwindSafe` assertion, that is not true.
	// Worst case is probably locked mutex, so not that harmfull.
	// The thread scenario adds a bit over it but there was already
	// locked backend.
	sp_externalities::set_and_run_with_externalities(
		ext,
		move || {
			// Substrate uses custom panic hook that terminates process on panic. Disable
			// termination for the native call.
			let _guard = sp_panic_handler::AbortGuard::force_unwind();
			std::panic::catch_unwind(f).map_err(|e| {
				if let Some(err) = e.downcast_ref::<String>() {
					Error::RuntimePanicked(err.clone())
				} else if let Some(err) = e.downcast_ref::<&'static str>() {
					Error::RuntimePanicked(err.to_string())
				} else {
					Error::RuntimePanicked("Unknown panic".into())
				}
			})
		},
	)
}

/// Not std `with_externalities_safe` is only for use with environment
/// where no threads are use.
/// This will NOT catch panic.
///
/// This explains that any panic from a worker using thread have to panic
/// the parent thread on join (not if dismissed since inline processing
/// is lazy).
#[cfg(not(feature = "std"))]
fn with_externalities_safe<F, U>(ext: &mut dyn Externalities, f: F) -> Result<U, ()>
	where F: FnOnce() -> U
{
	Ok(sp_externalities::set_and_run_with_externalities(
		ext,
		f,
	))
}

/// A call for wasm context.
pub struct WasmTask {
	/// Pointer to its dispatcher function:
	/// a wasm function that redirect the calls.
	pub dispatcher_ref: u32,
	/// Input data for this task call.
	pub data: Vec<u8>,
	/// Pointer to the actual wasm function.
	pub func: u32,
}

/// A native call, it directly access the function
/// to call.
pub struct NativeTask {
	/// Function to call with this task.
	pub func: fn(Vec<u8>) -> Vec<u8>,
	/// Input data for this task call.
	pub data: Vec<u8>,
}

/// A worker task, this is a callable function.
pub enum Task {
	/// See `NativeTask`.
	Native(NativeTask),
	/// See `WasmTask`.
	Wasm(WasmTask),
}

/// A task and its context that is waiting
/// to be processed or dismissed.
pub struct PendingTask {
	/// The actual `Task`.
	pub task: Task,
	/// The associated `Externalities`
	/// this task will get access to.
	pub ext: AsyncExt,
}

#[cfg(feature = "std")]
/// TODO
pub fn instantiate(
	module: Option<&dyn WasmModule>,
) -> Option<AssertUnwindSafe<Box<dyn WasmInstance>>> {
	Some(match module.map(|m| m.new_instance()) {
		Some(Ok(val)) => AssertUnwindSafe(val),
		Some(Err(e)) => {
			log_error!(
				target: "executor",
				"Failed to create new instance for module for async context: {}",
				e,
			);
			return None;
		}
		None => {
			log_error!(target: "executor", "No module for a wasm task.");
			return None;
		},
	})
}

/// Obtain externality and get id for worker.
pub fn spawn_call_ext(
	handle: u64,
	kind: u8,
	calling_ext: &mut dyn Externalities,
) -> AsyncExt {
	match AsyncStateType::from_u8(kind)
		// TODO better message
		.expect("Only from existing kind") {
		AsyncStateType::Stateless => {
			AsyncExt::stateless_ext()
		},
		AsyncStateType::ReadLastBlock => {
			let backend = calling_ext.get_past_async_backend()
				.expect("Unsupported spawn kind.");
			AsyncExt::previous_block_read(backend)
		},
		AsyncStateType::ReadAtSpawn => {
			let backend = calling_ext.get_async_backend(handle)
				.expect("Unsupported spawn kind.");
			AsyncExt::state_at_spawn_read(backend, handle)
		},
	}
}

/// Technical trait to factor code.
/// It access the instance lazilly from a module.
#[cfg(feature = "std")]
pub trait LazyInstanciate<'a> {
	/// Calling this function consume the lazy instantiate struct (similar
	/// semantic as `FnOnce`, and return a pointer to the existing or instantiated
	/// wasm instance.
	///
	/// Can return `None` when no module was defined or an error occurs, this should
	/// be considered as a panicking situation.
	fn instantiate(self) -> Option<&'a AssertUnwindSafe<Box<dyn WasmInstance>>>;
}

/// Lazy instantiaty for wasm instance.
#[cfg(feature = "std")]
pub struct InlineInstantiate<'a> {
	module: &'a Option<Box<dyn WasmModule>>,
	instance: &'a mut Option<AssertUnwindSafe<Box<dyn WasmInstance>>>,
}

#[cfg(feature = "std")]
impl<'a> LazyInstanciate<'a> for InlineInstantiate<'a> {
	fn instantiate(self) -> Option<&'a AssertUnwindSafe<Box<dyn WasmInstance>>> {
		if self.instance.is_none() {
			*self.instance = if let Some(instance) = instantiate(self.module.as_ref().map(AsRef::as_ref)) {
				Some(instance)
			} else {
				return None
			}
		};
		self.instance.as_ref()
	}
}

/// Run a given task inline.
pub fn process_task_inline<
	'a,
	HostLocal: HostLocalFunction,
	#[cfg(feature = "std")]
	I: LazyInstanciate<'a> + 'a,
>(
	task: Task,
	ext: AsyncExt,
	handle: u64,
	runtime_ext: Box<dyn RuntimeSpawn>,
	#[cfg(feature = "std")]
	instance_ref: I,
) -> WorkerResult {
	let async_ext = match new_inline_only_externalities(ext) {
		Ok(val) => val,
		Err(e) => {
			log_error!(
				target: "executor",
				"Failed to setup externalities for inline async context: {}",
				e,
			);
			return WorkerResult::HardPanic;
		}
	};
	let async_ext = match async_ext.with_runtime_spawn(runtime_ext) {
		Ok(val) => val,
		Err(e) => {
			log_error!(
				target: "executor",
				"Failed to setup runtime extension for async externalities: {}",
				e,
			);

			return WorkerResult::HardPanic;
		}
	};

	#[cfg(feature = "std")]
	{
		process_task::<HostLocal, _>(task, async_ext, handle, instance_ref)
	}
	#[cfg(not(feature = "std"))]
	{
		process_task::<HostLocal>(task, async_ext, handle)
	}
}


/// Run a given task.
pub fn process_task<
	'a,
	HostLocal: HostLocalFunction,
	#[cfg(feature = "std")]
	I: LazyInstanciate<'a> + 'a,
>(
	task: Task,
	mut async_ext: sp_tasks::AsyncExternalities,
	handle: u64,
	#[cfg(feature = "std")]
	instance_ref: I,
) -> WorkerResult {
	let need_resolve = async_ext.need_resolve();

	match task {
		Task::Wasm(WasmTask { dispatcher_ref, func, data }) => {

			#[cfg(feature = "std")]
			let result = if HostLocal::HOST_LOCAL {
				panic!("HOST_LOCAL is only expected for a wasm call");
			} else {
				let instance = if let Some(instance) = instance_ref.instantiate() {
					instance
				} else {
					return WorkerResult::HardPanic;
				};
				with_externalities_safe(
					&mut async_ext,
					|| instance.call(
						InvokeMethod::TableWithWrapper { dispatcher_ref, func },
						&data[..],
					)
				)
			};
			#[cfg(not(feature = "std"))]
			let result: Result<Result<_, ()>, _> = if HostLocal::HOST_LOCAL {
				let f: fn(Vec<u8>) -> Vec<u8> = unsafe { sp_std::mem::transmute(func) };
				with_externalities_safe(
					&mut async_ext,
					|| Ok(f(data)),
				)
			} else {
				panic!("No no_std wasm runner");
			};

			match result {
				// TODO if we knew tihs is stateless, we could return valid
				Ok(Ok(result)) => if need_resolve {
					WorkerResult::CallAt(result, handle)
				} else {
					WorkerResult::Valid(result)
				},
				Ok(Err(error)) => {
					log_error!("Wasm instance error in : {:?}", error);
					WorkerResult::HardPanic
				},
				Err(error) => {
					log_error!("Panic error in sinlined task: {:?}", error);
					WorkerResult::Panic
				}
			}
		},
		Task::Native(NativeTask { func, data }) => {
			match with_externalities_safe(
				&mut async_ext,
				|| func(data),
			) {
				// TODO Here if we got info about task being stateless, we could
				// directly return Valid
				Ok(result) => if need_resolve {
					WorkerResult::CallAt(result, handle)
				} else {
					WorkerResult::Valid(result)
				},
				Err(error) => {
					log_error!("Panic error in sinlined task: {:?}", error);
					WorkerResult::Panic
				}
			}
		},
	}
}

impl<HostLocal: HostLocalFunction> RuntimeInstanceSpawn<HostLocal> {
	/// Instantiate an inline instance spawn with
	/// support for wasm workers and sp_io calls.
	#[cfg(feature = "std")]
	pub fn with_module(module: Box<dyn WasmModule>) -> Self {
		let mut result = Self::new();
		result.module = Some(module);
		result
	}

	/// Instantiate an inline instance spawn without
	/// a wasm module.
	/// This can be use if we are sure native only will
	/// be use or if we are not using sp_io calls.
	pub fn new() -> Self {
		RuntimeInstanceSpawn {
			#[cfg(feature = "std")]
			module: None,
			#[cfg(feature = "std")]
			instance: None,
			tasks: BTreeMap::new(),
			counter: 0,
			_ph: PhantomData,
		}
	}

	/// Base implementation for `RuntimeSpawn` method.
	pub fn dismiss(&mut self, handle: u64) {
		self.tasks.remove(&handle);
	}
}

impl<HostLocal: HostLocalFunction> RuntimeInstanceSpawn<HostLocal> {
	fn spawn_call_inner(
		&mut self,
		task: Task,
		kind: u8,
		calling_ext: &mut dyn Externalities,
	) -> u64 {
		let handle = self.counter;
		self.counter += 1;
		let ext = spawn_call_ext(handle, kind, calling_ext);

		self.tasks.insert(handle, PendingTask {task, ext});

		handle
	}

	/// Base implementation for `RuntimeSpawn` method.
	pub fn spawn_call_native(
		&mut self,
		func: fn(Vec<u8>) -> Vec<u8>,
		data: Vec<u8>,
		kind: u8,
		calling_ext: &mut dyn Externalities,
	) -> u64 {
		let task = Task::Native(NativeTask { func, data });
		self.spawn_call_inner(task, kind, calling_ext)
	}

	/// Base implementation for `RuntimeSpawn` method.
	pub fn spawn_call(
		&mut self,
		dispatcher_ref: u32,
		func: u32,
		data: Vec<u8>,
		kind: u8,
		calling_ext: &mut dyn Externalities,
	) -> u64 {
		let task = Task::Wasm(WasmTask { dispatcher_ref, func, data });
		self.spawn_call_inner(task, kind, calling_ext)
	}

	/// Base implementation for `RuntimeSpawn` method.
	pub fn join(
		&mut self,
		handle: u64,
		calling_ext: &mut dyn Externalities,
		spawn_rec: Box<dyn RuntimeSpawn>,
	) -> Option<Vec<u8>> {
		let worker_result = match self.tasks.remove(&handle) {
			Some(task) => {
				#[cfg(feature = "std")]
				{
					let instance_ref = InlineInstantiate {
						module: &self.module,
						instance: &mut self.instance,
					};
					process_task_inline::<HostLocal, _>(task.task, task.ext, handle, spawn_rec, instance_ref)
				}
				#[cfg(not(feature = "std"))]
				process_task_inline::<HostLocal>(task.task, task.ext, handle, spawn_rec)
			},
			// handle has been removed due to dismiss or
			// invalid externality condition.
			None => WorkerResult::Invalid,
		};

		calling_ext.resolve_worker_result(worker_result)
	}

}

/// Inline instance spawn, to use with nodes that can manage threads.
#[cfg(feature = "std")]
#[derive(Clone)]
pub struct RuntimeInstanceSpawnSend(Arc<Mutex<RuntimeInstanceSpawn>>);

#[cfg(feature = "std")]
impl RuntimeSpawn for RuntimeInstanceSpawnSend {
	fn spawn_call_native(
		&self,
		func: fn(Vec<u8>) -> Vec<u8>,
		data: Vec<u8>,
		kind: u8,
		calling_ext: &mut dyn Externalities,
	) -> u64 {
		self.0.lock().spawn_call_native(func, data, kind, calling_ext)
	}

	fn spawn_call(
		&self,
		dispatcher_ref: u32,
		func: u32,
		data: Vec<u8>,
		kind: u8,
		calling_ext: &mut dyn Externalities,
	) -> u64 {
		self.0.lock().spawn_call(dispatcher_ref, func, data, kind, calling_ext)
	}

	fn join(&self, handle: u64, calling_ext: &mut dyn Externalities) -> Option<Vec<u8>> {
		self.0.lock().join(handle, calling_ext, Box::new(self.clone()))
	}

	fn dismiss(&self, handle: u64) {
		self.0.lock().dismiss(handle)
	}

	fn set_capacity(&self, _capacity: u32) {
		// No capacity, only inline, skip useless lock.
	}
}

/// Inline instance spawn, this run all workers lazilly when `join` is called.
///
/// Warning to use only with environment that do not use threads (mainly wasm)
/// and thus allows the unsafe `Send` declaration.
#[derive(Clone)]
pub struct RuntimeInstanceSpawnForceSend<HostLocal>(Rc<RefCell<RuntimeInstanceSpawn<HostLocal>>>);

unsafe impl<HostLocal> Send for RuntimeInstanceSpawnForceSend<HostLocal> { }

impl<HostLocal: HostLocalFunction> RuntimeSpawn for RuntimeInstanceSpawnForceSend<HostLocal> {
	fn spawn_call_native(
		&self,
		func: fn(Vec<u8>) -> Vec<u8>,
		data: Vec<u8>,
		kind: u8,
		calling_ext: &mut dyn Externalities,
	) -> u64 {
		self.0.borrow_mut().spawn_call_native(func, data, kind, calling_ext)
	}

	fn spawn_call(
		&self,
		dispatcher_ref: u32,
		func: u32,
		data: Vec<u8>,
		kind: u8,
		calling_ext: &mut dyn Externalities,
	) -> u64 {
		self.0.borrow_mut().spawn_call(dispatcher_ref, func, data, kind, calling_ext)
	}

	fn join(&self, handle: u64, calling_ext: &mut dyn Externalities) -> Option<Vec<u8>> {
		self.0.borrow_mut().join(handle, calling_ext, Box::new(self.clone()))
	}

	fn dismiss(&self, handle: u64) {
		self.0.borrow_mut().dismiss(handle)
	}

	fn set_capacity(&self, _capacity: u32) {
		// No capacity, only inline, skip useless lock.
	}
}

impl<HostLocal: HostLocalFunction> RuntimeInstanceSpawnForceSend<HostLocal> {
	/// Instantial a new inline `RuntimeSpawn`.
	///
	/// Warning this is implementing `Send` when it should not and
	/// should never be use in environment supporting threads.
	pub fn new() -> Self {
		RuntimeInstanceSpawnForceSend(Rc::new(RefCell::new(RuntimeInstanceSpawn::new())))
	}
}

/// Variant to use when the calls do not use the runtime interface.
/// For instance doing a proof verification in wasm.
pub mod hosted_runtime {
	use super::*;
	use sp_core::traits::RuntimeSpawnExt;
	use sp_externalities::ExternalitiesExt;

	/// Alias to an inline implementation that can be use when runtime interface
	/// is skipped.
	pub type HostRuntimeInstanceSpawn = RuntimeInstanceSpawnForceSend<HostLocal>;

	/// Hosted runtime variant of sp_io `RuntimeTasks` `set_capacity`.
	///
	/// Warning this is actually a noops, if at some point there is
	/// hosted threads, it will need the boilerpalte code to call
	/// current externality.
	pub fn host_runtime_tasks_set_capacity(_capacity: u32) {
		// Ignore (this implementation only run inline, so no
		// need to call extension).
	}

	/// Hosted runtime variant of sp_io `RuntimeTasks` `spawn`.
	pub fn host_runtime_tasks_spawn(
		dispatcher_ref: u32,
		entry: u32,
		payload: Vec<u8>,
		kind: u8,
	) -> u64 {
		sp_externalities::with_externalities(|mut ext| {
			let ext_unsafe = ext as *mut dyn Externalities;
			let runtime_spawn = ext.extension::<RuntimeSpawnExt>()
				.expect("Inline runtime extension improperly set.");
			// TODO could wrap ext_unsafe in a ext struct that filter calls to extension of
			// a given id, to make this safer.
			let ext_unsafe: &mut _  = unsafe { &mut *ext_unsafe };
			let result = runtime_spawn.spawn_call(dispatcher_ref, entry, payload, kind, ext_unsafe);
			core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::AcqRel);
			result
		}).unwrap()
	}

	/// Hosted runtime variant of sp_io `RuntimeTasks` `spawn`.
	pub fn host_runtime_tasks_join(handle: u64) -> Option<Vec<u8>> {
		sp_externalities::with_externalities(|mut ext| {
			let ext_unsafe = ext as *mut dyn Externalities;
			let runtime_spawn = ext.extension::<RuntimeSpawnExt>()
				.expect("Inline runtime extension improperly set.");
			// TODO could wrap ext_unsafe in a ext struct that filter calls to extension of
			// a given id, to make this safer.
			let ext_unsafe: &mut _  = unsafe { &mut *ext_unsafe };
			let result = runtime_spawn.join(handle, ext_unsafe);
			core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::AcqRel);
			result
		}).unwrap()
	}

	/// Hosted runtime variant of sp_io `RuntimeTasks` `spawn`.
	pub fn host_runtime_tasks_dismiss(handle: u64) {
		sp_externalities::with_externalities(|mut ext| {
			let runtime_spawn = ext.extension::<RuntimeSpawnExt>()
				.expect("Inline runtime extension improperly set.");
			runtime_spawn.dismiss(handle)
		}).unwrap()
	}
}
