# vw - VHDL Workspace

`vw` is a command-line tool for managing VHDL workspaces, dependencies, and testbench execution,
similar in spirit to Rust's `cargo`. It focuses on dependency management, language server
integration, and intelligent testbench simulation using NVC.

## Features

- **Dependency Management**: Add, remove, and update VHDL dependencies from git repositories
- **Smart Caching**: Dependencies are cached locally to avoid repeated downloads
- **Language Server Integration**: Automatically generates `vhdl_ls.toml` configuration
- **Testbench Execution**: Intelligent NVC-based testbench simulation with dependency analysis
- **Flexible Source Paths**: Specify subdirectories within repositories as source paths
- **Lock File Support**: Tracks exact dependency versions with `vw.lock`

## Installation

Build from source:

```bash
cargo install --path .
```

## Quick Start

1. **Initialize a new workspace:**
   ```bash
   vw init my-project
   ```

2. **Add a dependency:**
   ```bash
   vw add https://github.com/user/repo --branch main --src hdl/src
   ```

3. **Update dependencies:**
   ```bash
   vw update
   ```

4. **Run testbenches:**
   ```bash
   # List available testbenches
   vw test --list
   
   # Run a specific testbench
   vw test my_design_tb
   
   # Run with specific VHDL standard
   vw test my_design_tb --std 2008
   ```
## Configuration Files

### `vw.toml`
Example workspace configuration file:

```toml
[workspace]
name = "my-project"
version = "0.1.0"

[dependencies.quartz]
repo = "https://github.com/oxidecomputer/quartz"
branch = "main"
src = "hdl/ip/vhd"
```

### `vw.lock`
Lock file tracking exact dependency versions:

```toml
[dependencies.quartz]
repo = "https://github.com/oxidecomputer/quartz"
commit = "3084a34e3c83f8b45cda7ea428f8fcc8f17484c2"
src = "hdl/ip/vhd"
path = "$HOME/.vw/deps/quartz-3084a34e3c83f8b45cda7ea428f8fcc8f17484c2"
```

### `vhdl_ls.toml`
Automatically generated configuration for the vhdl_ls language server:

```toml
[libraries.quartz]
files = [
    "$HOME/.vw/deps/quartz-3084a34e3c83f8b45cda7ea428f8fcc8f17484c2/common/utils/calc_pkg.vhd",
    # ... more files
]
```

## How It Works

### Dependency Management

1. **Dependency Resolution**: When you run `vw update`, the tool resolves branch names to specific commit hashes
2. **Caching**: Dependencies are downloaded to `$HOME/.vw/deps/<name>-<commit>/`
3. **File Filtering**: Only VHDL files (`.vhd` and `.vhdl`) from the specified `src` path are cached
4. **Language Server Config**: The tool merges dependency information with any existing `vhdl_ls.toml` configuration

### Testbench Execution

`vw test` provides intelligent testbench execution using NVC simulator:

1. **Smart Dependency Analysis**: Analyzes VHDL files to find only the dependencies actually needed:
   - Detects `use work.package_name` statements
   - Finds direct entity instantiations like `entity work.entity_name`
   - Follows component declarations and instantiations
   - Recursively resolves dependency chains

2. **Intelligent Filtering**:
   - Includes only referenced files from your source code
   - Excludes other testbenches while allowing common bench utilities
   - Uses proper topological sorting for correct compilation order

3. **NVC Integration**:
   - Analyzes external libraries first with proper library names
   - Compiles and runs testbenches with optimized file sets
   - Generates FST waveform files for debugging
   - Provides clear error messages with exact commands run

## Directory Structure

```
my-project/
├── vw.toml              # Workspace configuration
├── vw.lock              # Dependency lock file  
├── vhdl_ls.toml         # Language server configuration (auto-generated)
├── src/
│   ├── my_design.vhd    # Your VHDL source files
│   └── my_package.vhd   # VHDL packages
└── bench/
    ├── my_design_tb.vhd # Testbenches
    ├── other_tb.vhd     # Additional testbenches
    └── test_utils.vhd   # Common test utilities
```

## Examples

See the `example/` directory for a sample workspace configuration.
