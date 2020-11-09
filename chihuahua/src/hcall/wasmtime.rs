//! Wasmtime host-call interface implementation.
//!
//! ## Authors
//!
//! The Veracruz Development Team.
//!
//! ## Copyright
//!
//! See the file `LICENSE.markdown` in the Veracruz root directory for licensing
//! and copyright information.

#[cfg(any(feature = "std", feature = "tz"))]
use std::sync::Mutex;
#[cfg(feature = "sgx")]
use std::sync::SgxMutex as Mutex;

use std::vec::Vec;

use byteorder::{ByteOrder, LittleEndian};
use wasmtime::{Caller, Extern, ExternType, Func, Instance, Module, Store, Trap, ValType};

use platform_services::{getrandom, result};

use crate::{
    error::common::VeracruzError,
    hcall::common::{
        sha_256_digest, Chihuahua, DataSourceMetadata, EntrySignature, FatalHostError, HCallError,
        HostProvisioningError, HostProvisioningState, LifecycleState, HCALL_GETRANDOM_NAME,
        HCALL_INPUT_COUNT_NAME, HCALL_INPUT_SIZE_NAME, HCALL_READ_INPUT_NAME,
        HCALL_WRITE_OUTPUT_NAME,
    },
};
use std::time::Instant;

////////////////////////////////////////////////////////////////////////////////
// The Wasmtime host provisioning state.
////////////////////////////////////////////////////////////////////////////////

/// The WASMI host provisioning state: the `HostProvisioningState` with the
/// Module and Memory type-variables specialised to WASMI's `ModuleRef` and
/// `MemoryRef` type.
type WasmtimeHostProvisioningState = HostProvisioningState<Vec<u8>, ()>;

lazy_static! {
    static ref HOST_PROVISIONING_STATE: Mutex<WasmtimeHostProvisioningState> =
        Mutex::new(WasmtimeHostProvisioningState::new());
}

/// Initializes the global host provisioning state.
///
/// **Panics** if the initialised host provisioning state is not in
/// `LifecycleState::Initial` immediately after creation or if the global lock
/// cannot be obtained.
pub(crate) fn initialize(expected_data_sources: &[u64], expected_shutdown_sources: &[u64]) {
    let mut guard = HOST_PROVISIONING_STATE
        .lock()
        .expect("Failed to obtain lock on host provisioning state.");

    guard.set_expected_data_sources(expected_data_sources);
    guard.set_expected_shutdown_sources(expected_shutdown_sources);
}

////////////////////////////////////////////////////////////////////////////////
// Constants.
////////////////////////////////////////////////////////////////////////////////

/// The name of the WASM program's entry point.
const ENTRY_POINT_NAME: &'static str = "main";
/// The name of the WASM program's linear memory.
const LINEAR_MEMORY_NAME: &'static str = "memory";

////////////////////////////////////////////////////////////////////////////////
// Checking function well-formedness.
////////////////////////////////////////////////////////////////////////////////

/// Checks whether `main` was declared with `argc` and `argv` or without in the
/// WASM program.
fn check_main(tau: &ExternType) -> EntrySignature {
    match tau {
        ExternType::Func(tau) => {
            let params = tau.params();

            if params == &[ValType::I32, ValType::I32] {
                EntrySignature::ArgvAndArgc
            } else if params == &[] {
                EntrySignature::NoParameters
            } else {
                EntrySignature::NoEntryFound
            }
        }
        _otherwise => EntrySignature::NoEntryFound,
    }
}

////////////////////////////////////////////////////////////////////////////////
// The Wasmtime host provisioning state.
////////////////////////////////////////////////////////////////////////////////

impl WasmtimeHostProvisioningState {
    fn load_program(&mut self, buffer: &[u8]) -> Result<(), HostProvisioningError> {
        if self.get_lifecycle_state() == &LifecycleState::Initial {
            self.set_program_module(buffer.to_vec());
            self.set_program_digest(&sha_256_digest(buffer));

            if self.get_expected_data_source_count() == 0 {
                self.set_ready_to_execute();
            } else {
                self.set_data_sources_loading();
            }
            return Ok(());
        } else {
            self.set_error();
            Err(HostProvisioningError::InvalidLifeCycleState {
                expected: vec![LifecycleState::Initial],
                found: self.get_lifecycle_state().clone(),
            })
        }
    }

