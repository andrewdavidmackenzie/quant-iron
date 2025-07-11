use crate::{components::state::State, errors::Error};
use dyn_clone::DynClone;
use num_complex::Complex;
use rayon::prelude::*;
use std::{collections::HashSet, fmt::Debug};
#[cfg(feature = "gpu")]
use crate::components::gpu_context::{GPU_CONTEXT, KernelType};
#[cfg(feature = "gpu")]
use ocl::prm::Float2;
#[cfg(feature = "gpu")]
use std::f64::consts::PI;
#[cfg(feature = "gpu")]
use crate::components::gpu_context::GpuKernelArgs;
use crate::compiler::compilable::Compilable;

/// Threshold for using parallel CPU implementation
const PARALLEL_THRESHOLD_NUM_QUBITS: usize = 10;

 /// Threshold for using OpenCL (GPU acceleration)
const OPENCL_THRESHOLD_NUM_QUBITS: usize = 15;

#[cfg(feature = "gpu")]
fn execute_on_gpu(
    state: &State,
    target_qubit: usize,
    control_qubits: &[usize],
    kernel_type: KernelType,
    global_work_size: usize,
    kernel_args: GpuKernelArgs,
) -> Result<Vec<Complex<f64>>, Error> {
    let mut context_guard = GPU_CONTEXT.lock().map_err(|_| Error::GpuContextLockError)?;
    let context = match *context_guard {
        Ok(ref mut ctx) => ctx,
        Err(ref e) => return Err(e.clone()), // Propagate initialisation error
    };

    let num_qubits = state.num_qubits();
    let num_state_elements = state.state_vector.len();

    // Ensure buffers are ready and get mutable references
    let state_buffer_cloned = context.ensure_state_buffer(num_state_elements)?.clone();
    
    let control_qubits_i32: Vec<i32> = control_qubits.iter().map(|&q| q as i32).collect();
    let control_buffer_len = control_qubits_i32.len();
    let control_buffer_cloned = context.ensure_control_buffer(control_buffer_len)?.clone();
    
    let state_vector_f32: Vec<Float2> = state.state_vector.iter()
        .map(|c| Float2::new(c.re as f32, c.im as f32))
        .collect();
    
    // Copy data to GPU buffers
    state_buffer_cloned.write(&state_vector_f32).enq()
        .map_err(|e| Error::OpenCLError(format!("Failed to write to state buffer: {}", e)))?;

    if !control_qubits_i32.is_empty() {
        control_buffer_cloned.write(&control_qubits_i32).enq()
            .map_err(|e| Error::OpenCLError(format!("Failed to write to control buffer: {}", e)))?;
    } else {
        // Write dummy data if no control qubits
        let dummy_control_data = vec![0; 1]; // Dummy data for control buffer
         control_buffer_cloned.write(&dummy_control_data).enq()
            .map_err(|e| Error::OpenCLError(format!("Failed to write to dummy control buffer: {}", e)))?;
    }

    let mut kernel_builder = context.pro_que.kernel_builder(kernel_type.name());
    kernel_builder.global_work_size(global_work_size)
        .arg(&state_buffer_cloned) // Pass by reference
        .arg(num_qubits as i32)
        .arg(target_qubit as i32)
        .arg(control_buffer_cloned)
        .arg(control_qubits_i32.len() as i32);

    match kernel_args {
        GpuKernelArgs::None => {
            // No additional arguments needed for Hadamard, PauliX, PauliY, PauliZ
        }
        GpuKernelArgs::SOrSdag { sign } => {
            kernel_builder.arg(sign);
        }
        GpuKernelArgs::PhaseShift { cos_angle, sin_angle } => {
            kernel_builder.arg(cos_angle).arg(sin_angle);
        }
        GpuKernelArgs::SwapTarget { q1 } => {
            kernel_builder.arg(q1);
        }
        GpuKernelArgs::RotationGate { cos_half_angle, sin_half_angle } => {
            kernel_builder.arg(cos_half_angle).arg(sin_half_angle);
        }
    }

    let kernel = kernel_builder.build()
        .map_err(|e| Error::OpenCLError(format!("Failed to build kernel '{}': {}", kernel_type.name(), e)))?;

    unsafe {
        kernel.enq().map_err(|e| Error::OpenCLError(format!("Failed to enqueue kernel: {}", e)))?;
    }

    let mut state_vector_ocl_result = vec![Float2::new(0.0, 0.0); num_state_elements];
    // Read data back from state_buffer
    state_buffer_cloned.read(&mut state_vector_ocl_result).enq()
        .map_err(|e| Error::OpenCLError(format!("Failed to read state buffer: {}", e)))?;

    Ok(state_vector_ocl_result.iter()
        .map(|f2| Complex::new(f2[0] as f64, f2[1] as f64))
        .collect())
}

/// A trait defining the interface for all operators.
pub trait Operator: Send + Sync + Debug + DynClone {
    /// Applies the operator to the given state's target qubits, using the control qubits if required.
    ///
    /// # Arguments:
    ///
    /// * `state` - The state to apply the operator to.
    ///
    /// * `target_qubits` - The target qubits to apply the operator to. If no target qubits are specified, the operator will be applied to all qubits in the state.
    ///
    /// * `control_qubits` - The control qubits to apply the operator to.
    ///
    /// # Returns:
    ///
    /// * The new state after applying the operator.
    fn apply(
        &self,
        state: &State,
        target_qubits: &[usize],
        control_qubits: &[usize],
    ) -> Result<State, Error>;

    /// Returns the number of qubits that the operator acts on.
    ///
    /// # Returns:
    ///
    /// * The number of qubits that the operator acts on.
    fn base_qubits(&self) -> usize;

    /// Optionally returns an intermediate representation of the operator for compilation to OpenQASM.
    /// 
    /// If you are not planning to compile the operator to an IR, you can ignore this method.
    /// If you want to compile the operator to QASM, you should implement this method.
    /// 
    /// # Returns:
    ///  * An optional vector of `InstructionIR` representing the operator in an intermediate representation.
    fn to_compilable(&self) -> Option<&dyn Compilable> {
        // Default implementation returns None, indicating no compilable representation
        None
    }
}

dyn_clone::clone_trait_object!(Operator);

/// Helper function to check if all control qubits are in the |1> state for a given basis state index.
fn check_controls(index: usize, control_qubits: &[usize]) -> bool {
    control_qubits
        .iter()
        .all(|&qubit| (index >> qubit) & 1 == 1)
}

/// Helper function to validate target and control qubits
///
/// # Arguments:
///
/// * `state` - The quantum state that contains information about the number of qubits.
/// * `target_qubits` - The target qubits to validate.
/// * `control_qubits` - The control qubits to validate.
/// * `expected_targets` - The expected number of target qubits.
///
/// # Returns:
///
/// * `Ok(())` if all validations pass.
/// * `Err(Error)` if any validation fails.
fn validate_qubits(
    state: &State,
    target_qubits: &[usize],
    control_qubits: &[usize],
    expected_targets: usize,
) -> Result<(), Error> {
    // Check if we have the expected number of target qubits
    if target_qubits.len() != expected_targets {
        return Err(Error::InvalidNumberOfQubits(target_qubits.len()));
    }

    let num_qubits = state.num_qubits();

    // Check if all target qubits are valid indices
    for &target_qubit in target_qubits {
        if target_qubit >= num_qubits {
            return Err(Error::InvalidQubitIndex(target_qubit, num_qubits));
        }
    }

    // Check if all control qubits are valid indices and don't overlap with target qubits
    for &control_qubit in control_qubits {
        if control_qubit >= num_qubits {
            return Err(Error::InvalidQubitIndex(control_qubit, num_qubits));
        }

        for &target_qubit in target_qubits {
            if control_qubit == target_qubit {
                return Err(Error::OverlappingControlAndTargetQubits(
                    control_qubit,
                    target_qubit,
                ));
            }
        }
    }

    // Special check for multiple target qubits to ensure no duplicates
    if expected_targets > 1 {
        if num_qubits >= PARALLEL_THRESHOLD_NUM_QUBITS {
            // Use HashSet for efficient duplicate detection in larger systems
            let mut seen_targets: HashSet<usize> = HashSet::with_capacity(target_qubits.len());
            for &target_qubit in target_qubits {
                if !seen_targets.insert(target_qubit) {
                    return Err(Error::InvalidQubitIndex(target_qubit, num_qubits));
                }
            }
        } else {
            // Use nested loops for smaller systems
            for i in 0..target_qubits.len() {
                for j in i + 1..target_qubits.len() {
                    if target_qubits[i] == target_qubits[j] {
                        return Err(Error::InvalidQubitIndex(target_qubits[i], num_qubits));
                    }
                }
            }
        }
    }

    Ok(())
}

