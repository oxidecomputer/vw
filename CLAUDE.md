# VHDL Workspace

`vw` is a tool to manage VHDL workspaces. `vw` is built using the rust Clap
crate. It is similar in spirit to Rust's `cargo`. However right now, the focus
is exclusively on dependency management and not on simulating or synthesizing
VHDL code.

Consider the example file `example/vw.toml`. This file describes a workspace
with a single dependency, the quartz repository. A repo dependency has a `repo`
property that's a path to a git repository accessible over https, a `branch`
property or a `commit` property that specifies either a branch or commit within
that repository to use and a `src` property that describes where to find the
VHDL code.

Executing `vw update` will do a few things.

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