    fn write_output(&mut self, caller: Caller, address: i32, size: i32) -> HCallError {
        let start = Instant::now();
        match caller
            .get_export(LINEAR_MEMORY_NAME)
            .and_then(|export| export.into_memory())
        {
            None => Err(FatalHostError::NoMemoryRegistered),
            Some(memory) => {
                let address = address as usize;
                let size = size as usize;
                let mut bytes: Vec<u8> = vec![0; size];

                unsafe {
                    bytes.copy_from_slice(std::slice::from_raw_parts(
                        memory.data_ptr().add(address),
                        size,
                    ))
                };

                /* If a result is already written, signal this to the WASM
                 * program and do not register a new result.  Otherwise,
                 * register the result and signal success.
                 */
                if self.is_result_registered() {
                    Ok(VeracruzError::ResultAlreadyWritten)
                } else {
                    self.set_result(&bytes);
                    println!(
                        ">>> write_output successfully executed in {:?}.",
                        start.elapsed()
                    );
                    Ok(VeracruzError::Success)
                }
            }
        }
    }

    fn input_count(&self, caller: Caller, address: i32) -> HCallError {
        let start = Instant::now();
        match caller
            .get_export(LINEAR_MEMORY_NAME)
            .and_then(|export| export.into_memory())
        {
            Some(memory) => {
                let address = address as usize;
                let result = self.get_current_data_source_count() as u32;

                let mut buffer = [0u8; std::mem::size_of::<u32>()];
                LittleEndian::write_u32(&mut buffer, result);

                unsafe {
                    std::slice::from_raw_parts_mut(
                        memory.data_ptr().add(address),
                        std::mem::size_of::<u32>(),
                    )
                    .copy_from_slice(&buffer)
                };

                println!(
                    ">>> input_count successfully executed in {:?}.",
                    start.elapsed()
                );
                Ok(VeracruzError::Success)
            }
            None => Err(FatalHostError::NoMemoryRegistered),
        }
    }

    fn input_size(&self, caller: Caller, index: i32, address: i32) -> HCallError {
        let start = Instant::now();
        match caller
            .get_export(LINEAR_MEMORY_NAME)
            .and_then(|export| export.into_memory())
        {
            None => Err(FatalHostError::NoMemoryRegistered),
            Some(memory) => {
                let index = index as usize;
                let address = address as usize;

                match self.get_current_data_source(index as usize) {
                    None => return Ok(VeracruzError::BadInput),
                    Some(frame) => {
                        let mut buffer = vec![0u8; std::mem::size_of::<u32>()];
                        LittleEndian::write_u32(&mut buffer, frame.get_data().len() as u32);

                        unsafe {
                            std::slice::from_raw_parts_mut(
                                memory.data_ptr().add(address),
                                std::mem::size_of::<u32>(),
                            )
                            .copy_from_slice(&buffer)
                        };

                        println!(
                            ">>> input_size successfully executed in {:?}.",
                            start.elapsed()
                        );
                        Ok(VeracruzError::Success)
                    }
                }
            }
        }
    }

    fn read_input(&self, caller: Caller, index: i32, address: i32, size: i32) -> HCallError {
        let start = Instant::now();
        match caller
            .get_export(LINEAR_MEMORY_NAME)
            .and_then(|export| export.into_memory())
        {
            None => Err(FatalHostError::NoMemoryRegistered),
            Some(memory) => {
                let address = address as usize;
                let index = index as usize;
                let size = size as usize;

                match self.get_current_data_source(index as usize) {
                    None => {
                        return Ok(VeracruzError::BadInput);
                    }
                    Some(frame) => {
                        let data = frame.get_data();

                        if data.len() > size {
                            Ok(VeracruzError::DataSourceSize)
                        } else {
                            unsafe {
                                std::slice::from_raw_parts_mut(memory.data_ptr().add(address), size)
                                    .copy_from_slice(data)
                            };

                            println!(
                                ">>> read_input successfully executed in {:?}.",
                                start.elapsed()
                            );

                            Ok(VeracruzError::Success)
                        }
                    }
                }
            }
        }
    }

