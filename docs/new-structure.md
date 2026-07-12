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
    digital/analog tests.

The filesystem structure that captures this is the following

workspace-root
|- `design.htcl`: entry point for HTCL-based configuration and automation
|- `ip/**/*.htcl`: IP configuration files used by design.htcl
|- `hdl/**/*.vhd`: VHDL design sources
|- `bench/**/*.rs`: Rust-based hardware testbenches, pure digital and mixed mode
|- `driver/**/*.rs`: Rust-based kernel driver for the hardware design

All of this machinery exists in ~/src/redhawk. But the organization is quite
haphazard. Within ~/src/redhawk most of the design and testbenches live within
host/hdl/n1 under design and bench respectively. This is basically the simulated
world. But then when we need to enter the synthesized world, we have to go to
vivado/vpk120-evb where a set of completely unmaintainable TCL scripts smash
a vivado project onto host/hdl/n1/design that's locally symlinked into
vivado/vpk120-evb. Within vivado/ there is also the metro folder which is a
vivado project for our custom metro motherboard with a versal on it, as opposed
to the vpk120-evb which is a vivado project for an AMD evaluation board.

The bottom line is redhawk has become a hot mess and we're going to start
incrementally pulling redhawk sources here into metroid using the organization
I described above.

Some other things to be aware of are:

## Anodizer

Much of our test infra depends on anodizer (at ~/src/anodizer)
which is machinery for generating Rust structures that correspond to VHDL
records.

## Rust Co-sim

We have developed Rust-based cosimulation machinery (at ~/src/rust-cosim). This
allows us to define our test benches in Rust instead of VHDL. Recently, this
machinery grew support for testing VHDL entities directly in rust, without
having to build a VHDL test bench wrapper. This is the path forward, but we
still have many test benches with explicit VHDL testbench harnesses that are
driven by Rust.

## RSF

Eventually we want to generate RSF specifications (~/src/rsf) for registe
interfaces. Right now these are manually maintained between our VHDL register
interfaces and our Rust kernel drivers.

## Builds from the bottom up

One of the goals here is to build form the bottom up.

1. Configure IP, generate wrappers and synthesize IP.
2. Elaborate/Synthesize/implement VHDL which depends on IP synth and wrappers
   from (1)
4. Generate RSF specs from VHDL (this has yet to be done, maybe with anodizer
   later?)
5. Build rust testbenches using anodizer
6. Build driver code using cargo with a build.rs that pulls in generated RSF
   and anodizer artifacts (if/when needed)

A lot of the machinery here has yet to be built, but this is the overall flow
we are looking for, and for it to be completely automatic. Data structures
should not be manually maintaind across the hardware/software boundary.

## Vhdl-ls

There is currently a strange relationship between vhdl-ls and vw, in that vw
uses vhdl-ls config to find VHDL files. I want to stop that. I'd like vhdl-ls
to get it's config from vw and not the other way around. I would even like to
explore embedding vhdl-ls as a library and have the vw LSP be the entry point
for HTCL, VHDL and Rust sources combined (also need to figure out the Rust
side of that). I've put the vhdl-ls sources at ~/src/rust_hdl.

To further drive home the point, the structure described at the beginning of
this document means that vw knows where ALL files are according to that
structure, so we don't need to glean it from other configs.

## Where to start

### 1. Pull in design code and synthesize it

I would like to start by pulling in our VHDL design code and then synthesize it.
This will involve

1. Copying redhawk hdl design code into the hdl directory
2. Adding a `vhdl_dependency_sources` function to the vw htcl module. This will
   be rust under the hood that we call via our RPC mechanism that returns all
   the VHDL sources VW has pulled in as dependencies.
3. Adding a `vhdl_design_sources` function to the vw htcl module. This will be
   rust under the hood that we call via our RPC mechanism that finds all the
   design sources in the vw workspace and returns them as a list.
4. Extending design.htcl to synthesize our VHDL design sources together with
   our VHDL dependency sources.

### 2. Sort out the vw/vhdl-ls relationship

Now that (1) is done and we can synthesize our VHDL design sources, we need
to get vhdl-ls working. Let's explore having our existing vw LSP use vhdl-ls
as a library to see if that is a reasonable approach. Something else to
consider is using vhdl-ls standalone, but figure out a way for vw to provide
it with a dynamic configuration source.

### 3. Start to bring over test benches

We've got quite a few test benches to bring over. This may be one of the
more compex tasks. I'd like them to be runnable with `vw bench` and have
the same nice interface we worked to develop for `vw test` for HTCL tests.
This will take a fair amount of iteration to get right and to fully integrate
anodizer, rust-cosim and mixed-mode simulation support that uses Xyce under
the hood. There are some heavyweight C/C++ dependencies lurking beneath here
that we want to integrate in a way that can capture all the build and runtime
complexity there in a reasonable way that does not make using the testbenches
a constant pain.

### 4. Bring the driver over

We can start by just bringing the rhdrv cargo workspace over from redhawk. The
harder part is going to be thinking about how we integate this with everything
else. e.g. generating RSF specs from VHDL code, propagating those up to a place
where build.rs can capture them and having all that be automatic. Automatic
means robust change and rebuild detection as well.


