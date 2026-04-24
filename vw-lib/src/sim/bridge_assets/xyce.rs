use std::ffi::CString;
#[allow(unused_imports)]
use std::os::raw::{c_char, c_double, c_int, c_void};
use std::path::Path;
use std::ptr;

// FFI declarations matching N_CIR_XyceCInterface.h
extern "C" {
    fn xyce_open(ptr: *mut *mut c_void);
    fn xyce_close(ptr: *mut *mut c_void);
    fn xyce_initialize(ptr: *mut *mut c_void, narg: c_int, argv: *mut *mut c_char) -> c_int;
    fn xyce_simulateUntil(
        ptr: *mut *mut c_void,
        requested_until_time: c_double,
        completed_until_time: *mut c_double,
    ) -> c_int;
    fn xyce_updateTimeVoltagePairs(
        ptr: *mut *mut c_void,
        dac_name: *mut c_char,
        num_points: c_int,
        time_array: *mut c_double,
        voltage_array: *mut c_double,
    ) -> c_int;
    fn xyce_obtainResponse(
        ptr: *mut *mut c_void,
        variable_name: *mut c_char,
        value: *mut c_double,
    ) -> c_int;
    fn xyce_checkResponseVar(ptr: *mut *mut c_void, variable_name: *mut c_char) -> c_int;
    fn xyce_getCircuitValue(ptr: *mut *mut c_void, param_name: *mut c_char) -> c_double;
    fn xyce_set_working_directory(ptr: *mut *mut c_void, dir_name: *const c_char);
}

pub struct Xyce {
    ptr: *mut c_void,
}

impl Xyce {
    /// Create a new Xyce simulator instance and initialize it with the given
    /// netlist file.
    pub fn new(netlist_path: &Path) -> Result<Self, String> {
        let mut ptr: *mut c_void = ptr::null_mut();
        unsafe { xyce_open(&mut ptr) };

        if ptr.is_null() {
            return Err("xyce_open returned null".into());
        }

        // Set working directory to the netlist's parent directory so that
        // relative file references (e.g., TABLE("main.dat")) resolve correctly.
        if let Some(parent) = netlist_path.parent() {
            let dir = CString::new(parent.to_str().unwrap()).unwrap();
            unsafe { xyce_set_working_directory(&mut ptr, dir.as_ptr()) };
        }

        let netlist = netlist_path
            .to_str()
            .ok_or("invalid netlist path")?
            .to_string();
        let prog = CString::new("Xyce").unwrap();
        let netlist_c = CString::new(netlist).unwrap();

        let mut argv: Vec<*mut c_char> = vec![
            prog.as_ptr() as *mut c_char,
            netlist_c.as_ptr() as *mut c_char,
        ];

        let status =
            unsafe { xyce_initialize(&mut ptr, argv.len() as c_int, argv.as_mut_ptr()) };

        if status == 0 {
            return Err("xyce_initialize failed (ERROR)".into());
        }

        Ok(Xyce { ptr })
    }

    /// Advance the simulation to `until_time` (seconds). Returns the actual
    /// time reached. Returns Ok(true) if simulation completed normally,
    /// Ok(false) if the netlist's final time was reached first.
    pub fn simulate_until(&mut self, until_time: f64) -> Result<(bool, f64), String> {
        let mut completed_time: f64 = 0.0;
        let status = unsafe {
            xyce_simulateUntil(&mut self.ptr, until_time, &mut completed_time)
        };
        if status == 0 {
            return Err(format!(
                "xyce_simulateUntil failed at t={until_time}"
            ));
        }
        let reached = completed_time >= until_time;
        Ok((reached, completed_time))
    }

    /// Update a DAC device with a new time/voltage schedule.
    /// `dac_name` should be the fully-qualified DAC name, e.g., "YDAC!DAC_SYM_MAIN".
    pub fn update_dac(
        &mut self,
        dac_name: &str,
        times: &[f64],
        voltages: &[f64],
    ) -> Result<(), String> {
        assert_eq!(times.len(), voltages.len());
        let name_c = CString::new(dac_name).unwrap();
        let mut times_vec = times.to_vec();
        let mut volts_vec = voltages.to_vec();

        let status = unsafe {
            xyce_updateTimeVoltagePairs(
                &mut self.ptr,
                name_c.as_ptr() as *mut c_char,
                times_vec.len() as c_int,
                times_vec.as_mut_ptr(),
                volts_vec.as_mut_ptr(),
            )
        };
        if status == 0 {
            return Err(format!("xyce_updateTimeVoltagePairs failed for {dac_name}"));
        }
        Ok(())
    }

    /// Read a simulation response variable (.MEASURE), e.g., "MAXV1".
    pub fn get_response(&mut self, var_name: &str) -> Result<f64, String> {
        let name_c = CString::new(var_name).unwrap();
        let mut value: f64 = 0.0;
        let status = unsafe {
            xyce_obtainResponse(
                &mut self.ptr,
                name_c.as_ptr() as *mut c_char,
                &mut value,
            )
        };
        if status == 0 {
            return Err(format!("xyce_obtainResponse failed for {var_name}"));
        }
        Ok(value)
    }

    /// Read a circuit value by name, e.g., "V(OUT_P)".
    pub fn get_circuit_value(&mut self, param_name: &str) -> f64 {
        let name_c = CString::new(param_name).unwrap();
        unsafe { xyce_getCircuitValue(&mut self.ptr, name_c.as_ptr() as *mut c_char) }
    }

    /// Check if a response variable name is valid.
    pub fn check_response_var(&mut self, var_name: &str) -> bool {
        let name_c = CString::new(var_name).unwrap();
        let status = unsafe {
            xyce_checkResponseVar(&mut self.ptr, name_c.as_ptr() as *mut c_char)
        };
        status == 1
    }
}

impl Drop for Xyce {
    fn drop(&mut self) {
        unsafe { xyce_close(&mut self.ptr) };
    }
}

// Xyce is not thread-safe, but we only use it from a single-threaded executor.
unsafe impl Send for Xyce {}