/// Defines a Hadamard operator.
///
/// A single-qubit operator that transforms the state of a qubit into a superposition of its basis states.
#[derive(Debug, Clone, Copy)]
pub struct Hadamard;

impl Operator for Hadamard {
    /// Applies the Hadamard operator to the given state's target qubit.
    ///
    /// # Arguments:
    ///
    /// * `state` - The state to apply the operator to.
    ///
    /// * `target_qubits` - The target qubits to apply the operator to. This should be a single qubit.
    ///
    /// * `control_qubits` - The control qubits for the operator. If not empty, the operator will be applied conditionally based on the control qubits. Otherwise, it will be applied unconditionally.
    ///
    /// # Returns:
    ///
    /// * The new state after applying the Hadamard operator.
    ///
    /// # Errors:
    ///
    /// * `Error::InvalidNumberOfQubits` - If the target qubits is not 1.
    ///
    /// * `Error::InvalidQubitIndex` - If the target qubit or control qubit index is invalid for the number of qubits in the state.
    ///
    /// * `Error::OverlappingControlAndTargetQubits` - If the control qubit and target qubit indices overlap.
    fn apply(
        &self,
        state: &State,
        target_qubits: &[usize],
        control_qubits: &[usize],
    ) -> Result<State, Error> {
        // Validation
        validate_qubits(state, target_qubits, control_qubits, 1)?;

        let target_qubit: usize = target_qubits[0];
        let num_qubits: usize = state.num_qubits();

        // Apply potentially controlled Hadamard operator
        let sqrt_2_inv: f64 = 1.0 / (2.0f64).sqrt();
        let dim: usize = 1 << num_qubits;
        #[allow(unused_assignments)]
        let mut new_state_vec: Vec<Complex<f64>> = state.state_vector.clone();
        let gpu_enabled: bool = cfg!(feature = "gpu");

        if num_qubits >= OPENCL_THRESHOLD_NUM_QUBITS && gpu_enabled {
            #[cfg(feature = "gpu")]
            {
                let global_work_size = if num_qubits > 0 { 1 << (num_qubits - 1) } else { 1 };
                new_state_vec = execute_on_gpu(
                    state,
                    target_qubit,
                    control_qubits,
                    KernelType::Hadamard,
                    global_work_size,
                    GpuKernelArgs::None,
                )?;
            }
        } else if num_qubits >= PARALLEL_THRESHOLD_NUM_QUBITS {
            // Rayon CPU Parallel implementation
            new_state_vec = state.state_vector.clone(); // Initialise for CPU path
            if control_qubits.is_empty() {
                // Parallel uncontrolled Hadamard
                let updates: Vec<(usize, Complex<f64>)> = (0..(1 << (num_qubits - 1)))
                    .into_par_iter()
                    .flat_map(|k| {
                        let i0 = (k >> target_qubit << (target_qubit + 1))
                            | (k & ((1 << target_qubit) - 1));
                        let i1 = i0 | (1 << target_qubit);
                        let amp0 = state.state_vector[i0];
                        let amp1 = state.state_vector[i1];
                        vec![
                            (i0, sqrt_2_inv * (amp0 + amp1)),
                            (i1, sqrt_2_inv * (amp0 - amp1)),
                        ]
                    })
                    .collect();
                for (idx, val) in updates {
                    new_state_vec[idx] = val;
                }
            } else {
                // Rayon CPU Parallel controlled Hadamard
                let updates: Vec<(usize, Complex<f64>)> = (0..dim)
                    .into_par_iter()
                    .filter_map(|i| {
                        if (i >> target_qubit) & 1 == 0 { // Process pairs (i, j) where i has 0 at target_qubit
                            let j = i | (1 << target_qubit); // j has 1 at target_qubit
                            if check_controls(i, control_qubits) { // Check controls based on i
                                let amp_i = state.state_vector[i];
                                let amp_j = state.state_vector[j];
                                Some(vec![
                                    (i, sqrt_2_inv * (amp_i + amp_j)),
                                    (j, sqrt_2_inv * (amp_i - amp_j)),
                                ])
                            } else {
                                None // Controls not met for this pair
                            }
                        } else {
                            None // Already processed as part of a pair starting with 0 at target_qubit
                        }
                    })
                    .flatten()
                    .collect();
                for (idx, val) in updates {
                    new_state_vec[idx] = val;
                }
            }
        } else {
            // Sequential CPU implementation
            new_state_vec = state.state_vector.clone(); // initialise for CPU path
            if control_qubits.is_empty() {
                // Sequential uncontrolled Hadamard
                for k in 0..(1 << (num_qubits - 1)) {
                    let i0 =
                        (k >> target_qubit << (target_qubit + 1)) | (k & ((1 << target_qubit) - 1));
                    let i1 = i0 | (1 << target_qubit);
                    let amp0 = state.state_vector[i0];
                    let amp1 = state.state_vector[i1];
                    new_state_vec[i0] = sqrt_2_inv * (amp0 + amp1);
                    new_state_vec[i1] = sqrt_2_inv * (amp0 - amp1);
                }
            } else {
                // Sequential controlled Hadamard
                for i in 0..dim {
                    if (i >> target_qubit) & 1 == 0 {
                        let j = i | (1 << target_qubit);
                        if check_controls(i, control_qubits) {
                            let amp_i = state.state_vector[i];
                            let amp_j = state.state_vector[j];
                            new_state_vec[i] = sqrt_2_inv * (amp_i + amp_j);
                            new_state_vec[j] = sqrt_2_inv * (amp_i - amp_j);
                        }
                    }
                }
            }
        }

        Ok(State {
            state_vector: new_state_vec,
            num_qubits,
        })
    }

    fn base_qubits(&self) -> usize {
        1 // Hadamard acts on 1 qubit only
    }

    fn to_compilable(&self) -> Option<&dyn Compilable> {
        Some(self)
    }
}

/// Defines the Pauli operators: X, Y, Z.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pauli {
    /// Pauli-X operator (NOT gate)
    X,
    /// Pauli-Y operator
    Y,
    /// Pauli-Z operator
    Z,
}

