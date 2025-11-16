# VHDL Workspace

`vw` is a tool to manage VHDL workspaces. `vw` is built using the rust Clap
crate. It is similar in spirit to Rust's `cargo`. The focus includes dependency
management and testbench execution using NVC simulator.

Consider the example file `example/vw.toml`. This file describes a workspace
with a single dependency, the quartz repository. A repo dependency has a `repo`
property that's a path to a git repository accessible over https, a `branch`
property or a `commit` property that specifies either a branch or commit within
that repository to use and a `src` property that describes where to find the
VHDL code.

The `src` property supports multiple formats:
- **Directory path**: A path to a directory containing VHDL files (e.g., `"hdl/ip/vhd"`). Use the `recursive` flag to include subdirectories.
- **Single file**: A path to a specific VHDL file (e.g., `"hdl/ip/vhd/uart_pkg.vhd"`).
- **Glob pattern**: A glob pattern to match specific files (e.g., `"hdl/ip/vhd/**/*.vhd"` or `"hdl/**/pkg_*.vhd"`). The `recursive` flag is ignored for glob patterns as they handle their own path traversal.

## Dependency Management

Executing `vw update` will do a few things:

- For each dependency create a
`$HOME/.vw/deps/<dependency-name>-<dependency-commit>` directory with the
contents for the target repository at the defined path. Only VHDL files with
either a `vhd` or `vhdl` extension are included.

- For each dependency an entry in a `vw.lock` file is created. This is a JSON
file that tracks what versions of dependencies are being used in the workspace
and can map this workspace's dependencies to specific downloaded artifacts in
`$HOME/.vw/deps` may contain multiple versions of a given dependency.

- Creates a vhdl_ls.toml configuration file for the vhdl_ls language server
that includes dependencies as libraries. The file to include in vhdl_ls for the
library is one that ends in package, e.g. `some_name_pkg.vhd`. If such a file
does not exist, it is not included as a library. Dependencies may have multiple
directories, and each directory should be searched for a package file ending in
`_pkg.vhd` for library inclusion.

## Testbench Execution

`vw test` provides intelligent testbench execution using NVC simulator with advanced dependency analysis.

### Usage

```bash
# Run a specific testbench
vw test my_testbench_tb

# Run with specific VHDL standard
vw test my_testbench_tb --std 2008

# List all available testbenches
vw test --list
```

### Features

**Smart Dependency Analysis**: Only includes the minimal set of VHDL files actually needed by each testbench:
- Analyzes `use work.package_name` statements
- Detects direct entity instantiations like `entity work.entity_name`
- Follows component declarations and instantiations
- Recursively resolves dependency chains

**Intelligent File Filtering**:
- Includes only referenced files from defaultlib
- Excludes other testbenches while allowing common bench utilities
- Proper topological sorting ensures correct compilation order

**NVC Integration**:
- Analyzes non-defaultlib libraries first using `nvc --std=<std> --work=<lib> -M 256m -a <files>`
- Runs testbench simulation with `nvc --std=<std> -M 256m -L . -a --check-synthesis <files> -e <tb> -r <tb> --dump-arrays --format=fst --wave=<tb>.fst`
- Converts library names with hyphens to underscores for NVC compatibility

**Testbench Discovery**:
- Automatically finds testbenches in `bench/` directory
- Supports both `.vhd` and `.vhdl` extensions
- Uses regex to identify entity declarations

### File Organization

Place testbenches in a `bench/` directory:
```
project/
├── src/
│   ├── my_entity.vhd
│   └── my_package.vhd
├── bench/
│   ├── my_testbench_tb.vhd    # Individual testbenches
│   ├── other_testbench_tb.vhd
│   └── test_utils.vhd         # Common utilities (included when referenced)
├── vw.toml
└── vhdl_ls.toml
```
