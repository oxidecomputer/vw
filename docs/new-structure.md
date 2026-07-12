# New Structure

We're going to be putting together a new structure for `vw` workspaces. A vw
workspace contains

- A VHDL design
- A Rust-based operating system driver for the VHDL hardware design
- Test suites at multiple levels
  - Mixed mode analog/digital testbenches for the hardware/wire interface
  - Pure digital testbenches for the VHDL design
  - Pure unit and rust integration tests for the Rust driver
  - Codesign tests that integrate combinations of Rust, digital and mixed mode
    digital/analog tests 
