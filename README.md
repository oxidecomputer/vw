# vw - VHDL Workspace

`vw` is a command-line tool for managing VHDL workspaces and dependencies,
similar in spirit to Rust's `cargo`. It focuses on dependency management and
automatically configures the vhdl_ls language server.

## Features

- **Dependency Management**: Add, remove, and update VHDL dependencies from git repositories
- **Smart Caching**: Dependencies are cached locally to avoid repeated downloads
- **Language Server Integration**: Automatically generates `vhdl_ls.toml` configuration
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

1. **Dependency Resolution**: When you run `vw update`, the tool resolves branch names to specific commit hashes
2. **Caching**: Dependencies are downloaded to `$HOME/.vw/deps/<name>-<commit>/`
3. **File Filtering**: Only VHDL files (`.vhd` and `.vhdl`) from the specified `src` path are cached
4. **Language Server Config**: The tool merges dependency information with any existing `vhdl_ls.toml` configuration

## Directory Structure

```
my-project/
├── vw.toml              # Workspace configuration
├── vw.lock              # Dependency lock file
├── vhdl_ls.toml         # Language server configuration (auto-generated)
└── src/
    └── my_design.vhd    # Your VHDL source files
```

## Examples

See the `example/` directory for a sample workspace configuration.