impl Operator for Pauli {
    /// Applies the Pauli operator to the given state's target qubit.
    ///
    /// # Arguments:
    ///
    /// * `state` - The state to apply the operator to.
    ///
    /// * `target_qubits` - The target qubits to apply the operator to. This should be a single qubit.
    ///
    /// * `control_qubits` - The control qubits for the operator. If not empty, the operator will be applied conditionally based on the control qubits. Otherwise, it will be applied unconditionally.
    ///
    /// # Returns:
    ///
    /// * The new state after applying the Pauli operator.
    ///
    /// # Errors:
    ///
    /// * `Error::InvalidNumberOfQubits` - If the target qubits is not 1.
    ///
    /// * `Error::InvalidQubitIndex` - If the target qubit index is invalid for the number of qubits in the state.
    ///
    /// * `Error::OverlappingControlAndTargetQubits` - If the control qubit and target qubit indices overlap.
    fn apply(
        &self,
        state: &State,
        target_qubits: &[usize],
        control_qubits: &[usize],
    ) -> Result<State, Error> {
        // Validation
        validate_qubits(state, target_qubits, control_qubits, 1)?;

        let target_qubit: usize = target_qubits[0];
        let num_qubits: usize = state.num_qubits();

        // Apply potentially controlled Pauli operator
        let dim: usize = 1 << num_qubits;
        let mut new_state_vec: Vec<Complex<f64>> = state.state_vector.clone();
        let i_complex: Complex<f64> = Complex::new(0.0, 1.0);
        let gpu_enabled: bool = cfg!(feature = "gpu");

        if num_qubits >= OPENCL_THRESHOLD_NUM_QUBITS && gpu_enabled {
            #[cfg(feature = "gpu")]
            {
                let kernel_type = match self {
                    Pauli::X => KernelType::PauliX,
                    Pauli::Y => KernelType::PauliY,
                    Pauli::Z => KernelType::PauliZ,
                };
                let global_work_size = if num_qubits == 0 {
                    1
                } else {
                    match self {
                        Pauli::Z => 1 << num_qubits, // N work items for Pauli Z
                        _ => 1 << (num_qubits - 1),   // N/2 work items for Pauli X, Y
                    }
                };
                new_state_vec = execute_on_gpu(
                    state,
                    target_qubit,
                    control_qubits,
                    kernel_type,
                    global_work_size,
                    GpuKernelArgs::None,
                )?;
            }
        } else if num_qubits >= PARALLEL_THRESHOLD_NUM_QUBITS {
            // Parallel implementation
            match self {
                Pauli::X => {
                    let updates: Vec<(usize, Complex<f64>)> = (0..dim)
                        .into_par_iter()
                        .filter_map(|i| {
                            if check_controls(i, control_qubits) && ((i >> target_qubit) & 1 == 0) {
                                let j = i | (1 << target_qubit);
                                let amp_i = state.state_vector[i];
                                let amp_j = state.state_vector[j];
                                Some(vec![(i, amp_j), (j, amp_i)])
                            } else {
                                None
                            }
                        })
                        .flatten()
                        .collect();
                    for (idx, val) in updates {
                        new_state_vec[idx] = val;
                    }
                }
                Pauli::Y => {
                    let updates: Vec<(usize, Complex<f64>)> = (0..dim)
                        .into_par_iter()
                        .filter_map(|i| {
                            if check_controls(i, control_qubits) && ((i >> target_qubit) & 1 == 0) {
                                let j = i | (1 << target_qubit);
                                let amp_i = state.state_vector[i];
                                let amp_j = state.state_vector[j];
                                Some(vec![(i, -i_complex * amp_j), (j, i_complex * amp_i)])
                            } else {
                                None
                            }
                        })
                        .flatten()
                        .collect();
                    for (idx, val) in updates {
                        new_state_vec[idx] = val;
                    }
                }
                Pauli::Z => {
                    new_state_vec
                        .par_iter_mut()
                        .enumerate()
                        .for_each(|(i, current_amp_ref)| {
                            if check_controls(i, control_qubits) && ((i >> target_qubit) & 1 == 1) {
                                *current_amp_ref = -state.state_vector[i];
                            }
                        });
                }
            }
        } else {
            // Sequential implementation
            for i in 0..dim {
                if check_controls(i, control_qubits) {
                    match self {
                        Pauli::X => {
                            if (i >> target_qubit) & 1 == 0 {
                                let j = i | (1 << target_qubit);
                                let amp_i = state.state_vector[i];
                                let amp_j = state.state_vector[j];
                                new_state_vec[i] = amp_j;
                                new_state_vec[j] = amp_i;
                            }
                        }
                        Pauli::Y => {
                            if (i >> target_qubit) & 1 == 0 {
                                let j = i | (1 << target_qubit);
                                let amp_i = state.state_vector[i];
                                let amp_j = state.state_vector[j];
                                new_state_vec[i] = -i_complex * amp_j;
                                new_state_vec[j] = i_complex * amp_i;
                            }
                        }
                        Pauli::Z => {
                            if (i >> target_qubit) & 1 == 1 {
                                new_state_vec[i] = -state.state_vector[i];
                            }
                        }
                    }
                }
            }
        }

        Ok(State {
            state_vector: new_state_vec,
            num_qubits: state.num_qubits(),
        })
    }

    fn base_qubits(&self) -> usize {
        1 // Pauli operators act on 1 qubit only
    }

    fn to_compilable(&self) -> Option<&dyn Compilable> {
        Some(self) // Manual implementation for enum
    }
}

impl std::fmt::Display for Pauli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Pauli::X => write!(f, "X"),
            Pauli::Y => write!(f, "Y"),
            Pauli::Z => write!(f, "Z"),
        }
    }
}

/// Defines a CNOT operator.
///
/// A two-qubit operator that flips the target qubit if the control qubit is in the |1> state.
#[derive(Debug, Clone, Copy)]
pub struct CNOT;

impl Operator for CNOT {
    /// Applies the CNOT operator to the given state's target qubit, using the control qubit.
    ///
    /// # Arguments:
    ///
    /// * `state` - The state to apply the operator to.
    ///
    /// * `target_qubits` - The target qubits to apply the operator to. This should be a single qubit.
    ///
    /// * `control_qubits` - The control qubits for the operator. This should be a single qubit.
    ///
    /// # Returns:
    ///
    /// * The new state after applying the CNOT operator.
    ///
    /// # Errors:
    ///
    /// * `Error::InvalidNumberOfQubits` - If the target or control qubits is not 1.
    ///
    /// * `Error::InvalidQubitIndex` - If the target or control qubit index is invalid for the number of qubits in the state.
    ///
    /// * `Error::OverlappingControlAndTargetQubits` - If the control qubit and target qubit indices overlap.
    fn apply(
        &self,
        state: &State,
        target_qubits: &[usize],
        control_qubits: &[usize],
    ) -> Result<State, Error> {
        // Validation
        validate_qubits(state, target_qubits, control_qubits, 1)?;

        // Additional validation for CNOT: exactly one control qubit
        if control_qubits.len() != 1 {
            return Err(Error::InvalidNumberOfQubits(control_qubits.len()));
        }

        let control_qubit: usize = control_qubits[0];

        // Apply CNOT operator (same as Pauli-X with 1 control qubit)
        Pauli::X.apply(state, target_qubits, &[control_qubit])
    }

    fn base_qubits(&self) -> usize {
        2 // CNOT acts on 2 qubits (1 control, 1 target)
    }

    fn to_compilable(&self) -> Option<&dyn Compilable> {
        Some(self)
    }
}

/// Defines a SWAP operator.
///
/// A two-qubit operator that swaps the states of the two qubits.
#[derive(Debug, Clone, Copy)]
pub struct SWAP;