    fn get_random(&self, caller: Caller, address: i32, size: i32) -> HCallError {
        let start = Instant::now();

        match caller
            .get_export(LINEAR_MEMORY_NAME)
            .and_then(|export| export.into_memory())
        {
            None => Err(FatalHostError::NoMemoryRegistered),
            Some(memory) => {
                let address = address as usize;
                let size = size as usize;
                let mut buffer: Vec<u8> = vec![0; size];

                match getrandom(&mut buffer) {
                    result::Result::Success => {
                        unsafe {
                            std::slice::from_raw_parts_mut(memory.data_ptr().add(address), size)
                                .copy_from_slice(&buffer)
                        };
                        println!(
                            ">>> getrandom successfully executed in {:?}.",
                            start.elapsed()
                        );

                        Ok(VeracruzError::Success)
                    }
                    result::Result::Unavailable => Ok(VeracruzError::ServiceUnavailable),
                    result::Result::UnknownError => Ok(VeracruzError::Generic),
                }
            }
        }
    }
}

pub(crate) fn invoke_entry_point() -> Result<i32, Trap> {
    let start = Instant::now();

    let binary;

    {
        let sigma = HOST_PROVISIONING_STATE
            .lock()
            .expect("Failed to obtain lock on host provisioning state.");

        if sigma.get_lifecycle_state() != &LifecycleState::ReadyToExecute {
            return Err(Trap::new(format!(
                "Machine is in state '{}', expecting 'ReadyToExecute'",
                sigma.get_lifecycle_state()
            )));
        }

        binary =
            match sigma.get_program() {
                Some(binary) => binary.clone(),
                None => return Err(Trap::new(
                    "No program module loaded in host provisioning state.  This is a Veracruz bug.",
                )),
            };
    }

    let store = Store::default();

    match Module::new(store.engine(), binary) {
        Err(_err) => return Err(Trap::new("Cannot create WASM module from input binary.")),
        Ok(module) => {
            let mut exports: Vec<Extern> = Vec::new();

            for import in module.imports() {
                if import.module() == "env" {
                    match import.name() {
                        HCALL_GETRANDOM_NAME => {
                            let getrandom =
                                Func::wrap(&store, |caller: Caller, buffer: i32, size: i32| {
                                    let sigma = HOST_PROVISIONING_STATE
                                        .lock()
                                        .expect("Failed to obtain lock on host provisioning state.");

                                    match sigma.get_random(caller, buffer, size) {
                                        Ok(return_code) => Ok(i32::from(return_code)),
                                        Err(reason)     => Err(Trap::new(format!("getrandom failed with error: '{}'.", reason)))
                                    }
                                });

                            exports.push(Extern::Func(getrandom))
                        },
                        HCALL_INPUT_COUNT_NAME => {
                            let input_count =
                                Func::wrap(&store, |caller: Caller, buffer: i32| {
                                    let sigma = HOST_PROVISIONING_STATE
                                        .lock()
                                        .expect("Failed to obtain lock on host provisioning state.");

                                    match sigma.input_count(caller, buffer) {
                                        Ok(return_code) => Ok(i32::from(return_code)),
                                        Err(reason)     => Err(Trap::new(format!("input_count failed with error: '{}'.", reason)))
                                    }
                                });

                            exports.push(Extern::Func(input_count))
                        }
                        HCALL_INPUT_SIZE_NAME => {
                            let input_size =
                                Func::wrap(&store, |caller: Caller, index: i32, buffer: i32| {
                                    let sigma = HOST_PROVISIONING_STATE
                                        .lock()
                                        .expect("Failed to obtain lock on host provisioning state.");

                                    match sigma.input_size(caller, index, buffer) {
                                        Ok(return_code) => Ok(i32::from(return_code)),
                                        Err(reason)     => Err(Trap::new(format!("input_size failed with error: '{}'.", reason)))
                                    }
                                });

                            exports.push(Extern::Func(input_size))
                        },
                        HCALL_READ_INPUT_NAME => {
                            let read_input =
                                Func::wrap(&store, |caller: Caller, index: i32, buffer: i32, size: i32| {
                                    let sigma = HOST_PROVISIONING_STATE
                                        .lock()
                                        .expect("Failed to obtain lock on host provisioning state.");

                                    match sigma.read_input(caller, index, buffer, size) {
                                        Ok(return_code) => Ok(i32::from(return_code)),
                                        Err(reason)     => Err(Trap::new(format!("read_input failed with error: '{}'.", reason)))
                                    }
                                });

                            exports.push(Extern::Func(read_input))
                        },
                        HCALL_WRITE_OUTPUT_NAME => {
                            let write_output =
                                Func::wrap(&store, |caller: Caller, buffer: i32, size: i32| {
                                    let mut sigma = HOST_PROVISIONING_STATE
                                        .lock()
                                        .expect("Failed to obtain lock on host provisioning state.");

                                    match sigma.write_output(caller, buffer, size) {
                                        Ok(return_code) => Ok(i32::from(return_code)),
                                        Err(reason)     => Err(Trap::new(format!("write_output failed with error: '{}'.", reason)))
                                    }
                                });

                            exports.push(Extern::Func(write_output))
                        },
                        otherwise => return Err(Trap::new(format!("Veracruz programs support only the Veracruz host interface.  Unrecognised host call: '{}'.", otherwise)))
                    }
                } else {
                    return Err(Trap::new(format!("Veracruz programs support only the Veracruz host interface.  Unrecognised module import '{}'.", import.name())));
                }
            }

            let instance = Instance::new(&store, &module, &exports).map_err(|err| {
                Trap::new(format!(
                    "Failed to create WASM module.  Error '{}' returned.",
                    err
                ))
            })?;

            match instance.get_export(ENTRY_POINT_NAME) {
                Some(export) => match check_main(&export.ty()) {
                    EntrySignature::ArgvAndArgc => {
                        let main =
                            export
                                .into_func()
                                .expect("Internal invariant failed: entry point not convertible to callable function.")
                                .get2::<i32, i32, i32>()
                                .expect("Internal invariant failed: entry point type-checking bug.");

                        println!(
                            ">>> invoke_main took {:?} to setup pre-main.",
                            start.elapsed()
                        );
                        main(0, 0)
                    }
                    EntrySignature::NoParameters => {
                        let main =
                            export
                                .into_func()
                                .expect("Internal invariant failed: entry point not convertible to callable function.")
                                .get0::<i32>()
                                .expect("Internal invariant failed: entry point type-checking bug.");

                        println!(
                            ">>> invoke_main took {:?} to setup pre-main.",
                            start.elapsed()
                        );
                        main()
                    }
                    EntrySignature::NoEntryFound => {
                        return Err(Trap::new(format!(
                            "Entry point '{}' has a missing or incorrect type signature.",
                            ENTRY_POINT_NAME
                        )))
                    }
                },
                None => return Err(Trap::new("No export with name '{}' in WASM program.")),
            }
        }
    }
}

////////////////////////////////////////////////////////////////////////////////
// The H-call interface.
////////////////////////////////////////////////////////////////////////////////

/// **HACK AHOY!**
///
/// This is just an "empty" struct that's being used more as a marker, than
/// anything else in order to be able to implement WASMI traits which ignore
/// their `self` argument and modify a global constant, instead.
///
/// Yes, this is ugly.  However, there's an irritating difference in the APIs
/// provided by WASMI and Wasmtime for implementing their host states which
/// makes them hard to unify in a nice way.  In particular, WASMI uses traits
/// (the `ModuleImportResolver` and `Externals` traits) to implement the WASM
/// host interface, which means we need to have some type to implement this
/// trait with.  On the other hand, Wasmtime currently works by the host
/// registering callbacks (implementations of the `Fn` trait) that implement
/// each host call. The use of the `Fn` trait is especially problematic, as it
/// means we are unable to mutate a self reference from within the callback
/// body, as this pushes us into the `FnMut` trait (we also run into lifetime
/// issues, as these closures need to modify the `self` parameter of the
/// function within which they were created).  What we can do, instead, is
/// modify a global object hidden behind a mutex in the body of one of these
/// functions without falling foul of the `Fn` constraint.  The following is the
/// hack necessary to allow all this to work uniformly across both backends...
///
/// TODO: revisit all this in the future at some point.
pub(crate) struct DummyWasmtimeHostProvisioningState;

/// Operations on the `DummyWasmtimeHostProvisioningState`.
impl DummyWasmtimeHostProvisioningState {
    /// Creates a new `DummyWasmtimeHostProvisioningState`.
    #[inline]
    pub(crate) fn new() -> Self {
        DummyWasmtimeHostProvisioningState
    }
}

////////////////////////////////////////////////////////////////////////////////
// Chihuahua trait implementation.
////////////////////////////////////////////////////////////////////////////////