impl Operator for SWAP {
    /// Applies the SWAP operator to the given state's target qubits.
    ///
    /// # Arguments:
    ///
    /// * `state` - The state to apply the operator to.
    ///
    /// * `target_qubits` - The target qubits to apply the operator to. This should be two qubits.
    ///
    /// * `control_qubits` - The control qubits. If empty, the swap is unconditional. Otherwise, the swap occurs only if all control qubits are |1> for the relevant basis states.
    /// # Returns:
    ///
    /// * The new state after applying the SWAP operator.
    ///
    /// # Errors:
    ///
    /// * `Error::InvalidNumberOfQubits` - If the target qubits are not 2 different qubits.
    ///
    /// * `Error::InvalidQubitIndex` - If the target qubit indices are invalid for the number of qubits in the state.
    ///
    /// * `Error::InvalidQubitIndex` - If the target qubit indices are not different.
    ///
    /// * `Error::OverlappingControlAndTargetQubits` - If the control qubit and target qubit indices overlap.
    fn apply(
        &self,
        state: &State,
        target_qubits: &[usize],
        control_qubits: &[usize],
    ) -> Result<State, Error> {
        // Validation
        validate_qubits(state, target_qubits, control_qubits, 2)?;

        let target_qubit_1: usize = target_qubits[0];
        let target_qubit_2: usize = target_qubits[1];
        let num_qubits: usize = state.num_qubits();

        // Apply potentially controlled SWAP operator
        let dim: usize = 1 << num_qubits;
        #[allow(unused_assignments)] // new_state_vec might be reassigned by GPU path
        let mut new_state_vec = state.state_vector.clone(); // Start with a copy
        let gpu_enabled: bool = cfg!(feature = "gpu");

        if num_qubits >= OPENCL_THRESHOLD_NUM_QUBITS && gpu_enabled {
            #[cfg(feature = "gpu")]
            {
                // For SWAP, global_work_size is 2^(N-2) because each work item handles
                // a pair of states differing at target_qubit_1 and target_qubit_2.
                // The kernel iterates over the 2^(N-2) combinations of other qubits.
                let global_work_size = if num_qubits >= 2 { 1 << (num_qubits - 2) } else { 1 }; // Handle N=0,1 edge cases for work size
                new_state_vec = execute_on_gpu(
                    state,
                    target_qubit_1, // target_a in kernel
                    control_qubits,
                    KernelType::Swap,
                    global_work_size,
                    GpuKernelArgs::SwapTarget { q1: target_qubit_2 as i32 }, // target_b in kernel
                )?;
            }
        } else if num_qubits >= PARALLEL_THRESHOLD_NUM_QUBITS {
            // Parallel implementation
            let updates: Vec<(usize, Complex<f64>)> = (0..dim)
                .into_par_iter()
                .filter_map(|i| {
                    let target_bit_1 = (i >> target_qubit_1) & 1;
                    let target_bit_2 = (i >> target_qubit_2) & 1;

                    if target_bit_1 != target_bit_2 {
                        let j = i ^ (1 << target_qubit_1) ^ (1 << target_qubit_2);
                        if i < j && check_controls(i, control_qubits) {
                            let amp_i = state.state_vector[i];
                            let amp_j = state.state_vector[j];
                            Some(vec![(i, amp_j), (j, amp_i)])
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
                .flatten()
                .collect();
            for (idx, val) in updates {
                new_state_vec[idx] = val;
            }
        } else {
            // Sequential implementation
            for i in 0..dim {
                let target_bit_1 = (i >> target_qubit_1) & 1;
                let target_bit_2 = (i >> target_qubit_2) & 1;

                if target_bit_1 != target_bit_2 {
                    let j = i ^ (1 << target_qubit_1) ^ (1 << target_qubit_2);
                    if i < j {
                        if check_controls(i, control_qubits) {
                            let amp_i = state.state_vector[i];
                            let amp_j = state.state_vector[j];
                            new_state_vec[i] = amp_j;
                            new_state_vec[j] = amp_i;
                        }
                    }
                }
            }
        }

        Ok(State {
            state_vector: new_state_vec,
            num_qubits: state.num_qubits(),
        })
    }

    fn base_qubits(&self) -> usize {
        2 // SWAP acts on 2 qubits
    }

    fn to_compilable(&self) -> Option<&dyn Compilable> {
        Some(self)
    }
}

/// Defines a Toffoli operator.
///
/// A three-qubit operator that flips the target qubit if both control qubits are in the |1> state. Also known as CCNOT (Controlled-Controlled-NOT).
#[derive(Debug, Clone, Copy)]
pub struct Toffoli;

impl Operator for Toffoli {
    /// Applies the Toffoli operator to the given state's target qubit, using the control qubits.
    ///
    /// # Arguments:
    ///
    /// * `state` - The state to apply the operator to.
    ///
    /// * `target_qubits` - The target qubit to apply the operator to. This should be a single qubit.
    ///
    /// * `control_qubits` - The control qubits for the operator. This should be two qubits.
    ///
    /// # Returns:
    ///
    /// * The new state after applying the Toffoli operator.
    ///
    /// # Errors:
    ///
    /// * `Error::InvalidNumberOfQubits` - If the target or control qubits are not 1 and 2 respectively, or if the control qubits are not different.
    ///
    /// * `Error::InvalidQubitIndex` - If the target or control qubit indices are invalid for the number of qubits in the state.
    ///
    /// * `Error::OverlappingControlAndTargetQubits` - If the control qubit and target qubit indices overlap.
    fn apply(
        &self,
        state: &State,
        target_qubits: &[usize],
        control_qubits: &[usize],
    ) -> Result<State, Error> {
        // Validation
        validate_qubits(state, target_qubits, control_qubits, 1)?;

        // Additional validation for Toffoli: exactly two control qubits
        if control_qubits.len() != 2 {
            return Err(Error::InvalidNumberOfQubits(control_qubits.len()));
        }

        // Additional validation for Toffoli: control qubits must be different
        if control_qubits[0] == control_qubits[1] {
            return Err(Error::InvalidNumberOfQubits(control_qubits.len()));
        }

        Pauli::X.apply(state, target_qubits, control_qubits)
    }

    fn base_qubits(&self) -> usize {
        3 // Toffoli acts on 3 qubits (2 control, 1 target)
    }

    fn to_compilable(&self) -> Option<&dyn Compilable> {
        Some(self)
    }
}

/// Defines an identity operator
///
/// A single-qubit operator that does not change the state of the qubit.
#[derive(Debug, Clone, Copy)]
pub struct Identity;

impl Operator for Identity {
    /// Applies the identity operator to the given state's target qubit.
    ///
    /// # Arguments:
    ///
    /// * `state` - The state to apply the operator to.
    ///
    /// * `target_qubits` - The target qubits to apply the operator to. This should be a single qubit.
    ///
    /// * `control_qubits` - The control qubits for the operator. If not empty, the operator will be applied conditionally based on the control qubits. Otherwise, it will be applied unconditionally.
    ///
    /// # Returns:
    ///
    /// * The new state after applying the identity operator.
    fn apply(
        &self,
        state: &State,
        target_qubits: &[usize],
        control_qubits: &[usize],
    ) -> Result<State, Error> {
        // Validation
        validate_qubits(state, target_qubits, control_qubits, 1)?;

        // Apply identity operator (no change)
        Ok(state.clone())
    }

    fn base_qubits(&self) -> usize {
        1 // Identity acts on 1 qubit only
    }

    fn to_compilable(&self) -> Option<&dyn Compilable> {
        Some(self)
    }
}

/// Defines a Phase S operator.
///
/// A single-qubit operator that applies a phase shift to the |1> state. Also known as the S gate or Phase gate.
#[derive(Debug, Clone, Copy)]
pub struct PhaseS;

impl Operator for PhaseS {
    /// Applies the Phase S operator to the given state's target qubit.
    ///
    /// # Arguments:
    ///
    /// * `state` - The state to apply the operator to.
    ///
    /// * `target_qubits` - The target qubits to apply the operator to. This should be a single qubit.
    ///
    /// * `control_qubits` - The control qubits for the operator. If not empty, the operator will be applied conditionally based on the control qubits. Otherwise, it will be applied unconditionally.
    ///
    /// # Returns:
    ///
    /// * The new state after applying the Phase S operator.
    fn apply(
        &self,
        state: &State,
        target_qubits: &[usize],
        control_qubits: &[usize],
    ) -> Result<State, Error> {
        // Validation
        validate_qubits(state, target_qubits, control_qubits, 1)?;

        let target_qubit: usize = target_qubits[0];
        let num_qubits: usize = state.num_qubits();

        // Apply potentially controlled Phase S operator
        #[allow(unused_assignments)]
        let mut new_state_vec: Vec<Complex<f64>> = state.state_vector.clone();
        let gpu_enabled: bool = cfg!(feature = "gpu");

        if num_qubits >= OPENCL_THRESHOLD_NUM_QUBITS && gpu_enabled {
            #[cfg(feature = "gpu")]
            {
                let global_work_size = 1 << num_qubits;
                new_state_vec = execute_on_gpu(
                    state,
                    target_qubit,
                    control_qubits,
                    KernelType::PhaseSOrSdag,
                    global_work_size,
                    GpuKernelArgs::SOrSdag { sign: 1.0f32 },
                )?;
            }
        } else if num_qubits >= PARALLEL_THRESHOLD_NUM_QUBITS {
            let phase_factor = Complex::new(0.0, 1.0); // Phase shift of pi/2 (i)
            new_state_vec = state.state_vector.clone(); // Ensure cloned for CPU path
            new_state_vec
                .par_iter_mut()
                .enumerate()
                .for_each(|(i, current_amp_ref)| {
                    if ((i >> target_qubit) & 1 == 1) && check_controls(i, control_qubits) {
                        *current_amp_ref = state.state_vector[i] * phase_factor;
                    }
                });
        } else {
            let phase_factor = Complex::new(0.0, 1.0); // Phase shift of pi/2 (i)
            new_state_vec = state.state_vector.clone(); // Ensure cloned for CPU path
            let dim: usize = 1 << num_qubits;
            for i in 0..dim {
                let target_bit_is_one = (i >> target_qubit) & 1 == 1;
                if target_bit_is_one && check_controls(i, control_qubits) {
                    new_state_vec[i] = state.state_vector[i] * phase_factor;
                }
            }
        }

        Ok(State {
            state_vector: new_state_vec,
            num_qubits,
        })
    }

    fn base_qubits(&self) -> usize {
        1 // Phase S acts on 1 qubit only
    }

    fn to_compilable(&self) -> Option<&dyn Compilable> {
        Some(self)
    }
}

/// Defines a Phase T operator.
///
/// A single-qubit operator that applies a phase shift to the |1> state. Also known as the T gate or π/8 gate.
#[derive(Debug, Clone, Copy)]
pub struct PhaseT;

impl Operator for PhaseT {
    /// Applies the Phase T operator to the given state's target qubit.
    ///
    /// # Arguments:
    ///
    /// * `state` - The state to apply the operator to.
    ///
    /// * `target_qubits` - The target qubits to apply the operator to. This should be a single qubit.
    ///
    /// * `control_qubits` - The control qubits for the operator. If not empty, the operator will be applied conditionally based on the control qubits. Otherwise, it will be applied unconditionally.
    ///
    /// # Returns:
    ///
    /// * The new state after applying the Phase T operator.
    fn apply(
        &self,
        state: &State,
        target_qubits: &[usize],
        control_qubits: &[usize],
    ) -> Result<State, Error> {
        // Validation
        validate_qubits(state, target_qubits, control_qubits, 1)?;

        let target_qubit = target_qubits[0];
        let num_qubits = state.num_qubits();

        // Apply potentially controlled Phase T operator
        #[allow(unused_assignments)]
        let mut new_state_vec: Vec<Complex<f64>> = state.state_vector.clone();
        let gpu_enabled: bool = cfg!(feature = "gpu");

        if num_qubits >= OPENCL_THRESHOLD_NUM_QUBITS && gpu_enabled {
            #[cfg(feature = "gpu")]
            {
                let global_work_size = 1 << num_qubits;
                let angle = PI / 4.0;
                new_state_vec = execute_on_gpu(
                    state,
                    target_qubit,
                    control_qubits,
                    KernelType::PhaseShift,
                    global_work_size,
                    GpuKernelArgs::PhaseShift {
                        cos_angle: angle.cos() as f32,
                        sin_angle: angle.sin() as f32,
                    },
                )?;
            }
        } else if num_qubits >= PARALLEL_THRESHOLD_NUM_QUBITS {
            let invsqrt2: f64 = 1.0 / (2.0f64).sqrt();
            let phase_factor = Complex::new(invsqrt2, invsqrt2); // Phase shift of pi/4 (exp(i*pi/4))
            new_state_vec = state.state_vector.clone(); // Ensure cloned for CPU path
            new_state_vec
                .par_iter_mut()
                .enumerate()
                .for_each(|(i, current_amp_ref)| {
                    if ((i >> target_qubit) & 1 == 1) && check_controls(i, control_qubits) {
                        *current_amp_ref = state.state_vector[i] * phase_factor;
                    }
                });
        } else {
            let invsqrt2: f64 = 1.0 / (2.0f64).sqrt();
            let phase_factor = Complex::new(invsqrt2, invsqrt2); // Phase shift of pi/4 (exp(i*pi/4))
            new_state_vec = state.state_vector.clone(); // Ensure cloned for CPU path
            let dim: usize = 1 << num_qubits;
            for i in 0..dim {
                let target_bit_is_one = (i >> target_qubit) & 1 == 1;
                if target_bit_is_one && check_controls(i, control_qubits) {
                    new_state_vec[i] = state.state_vector[i] * phase_factor;
                }
            }
        }

        Ok(State {
            state_vector: new_state_vec,
            num_qubits,
        })
    }

    fn base_qubits(&self) -> usize {
        1 // Phase T acts on 1 qubit only
    }

    fn to_compilable(&self) -> Option<&dyn Compilable> {
        Some(self)
    }
}

/// Defines a Phase Sdag operator.
///
/// A single-qubit operator that applies a phase shift to the |1> state. Also known as the S† gate or Phase† gate. Inverse of S gate.
#[derive(Debug, Clone, Copy)]
pub struct PhaseSdag;

impl Operator for PhaseSdag {
    /// Applies the Phase Sdag operator to the given state's target qubit.
    ///
    /// # Arguments:
    ///
    /// * `state` - The state to apply the operator to.
    ///
    /// * `target_qubits` - The target qubits to apply the operator to. This should be a single qubit.
    ///
    /// * `control_qubits` - The control qubits for the operator. If not empty, the operator will be applied conditionally based on the control qubits. Otherwise, it will be applied unconditionally.
    ///
    /// # Returns:
    ///
    /// * The new state after applying the Phase Sdag operator.
    fn apply(
        &self,
        state: &State,
        target_qubits: &[usize],
        control_qubits: &[usize],
    ) -> Result<State, Error> {
        // Validation
        validate_qubits(state, target_qubits, control_qubits, 1)?;

        let target_qubit = target_qubits[0];
        let num_qubits = state.num_qubits();

        // Apply potentially controlled Phase Sdag operator
        #[allow(unused_assignments)]
        let mut new_state_vec: Vec<Complex<f64>> = state.state_vector.clone();
        let gpu_enabled: bool = cfg!(feature = "gpu");

        if num_qubits >= OPENCL_THRESHOLD_NUM_QUBITS && gpu_enabled {
            #[cfg(feature = "gpu")]
            {
                let global_work_size = 1 << num_qubits;
                new_state_vec = execute_on_gpu(
                    state,
                    target_qubit,
                    control_qubits,
                    KernelType::PhaseSOrSdag,
                    global_work_size,
                    GpuKernelArgs::SOrSdag { sign: -1.0f32 },
                )?;
            }
        } else if num_qubits >= PARALLEL_THRESHOLD_NUM_QUBITS {
            let phase_factor = Complex::new(0.0, -1.0); // Phase shift of -pi/2 (-i)
            new_state_vec = state.state_vector.clone(); // Ensure cloned for CPU path
            new_state_vec
                .par_iter_mut()
                .enumerate()
                .for_each(|(i, current_amp_ref)| {
                    if ((i >> target_qubit) & 1 == 1) && check_controls(i, control_qubits) {
                        *current_amp_ref = state.state_vector[i] * phase_factor;
                    }
                });
        } else {
            let phase_factor = Complex::new(0.0, -1.0); // Phase shift of -pi/2 (-i)
            new_state_vec = state.state_vector.clone(); // Ensure cloned for CPU path
            let dim: usize = 1 << num_qubits;
            for i in 0..dim {
                let target_bit_is_one = (i >> target_qubit) & 1 == 1;
                if target_bit_is_one && check_controls(i, control_qubits) {
                    new_state_vec[i] = state.state_vector[i] * phase_factor;
                }
            }
        }

        Ok(State {
            state_vector: new_state_vec,
            num_qubits,
        })
    }

    fn base_qubits(&self) -> usize {
        1 // Phase Sdag acts on 1 qubit only
    }

    fn to_compilable(&self) -> Option<&dyn Compilable> {
        Some(self)
    }
}

/// Defines a Phase Tdag operator.
///
/// A single-qubit operator that applies a phase shift to the |1> state. Also known as the T† gate or π/8† gate. Inverse of T gate.
#[derive(Debug, Clone, Copy)]
pub struct PhaseTdag;

impl Operator for PhaseTdag {
    /// Applies the Phase Tdag operator to the given state's target qubit.
    ///
    /// # Arguments:
    ///
    /// * `state` - The state to apply the operator to.
    ///
    /// * `target_qubits` - The target qubits to apply the operator to. This should be a single qubit.
    ///
    /// * `control_qubits` - The control qubits for the operator. If not empty, the operator will be applied conditionally based on the control qubits. Otherwise, it will be applied unconditionally.
    ///
    /// # Returns:
    ///
    /// * The new state after applying the Phase Tdag operator.
    fn apply(
        &self,
        state: &State,
        target_qubits: &[usize],
        control_qubits: &[usize],
    ) -> Result<State, Error> {
        // Validation
        validate_qubits(state, target_qubits, control_qubits, 1)?;

        let target_qubit = target_qubits[0];
        let num_qubits = state.num_qubits();

        // Apply potentially controlled Phase Tdag operator
        #[allow(unused_assignments)]
        let mut new_state_vec: Vec<Complex<f64>> = state.state_vector.clone();
        let gpu_enabled: bool = cfg!(feature = "gpu");

        if num_qubits >= OPENCL_THRESHOLD_NUM_QUBITS && gpu_enabled {
            #[cfg(feature = "gpu")]
            {
                let global_work_size = 1 << num_qubits;
                let angle = -PI / 4.0;
                new_state_vec = execute_on_gpu(
                    state,
                    target_qubit,
                    control_qubits,
                    KernelType::PhaseShift,
                    global_work_size,
                    GpuKernelArgs::PhaseShift {
                        cos_angle: angle.cos() as f32,
                        sin_angle: angle.sin() as f32,
                    },
                )?;
            }
        } else if num_qubits >= PARALLEL_THRESHOLD_NUM_QUBITS {
            let invsqrt2: f64 = 1.0 / (2.0f64).sqrt();
            let phase_factor = Complex::new(invsqrt2, -invsqrt2);
            new_state_vec = state.state_vector.clone(); // Ensure cloned for CPU path
            new_state_vec
                .par_iter_mut()
                .enumerate()
                .for_each(|(i, current_amp_ref)| {
                    if ((i >> target_qubit) & 1 == 1) && check_controls(i, control_qubits) {
                        *current_amp_ref = state.state_vector[i] * phase_factor;
                    }
                });
        } else {
            let invsqrt2: f64 = 1.0 / (2.0f64).sqrt();
            let phase_factor = Complex::new(invsqrt2, -invsqrt2);
            new_state_vec = state.state_vector.clone(); // Ensure cloned for CPU path
            let dim: usize = 1 << num_qubits;
            for i in 0..dim {
                let target_bit_is_one = (i >> target_qubit) & 1 == 1;
                if target_bit_is_one && check_controls(i, control_qubits) {
                    new_state_vec[i] = state.state_vector[i] * phase_factor;
                }
            }
        }

        Ok(State {
            state_vector: new_state_vec,
            num_qubits,
        })
    }

    fn base_qubits(&self) -> usize {
        1 // Phase Tdag acts on 1 qubit only
    }

    fn to_compilable(&self) -> Option<&dyn Compilable> {
        Some(self)
    }
}

/// Defines the phase shift operator
///
/// A single-qubit operator that applies a phase shift of the provided angle to the |1> state. Also known as the phase shift gate.
#[derive(Debug, Clone, Copy)]
pub struct PhaseShift {
    pub(crate) angle: f64,
}

impl PhaseShift {
    /// Creates a new PhaseShift operator with the given angle.
    ///
    /// # Arguments:
    ///
    /// * `angle` - The angle of the phase shift in radians.
    pub fn new(angle: f64) -> Self {
        PhaseShift { angle }
    }
}

impl Operator for PhaseShift {
    /// Applies the phase shift operator to the given state's target qubit.
    ///
    /// # Arguments:
    ///
    /// * `state` - The state to apply the operator to.
    ///
    /// * `target_qubits` - The target qubits to apply the operator to. This should be a single qubit.
    ///
    /// * `control_qubits` - The control qubits for the operator. If not empty, the operator will be applied conditionally based on the control qubits. Otherwise, it will be applied unconditionally.
    ///
    /// # Returns:
    ///
    /// * The new state after applying the phase shift operator.
    ///
    /// # Errors:
    ///
    /// * `Error::InvalidNumberOfQubits` - If the target qubits is not 1.
    ///
    /// * `Error::InvalidQubitIndex` - If the target qubit index or control qubit index is invalid for the number of qubits in the state.
    ///
    /// * `Error::OverlappingControlAndTargetQubits` - If the control qubit and target qubit indices overlap.
    fn apply(
        &self,
        state: &State,
        target_qubits: &[usize],
        control_qubits: &[usize],
    ) -> Result<State, Error> {
        // Validation
        validate_qubits(state, target_qubits, control_qubits, 1)?;

        let target_qubit = target_qubits[0];
        let num_qubits = state.num_qubits();

        // Apply potentially controlled Phase Shift operator
        #[allow(unused_assignments)]
        let mut new_state_vec: Vec<Complex<f64>> = state.state_vector.clone();
        let gpu_enabled: bool = cfg!(feature = "gpu");

        if num_qubits >= OPENCL_THRESHOLD_NUM_QUBITS && gpu_enabled {
            #[cfg(feature = "gpu")]
            {
                let global_work_size = 1 << num_qubits;
                new_state_vec = execute_on_gpu(
                    state,
                    target_qubit,
                    control_qubits,
                    KernelType::PhaseShift,
                    global_work_size,
                    GpuKernelArgs::PhaseShift {
                        cos_angle: self.angle.cos() as f32,
                        sin_angle: self.angle.sin() as f32,
                    },
                )?;
            }
        } else if num_qubits >= PARALLEL_THRESHOLD_NUM_QUBITS {
            let phase_factor = Complex::new(self.angle.cos(), self.angle.sin());
            new_state_vec = state.state_vector.clone(); // Ensure cloned for CPU path
            new_state_vec
                .par_iter_mut()
                .enumerate()
                .for_each(|(i, current_amp_ref)| {
                    if ((i >> target_qubit) & 1 == 1) && check_controls(i, control_qubits) {
                        *current_amp_ref = state.state_vector[i] * phase_factor;
                    }
                });
        } else {
            let phase_factor = Complex::new(self.angle.cos(), self.angle.sin());
            new_state_vec = state.state_vector.clone(); // Ensure cloned for CPU path
            let dim: usize = 1 << num_qubits;
            for i in 0..dim {
                let target_bit_is_one = (i >> target_qubit) & 1 == 1;
                if target_bit_is_one && check_controls(i, control_qubits) {
                    new_state_vec[i] = state.state_vector[i] * phase_factor;
                }
            }
        }

        Ok(State {
            state_vector: new_state_vec,
            num_qubits,
        })
    }

    fn base_qubits(&self) -> usize {
        1 // Phase shift acts on 1 qubit only
    }

    fn to_compilable(&self) -> Option<&dyn Compilable> {
        Some(self)
    }
}

/// Defines the rotate-X operator
///
/// A single-qubit operator that applies a rotation around the X axis of the Bloch sphere by the given angle. Also known as the RX gate.
#[derive(Debug, Clone, Copy)]
pub struct RotateX {
    pub(crate) angle: f64,
}

impl RotateX {
    /// Creates a new RotateX operator with the given angle.
    ///
    /// # Arguments:
    ///
    /// * `angle` - The angle of rotation in radians.
    pub fn new(angle: f64) -> Self {
        RotateX { angle }
    }
}

impl Operator for RotateX {
    /// Applies the RotateX operator to the given state's target qubit.
    ///
    /// # Arguments:
    ///
    /// * `state` - The state to apply the operator to.
    ///
    /// * `target_qubits` - The target qubits to apply the operator to. This should be a single qubit.
    ///
    /// * `control_qubits` - The control qubits for the operator. If not empty, the operator will be applied conditionally based on the control qubits. Otherwise, it will be applied unconditionally.
    ///
    /// # Returns:
    ///
    /// * The new state after applying the RotateX operator.
    fn apply(
        &self,
        state: &State,
        target_qubits: &[usize],
        control_qubits: &[usize],
    ) -> Result<State, Error> {
        // Validation
        validate_qubits(state, target_qubits, control_qubits, 1)?;

        let target_qubit = target_qubits[0];
        let num_qubits = state.num_qubits();

        // Apply potentially controlled RotateX operator
        #[allow(unused_assignments)]
        let mut new_state_vec: Vec<Complex<f64>> = state.state_vector.clone();
        let gpu_enabled: bool = cfg!(feature = "gpu");

        if num_qubits >= OPENCL_THRESHOLD_NUM_QUBITS && gpu_enabled {
            #[cfg(feature = "gpu")]
            {
                let half_angle = self.angle / 2.0;
                let global_work_size = if num_qubits > 0 { 1 << (num_qubits - 1) } else { 1 };
                new_state_vec = execute_on_gpu(
                    state,
                    target_qubit,
                    control_qubits,
                    KernelType::RotateX,
                    global_work_size,
                    GpuKernelArgs::RotationGate {
                        cos_half_angle: half_angle.cos() as f32,
                        sin_half_angle: half_angle.sin() as f32,
                    },
                )?;
            }
        } else if num_qubits >= PARALLEL_THRESHOLD_NUM_QUBITS {
            // Parallel implementation
            let half_angle: f64 = self.angle / 2.0;
            let cos_half: f64 = half_angle.cos();
            let sin_half: f64 = half_angle.sin();
            let i_complex: Complex<f64> = Complex::new(0.0, 1.0);
            let dim: usize = 1 << num_qubits;
            new_state_vec = state.state_vector.clone(); // Ensure cloned for CPU path

            let updates: Vec<(usize, Complex<f64>)> = (0..dim)
                .into_par_iter()
                .filter_map(|i| {
                    if ((i >> target_qubit) & 1 == 0) && check_controls(i, control_qubits) {
                        let j = i | (1 << target_qubit);
                        let amp_i = state.state_vector[i];
                        let amp_j = state.state_vector[j];
                        Some(vec![
                            (i, cos_half * amp_i - i_complex * sin_half * amp_j),
                            (j, -i_complex * sin_half * amp_i + cos_half * amp_j),
                        ])
                    } else {
                        None
                    }
                })
                .flatten()
                .collect();
            for (idx, val) in updates {
                new_state_vec[idx] = val;
            }
        } else {
            // Sequential implementation
            let half_angle: f64 = self.angle / 2.0;
            let cos_half: f64 = half_angle.cos();
            let sin_half: f64 = half_angle.sin();
            let i_complex: Complex<f64> = Complex::new(0.0, 1.0);
            let dim: usize = 1 << num_qubits;
            new_state_vec = state.state_vector.clone(); // Ensure cloned for CPU path

            for i in 0..dim {
                if (i >> target_qubit) & 1 == 0 {
                    let j = i | (1 << target_qubit);
                    if check_controls(i, control_qubits) {
                        let amp_i = state.state_vector[i];
                        let amp_j = state.state_vector[j];
                        new_state_vec[i] = cos_half * amp_i - i_complex * sin_half * amp_j;
                        new_state_vec[j] = -i_complex * sin_half * amp_i + cos_half * amp_j;
                    }
                }
            }
        }

        Ok(State {
            state_vector: new_state_vec,
            num_qubits: state.num_qubits(),
        })
    }

    fn base_qubits(&self) -> usize {
        1 // RotateX acts on 1 qubit only
    }

    fn to_compilable(&self) -> Option<&dyn Compilable> {
        Some(self)
    }
}

/// Defines the rotate-Y operator
///
/// A single-qubit operator that applies a rotation around the Y axis of the Bloch sphere by the given angle. Also known as the RY gate.
#[derive(Debug, Clone, Copy)]
pub struct RotateY {
    pub(crate) angle: f64,
}

impl RotateY {
    /// Creates a new RotateY operator with the given angle.
    ///
    /// # Arguments:
    ///
    /// * `angle` - The angle of rotation in radians.
    pub fn new(angle: f64) -> Self {
        RotateY { angle }
    }
}

impl Operator for RotateY {
    /// Applies the RotateY operator to the given state's target qubit.
    ///
    /// # Arguments:
    ///
    /// * `state` - The state to apply the operator to.
    ///
    /// * `target_qubits` - The target qubits to apply the operator to. This should be a single qubit.
    ///
    /// * `control_qubits` - The control qubits for the operator. If not empty, the operator will be applied conditionally based on the control qubits. Otherwise, it will be applied unconditionally.
    ///
    /// # Returns:
    ///
    /// * The new state after applying the RotateY operator.
    fn apply(
        &self,
        state: &State,
        target_qubits: &[usize],
        control_qubits: &[usize],
    ) -> Result<State, Error> {
        // Validation
        validate_qubits(state, target_qubits, control_qubits, 1)?;

        let target_qubit = target_qubits[0];
        let num_qubits = state.num_qubits();

        // Apply potentially controlled RotateY operator
        #[allow(unused_assignments)]
        let mut new_state_vec: Vec<Complex<f64>> = state.state_vector.clone();
        let gpu_enabled: bool = cfg!(feature = "gpu");

        if num_qubits >= OPENCL_THRESHOLD_NUM_QUBITS && gpu_enabled {
            #[cfg(feature = "gpu")]
            {
                let half_angle = self.angle / 2.0;
                let global_work_size = if num_qubits > 0 { 1 << (num_qubits - 1) } else { 1 };
                new_state_vec = execute_on_gpu(
                    state,
                    target_qubit,
                    control_qubits,
                    KernelType::RotateY,
                    global_work_size,
                    GpuKernelArgs::RotationGate {
                        cos_half_angle: half_angle.cos() as f32,
                        sin_half_angle: half_angle.sin() as f32,
                    },
                )?;
            }
        } else if num_qubits >= PARALLEL_THRESHOLD_NUM_QUBITS {
            // Parallel implementation
            let half_angle: f64 = self.angle / 2.0;
            let cos_half: f64 = half_angle.cos();
            let sin_half: f64 = half_angle.sin();
            let dim: usize = 1 << num_qubits;
            new_state_vec = state.state_vector.clone(); // Ensure cloned for CPU path

            let updates: Vec<(usize, Complex<f64>)> = (0..dim)
                .into_par_iter()
                .filter_map(|i| {
                    if ((i >> target_qubit) & 1 == 0) && check_controls(i, control_qubits) {
                        let j = i | (1 << target_qubit);
                        let amp_i = state.state_vector[i];
                        let amp_j = state.state_vector[j];
                        Some(vec![
                            (i, cos_half * amp_i - sin_half * amp_j),
                            (j, sin_half * amp_i + cos_half * amp_j),
                        ])
                    } else {
                        None
                    }
                })
                .flatten()
                .collect();
            for (idx, val) in updates {
                new_state_vec[idx] = val;
            }
        } else {
            // Sequential implementation
            let half_angle: f64 = self.angle / 2.0;
            let cos_half: f64 = half_angle.cos();
            let sin_half: f64 = half_angle.sin();
            let dim: usize = 1 << num_qubits;
            new_state_vec = state.state_vector.clone(); // Ensure cloned for CPU path

            for i in 0..dim {
                if (i >> target_qubit) & 1 == 0 {
                    let j = i | (1 << target_qubit);
                    if check_controls(i, control_qubits) {
                        let amp_i = state.state_vector[i];
                        let amp_j = state.state_vector[j];
                        new_state_vec[i] = cos_half * amp_i - sin_half * amp_j;
                        new_state_vec[j] = sin_half * amp_i + cos_half * amp_j;
                    }
                }
            }
        }

        Ok(State {
            state_vector: new_state_vec,
            num_qubits: state.num_qubits(),
        })
    }

    fn base_qubits(&self) -> usize {
        1 // RotateY acts on 1 qubit only
    }

    fn to_compilable(&self) -> Option<&dyn Compilable> {
        Some(self)
    }
}

/// Defines the rotate-Z operator
///
/// A single-qubit operator that applies a rotation around the Z axis of the Bloch sphere by the given angle. Also known as the RZ gate.
#[derive(Debug, Clone, Copy)]
pub struct RotateZ {
    pub(crate) angle: f64,
}

impl RotateZ {
    /// Creates a new RotateZ operator with the given angle.
    ///
    /// # Arguments:
    ///
    /// * `angle` - The angle of rotation in radians.
    pub fn new(angle: f64) -> Self {
        RotateZ { angle }
    }
}

impl Operator for RotateZ {
    /// Applies the RotateZ operator to the given state's target qubit.
    ///
    /// # Arguments:
    ///
    /// * `state` - The state to apply the operator to.
    ///
    /// * `target_qubits` - The target qubits to apply the operator to. This should be a single qubit.
    ///
    /// * `control_qubits` - The control qubits for the operator. If not empty, the operator will be applied conditionally based on the control qubits. Otherwise, it will be applied unconditionally.
    ///
    /// # Returns:
    ///
    /// * The new state after applying the RotateZ operator.
    fn apply(
        &self,
        state: &State,
        target_qubits: &[usize],
        control_qubits: &[usize],
    ) -> Result<State, Error> {
        // Validation
        validate_qubits(state, target_qubits, control_qubits, 1)?;

        let target_qubit = target_qubits[0];
        let num_qubits = state.num_qubits();

        // Apply potentially controlled RotateZ operator
        #[allow(unused_assignments)]
        let mut new_state_vec: Vec<Complex<f64>> = state.state_vector.clone();
        let gpu_enabled: bool = cfg!(feature = "gpu");

        if num_qubits >= OPENCL_THRESHOLD_NUM_QUBITS && gpu_enabled {
            #[cfg(feature = "gpu")]
            {
                let half_angle = self.angle / 2.0;
                let global_work_size = 1 << num_qubits; // N work items for RZ
                new_state_vec = execute_on_gpu(
                    state,
                    target_qubit,
                    control_qubits,
                    KernelType::RotateZ,
                    global_work_size,
                    GpuKernelArgs::RotationGate {
                        cos_half_angle: half_angle.cos() as f32,
                        sin_half_angle: half_angle.sin() as f32,
                    },
                )?;
            }
        } else if num_qubits >= PARALLEL_THRESHOLD_NUM_QUBITS {
            // Parallel implementation
            let half_angle = self.angle / 2.0;
            let phase_0 = Complex::new(half_angle.cos(), -half_angle.sin());
            let phase_1 = Complex::new(half_angle.cos(), half_angle.sin());
            new_state_vec = state.state_vector.clone(); // Ensure cloned for CPU path

            new_state_vec
                .par_iter_mut()
                .enumerate()
                .for_each(|(i, current_amp_ref)| {
                    if check_controls(i, control_qubits) {
                        let target_bit_is_one = (i >> target_qubit) & 1 == 1;
                        if target_bit_is_one {
                            *current_amp_ref = state.state_vector[i] * phase_1;
                        } else {
                            *current_amp_ref = state.state_vector[i] * phase_0;
                        }
                    }
                });
        } else {
            // Sequential implementation
            let half_angle = self.angle / 2.0;
            let phase_0 = Complex::new(half_angle.cos(), -half_angle.sin());
            let phase_1 = Complex::new(half_angle.cos(), half_angle.sin());
            let dim: usize = 1 << num_qubits;
            new_state_vec = state.state_vector.clone(); // Ensure cloned for CPU path

            for i in 0..dim {
                if check_controls(i, control_qubits) {
                    let target_bit_is_one = (i >> target_qubit) & 1 == 1;
                    if target_bit_is_one {
                        new_state_vec[i] = state.state_vector[i] * phase_1;
                    } else {
                        new_state_vec[i] = state.state_vector[i] * phase_0;
                    }
                }
            }
        }

        Ok(State {
            state_vector: new_state_vec,
            num_qubits: state.num_qubits(),
        })
    }

    fn base_qubits(&self) -> usize {
        1 // RotateZ acts on 1 qubit only
    }

    fn to_compilable(&self) -> Option<&dyn Compilable> {
        Some(self)
    }
}

/// An arbitrary 2×2 unitary operator.
///
/// This operator can be applied to a single qubit in a quantum state. It is represented by a 2×2 unitary matrix.
#[derive(Debug, Clone, Copy)]
pub struct Unitary2 {
    /// The 2×2 unitary matrix representing the operator.
    pub(crate) matrix: [[Complex<f64>; 2]; 2],
}

impl Unitary2 {
    /// Creates a new Unitary2 operator with the given 2×2 unitary matrix.
    ///
    /// # Arguments:
    ///
    /// * `matrix` - A 2×2 unitary matrix represented as a 2D array of complex numbers.
    ///
    /// # Returns:
    ///
    /// * `Result<Self, Error>` - A result containing the new Unitary2 operator or an error if the matrix is not unitary.
    ///
    /// # Errors:
    ///
    /// * `Error::NonUnitaryMatrix` - If the provided matrix is not unitary.
    pub fn new(matrix: [[Complex<f64>; 2]; 2]) -> Result<Self, Error> {
        // Faster 2×2 unitary check: U U_dagger = I (rows are orthonormal)
        let tol: f64 = f64::EPSILON * 2.0; // Tolerance for floating point comparisons
        let a: Complex<f64> = matrix[0][0]; // U_00
        let b: Complex<f64> = matrix[0][1]; // U_01
        let c: Complex<f64> = matrix[1][0]; // U_10
        let d: Complex<f64> = matrix[1][1]; // U_11

        // Check if each row has norm 1
        // Row 0: |a|^2 + |b|^2 == 1
        if ((a.norm_sqr() + b.norm_sqr()) - 1.0).abs() > tol {
            return Err(Error::NonUnitaryMatrix);
        }
        // Row 1: |c|^2 + |d|^2 == 1
        if ((c.norm_sqr() + d.norm_sqr()) - 1.0).abs() > tol {
            return Err(Error::NonUnitaryMatrix);
        }

        // Check if rows are orthogonal
        // Row 0 dot Row 1_conj: a*c_conj + b*d_conj == 0
        if (a * c.conj() + b * d.conj()).norm_sqr() > tol * tol {
            // Compare norm_sqr with tol^2
            return Err(Error::NonUnitaryMatrix);
        }

        Ok(Unitary2 { matrix })
    }
}

impl Operator for Unitary2 {
    /// Applies the Unitary2 operator to the given state's target qubit.
    ///
    /// # Arguments:
    ///
    /// * `state` - The state to apply the operator to.
    ///
    /// * `target_qubits` - The target qubits to apply the operator to. This should be a single qubit.
    ///
    /// * `control_qubits` - The control qubits for the operator. If not empty, the operator will be applied conditionally based on the control qubits. Otherwise, it will be applied unconditionally.
    ///
    /// # Returns:
    ///
    /// * The new state after applying the Unitary2 operator.
    fn apply(
        &self,
        state: &State,
        target_qubits: &[usize],
        control_qubits: &[usize],
    ) -> Result<State, Error> {
        // Validation
        validate_qubits(state, target_qubits, control_qubits, 1)?;

        let t: usize = target_qubits[0];
        let nq: usize = state.num_qubits();

        // Apply 2×2 block on each basis‐pair
        let dim = 1 << nq;
        let mut new_state_vec = state.state_vector.clone();

        if nq >= PARALLEL_THRESHOLD_NUM_QUBITS {
            // Parallel implementation
            let updates: Vec<(usize, Complex<f64>)> = (0..dim)
                .into_par_iter()
                .filter_map(|i| {
                    if ((i >> t) & 1 == 0) && check_controls(i, control_qubits) {
                        let j = i | (1 << t);
                        let ai = state.state_vector[i];
                        let aj = state.state_vector[j];
                        Some(vec![
                            (i, self.matrix[0][0] * ai + self.matrix[0][1] * aj),
                            (j, self.matrix[1][0] * ai + self.matrix[1][1] * aj),
                        ])
                    } else {
                        None
                    }
                })
                .flatten()
                .collect();
            for (idx, val) in updates {
                new_state_vec[idx] = val;
            }
        } else {
            // Sequential implementation
            for i in 0..dim {
                if (i >> t) & 1 == 0 {
                    let j = i | (1 << t);
                    if check_controls(i, control_qubits) {
                        let ai = state.state_vector[i];
                        let aj = state.state_vector[j];
                        new_state_vec[i] = self.matrix[0][0] * ai + self.matrix[0][1] * aj;
                        new_state_vec[j] = self.matrix[1][0] * ai + self.matrix[1][1] * aj;
                    }
                }
            }
        }

        Ok(State {
            state_vector: new_state_vec,
            num_qubits: nq,
        })
    }

    fn base_qubits(&self) -> usize {
        1
    }

    fn to_compilable(&self) -> Option<&dyn Compilable> {
        Some(self)
    }
}