/// The `WasmtimeHostProvisioningState` implements everything needed to create a
/// compliant instance of `Chihuahua`.
impl Chihuahua for DummyWasmtimeHostProvisioningState {
    #[inline]
    fn load_program(&mut self, buffer: &[u8]) -> Result<(), HostProvisioningError> {
        HOST_PROVISIONING_STATE
            .lock()
            .expect("Failed to obtain lock on host provisioning state.")
            .load_program(buffer)
    }

    #[inline]
    fn add_new_data_source(
        &mut self,
        metadata: DataSourceMetadata,
    ) -> Result<(), HostProvisioningError> {
        HOST_PROVISIONING_STATE
            .lock()
            .expect("Failed to obtain lock on host provisioning state.")
            .add_new_data_source(metadata)
    }

    #[inline]
    fn invoke_entry_point(&mut self) -> Result<i32, FatalHostError> {
        invoke_entry_point()
            //TODO: Change the error of invoke_entry_point to FatalHostError.
            //      Add better error type to FatalHostErorr.
            .map_err(|e| format!("WASM program issued trap: {}.", e))
            .map_err(|e| {
                FatalHostError::DirectErrorMessage(format!("WASM program issued trap: {}.", e))
            })
            .map(|r| r.clone())
    }

    #[inline]
    fn is_program_registered(&self) -> bool {
        HOST_PROVISIONING_STATE
            .lock()
            .expect("Failed to obtain lock on host provisioning state.")
            .is_program_registered()
            .clone()
    }

    #[inline]
    fn is_result_registered(&self) -> bool {
        HOST_PROVISIONING_STATE
            .lock()
            .expect("Failed to obtain lock on host provisioning state.")
            .is_result_registered()
            .clone()
    }

    #[inline]
    fn is_memory_registered(&self) -> bool {
        HOST_PROVISIONING_STATE
            .lock()
            .expect("Failed to obtain lock on host provisioning state.")
            .is_memory_registered()
            .clone()
    }

    #[inline]
    fn is_able_to_shutdown(&self) -> bool {
        HOST_PROVISIONING_STATE
            .lock()
            .expect("Failed to obtain lock on host provisioning state.")
            .is_able_to_shutdown()
            .clone()
    }

    #[inline]
    fn get_lifecycle_state(&self) -> LifecycleState {
        HOST_PROVISIONING_STATE
            .lock()
            .expect("Failed to obtain lock on host provisioning state.")
            .get_lifecycle_state()
            .clone()
    }

    #[inline]
    fn get_current_data_source_count(&self) -> usize {
        HOST_PROVISIONING_STATE
            .lock()
            .expect("Failed to obtain lock on host provisioning state.")
            .get_current_data_source_count()
            .clone()
    }

    #[inline]
    fn get_expected_data_sources(&self) -> Vec<u64> {
        HOST_PROVISIONING_STATE
            .lock()
            .expect("Failed to obtain lock on host provisioning state.")
            .get_expected_data_sources()
            .clone()
    }

    #[inline]
    fn get_expected_shutdown_sources(&self) -> Vec<u64> {
        HOST_PROVISIONING_STATE
            .lock()
            .expect("Failed to obtain lock on host provisioning state.")
            .get_expected_shutdown_sources()
            .clone()
    }

    #[inline]
    fn get_result(&self) -> Option<Vec<u8>> {
        HOST_PROVISIONING_STATE
            .lock()
            .expect("Failed to obtain lock on host provisioning state.")
            .get_result()
            .map(|d| d.clone())
    }

    #[inline]
    fn get_program_digest(&self) -> Option<Vec<u8>> {
        HOST_PROVISIONING_STATE
            .lock()
            .expect("Failed to obtain lock on host provisioning state.")
            .get_program_digest()
            .map(|d| d.clone())
    }

    #[inline]
    fn set_expected_data_sources(&mut self, sources: &[u64]) {
        HOST_PROVISIONING_STATE
            .lock()
            .expect("Failed to obtain lock on host provisioning state.")
            .set_expected_data_sources(sources);
    }

    #[inline]
    fn invalidate(&mut self) {
        HOST_PROVISIONING_STATE
            .lock()
            .expect("Failed to obtain lock on host provisioning state.")
            .set_error();
    }

    #[inline]
    fn request_shutdown(&mut self, client_id: u64) {
        HOST_PROVISIONING_STATE
            .lock()
            .expect("Failed to obtain lock on host provisioning state.")
            .request_shutdown(client_id);
    }
}
