# vw: extending for HDL workflow scripting

## Audience and intent

This document is a working plan for extending [`vw`](https://github.com/oxidecomputer/vw)
with first-class support for HDL workflow scripting: a structured TCL dialect
("htcl"), a workflow-aware analyzer (LSP), an interactive REPL, and a
Vivado-driving executor. The audience is Claude Code working with the
author. Treat it as a living spec — open questions are called out
explicitly; close them with the author before locking in design decisions.

## Goal

The underlying purpose of this work is **complexity management for HDL
designs**: making IP configuration and workflow scripting first-class
source-controlled artifacts that engineers can read, write, review, and
evolve over years. See "Strategic context" below for the full framing.
The concrete capabilities the project adds to `vw`:

1. **Provide a best-in-class interactive experience for HDL workflow
   code, in both the editor and a REPL.** This is the primary goal.
   Completion, hover, diagnostics, and navigation should match what a
   Rust or TypeScript developer expects from their IDE — and the same
   capabilities should be available in an interactive shell that
   replaces Vivado's TCL console. Both surfaces consume the same
   analysis backend (`vw-htcl`), so a feature built for one is
   available to the other for free. Every other language-design
   decision in this document is partly in service of this goal — the
   parser, the proc grammar, the module system, and the reuse of
   `vw`'s dependency resolver all exist in forms designed to be
   statically analyzable.
2. **Establish a unified multi-language LSP for the HDL workflow.**
   `vw analyzer` is designed from day one as a multi-language language
   server. htcl is the first language wired up (and the focus of v1);
   VHDL is the planned second, initially via a `vhdl_ls` proxy and
   eventually via direct integration with Oxide's developing VHDL
   frontend. The architecture (a `LanguageBackend` abstraction and
   per-file dispatch) is in place from the initial analyzer phase even
   while only htcl is wired up. See "LSP design" for the full
   treatment.
3. Provide an ergonomic dialect of TCL ("htcl") for HDL workflow
   scripting, with first-class support for structured proc
   declarations, modules, and dependencies resolved via `vw`. This
   dialect is not typed in v1; the structural improvements
   (per-argument doc comments, attributes like `@default` / `@enum` /
   `@required`, real imports) deliver most of the value without
   committing to a type system before we know what shape it should
   take.
4. Execute htcl by talking to Vivado's built-in TCL interpreter over a
   pipe, with a thin TCL shim on the Vivado side.
5. Stay vendor-aware: Vivado first, but the architecture should
   accommodate Quartus and other backends later.

This is an alternative to `set_property -dict {...}` bag-of-strings IP
config, ad-hoc `source [file join $::ROOT ...]` module loading, and
Vivado's generally unpleasant interpreter as a development environment.

## Strategic context: complexity management

The underlying problem this project addresses is that today's IP
integration workflow is intrinsically lossy with respect to source
control. Integrating IP into a design currently means reading user
guides and architecture manuals, then mapping what's learned into
GUI-based configuration in Vivado. The connection between official
documentation and GUI configuration is ambiguous and — critically —
not reproducible from an engineering-process perspective. The
artifacts that end up in source control (TCL block design exports,
generated wrappers, project files) don't capture how those
configurations were created or why specific parameterizations were
chosen. There's nowhere in the workflow to record rationale, no way to
review configuration changes the way code is reviewed, and no
mechanism to evolve a design over years and remember why it looks the
way it does.

htcl is a complexity-management tool first, and a TCL replacement
second. Configuration becomes textual source code: reviewable,
diffable, doc-commentable, version-controlled, and analyzed by
tooling. Rationale lives next to the configuration it explains. The
artifact in source control is the authoritative record of *what was
chosen and why*, not a lossy projection of decisions made in a GUI.

### Conceptual layering: specification, interface, instantiation

These are three distinct things. Confusing them leads to bad design
decisions, so they're named explicitly here:

**IP specification (IP-XACT).** Describes what an IP *is* — its full
parameter space, the parameterized port set, the parameterized memory
map, and the relationships between configuration choices and the
resulting structure. The specification is static; it covers all valid
configurations. IP-XACT is the format vendors use for this.

**Configuration interface (htcl).** A means to invoke an IP at a
specific configuration, with human ergonomics. An htcl wrapper is a
proc whose arguments are the IP's parameters and whose body emits the
underlying `create_ip` / `set_property` calls. The htcl proc *is not*
a description of the IP — it's a means to pick a configuration and
record the rationale for that pick. Different layer entirely from
IP-XACT.

**Instantiation (RTL + memory map).** What you get when you run a
configuration interface at chosen parameter values: a specific VHDL
entity (the wrapper Vivado generates) and a specific memory map (RSF,
in our case). These are the concrete artifacts at one configuration.

The lifecycle is:

1. *Specification* (IP-XACT, authored by the IP vendor): all valid
   configurations and their resulting structure.
2. *Configuration interface* (htcl wrapper, generated from IP-XACT by a
   sideband tool): the ergonomic surface for picking a configuration.
3. *Configuration choice* (htcl call site, hand-written by the
   engineer): specific parameter values, with doc comments capturing
   rationale, source-controlled and reviewed.
4. *Instantiation* (generated VHDL + RSF, produced by running htcl
   through Vivado): the actual artifacts at the chosen configuration.

htcl sits squarely at layer 2, with call sites at layer 3. It does
*not* attempt to subsume IP-XACT (layer 1) or replace generated RTL
and RSF (layer 4). The value of htcl is in giving layer 3 a
first-class, source-controlled, tool-analyzable form.

### The Vivado-team pitch

There is an active conversation with the Vivado team about Xilinx
publishing htcl configuration interfaces alongside the IP-XACT they
already publish. The pitch:

> IP-XACT is your specification format and stays the source of truth.
> What's missing is a published *configuration interface* — a layer
> where engineers can record what configuration they chose and why, in
> a form that is source-controllable, reviewable, and analyzable by
> tooling. Today engineers do this in GUIs, and the resulting TCL
> dumps don't capture intent. htcl wrappers, generated from your
> IP-XACT, fill that gap. You keep the specification; we get a
> rationale-preserving configuration layer that bridges to your
> existing pipelines unchanged.

This is a smaller, more defensible pitch than "replace IP-XACT with
htcl." htcl complements IP-XACT; it doesn't compete with it. The
showcase that earns the conversation is a set of generated htcl
wrappers that a Vivado engineer would be happy to publish — ergonomic,
documented, idiomatic.

### IP as distributed packages

IP configuration interfaces live in packages that vw resolves as
ordinary dependencies. A Xilinx-published `xilinx-ip` package
(generated from Xilinx's IP-XACT) contains `.htcl` wrappers for each
IP. A custom-IP repository at Oxide ships its own htcl wrappers,
generated from its own IP-XACT. A third party publishes wrappers for
their IP. Consumers add the relevant package to `vw.toml` and `src
@xilinx-ip/axis_register_slice` works the same as any other import.

There is no IP database, no central registry, no special-case
infrastructure. Repositories of htcl distributed through vw
dependencies *are* the catalog of available configuration interfaces,
decentralized by construction. This matches how the Rust crate
ecosystem works and how Oxide's existing vw-managed VHDL dependencies
work.

### Scope of htcl as a configuration interface

htcl describes how to *invoke* an IP. It does not describe the IP's
ports, the IP's memory map, or the IP's parameterized structure —
those are IP-XACT's job, and the artifacts at any specific
configuration come out the other side as generated RTL and RSF.

What an htcl wrapper proc declares:

- Parameter names, types, defaults, constraints, and inter-parameter
  dependencies — the configuration interface itself.
- Doc comments on each parameter (sourced from IP-XACT descriptions
  when generated).
- Doc comments on the wrapper as a whole.

What an htcl wrapper proc *emits* (in its body):

- `create_ip` and `set_property` calls that hand the configuration
  choice to Vivado.
- Optionally, directives that influence Vivado's wrapper generation
  (see "Wrapper documentation" below).

What an htcl wrapper proc does *not* contain:

- Port lists. The ports of any specific instantiation come from the
  generated VHDL/Verilog wrapper. The space of possible ports across
  all configurations is in the IP-XACT specification.
- Memory maps. The register interface of any specific instantiation
  comes from RSF generated by an IP-XACT-aware pipeline (see "RSF
  generation" below). The space of possible memory maps is in the
  IP-XACT specification.

This scope is the point: htcl is small, focused on the human-ergonomic
configuration layer, and stays out of the description and
instantiation layers where IP-XACT and generated artifacts already
serve well.

### RSF generation

Software needs to know the register map of any IP it talks to. RSF is
Oxide's register-spec format and the natural target for this
information.

The RSF for a specific IP instantiation is a function of two inputs:
the IP-XACT specification (which describes the parameterized memory
map) and the chosen configuration values (which pin the parameters).
The pipeline is:

```
IP-XACT spec + chosen parameter values  -->  RSF for this instance
```

This pipeline is not part of htcl, vw, the analyzer, or the REPL. It
is its own tool (call it `ipxact2rsf` or whatever the team names it)
that reads the IP-XACT spec, takes the chosen configuration values
(extracted from the htcl call site, or passed in directly, or queried
from Vivado post-instantiation), and emits RSF.

What htcl contributes to this pipeline: it is the place where the
configuration values are pinned down in a source-controlled form. The
RSF generator can extract those values by reading the htcl call site,
by running the htcl through to Vivado and querying the instantiated
IP, or by both. The mechanics are for that tool to decide; htcl just
needs to ensure the configuration is recoverable.

### Wrapper documentation

Vivado's generated VHDL/Verilog wrappers around configured IP are
notoriously undocumented. Port semantics, parameter effects, and
intended usage patterns are absent from the wrapper file. This is a
real obstacle to using IP correctly and reviewing changes to its
configuration.

The information flow we want:

```
htcl wrapper proc (doc comments on parameters, derived from IP-XACT)
  --> create_ip with directives capturing those docs
  --> Vivado wrapper generator (would need to honor the directives)
  --> generated VHDL with carry-through documentation
```

The first step we control (htcl carries the docs). The last step we
want (Vivado emits documented wrappers). The middle step is the open
question: what mechanism gets the doc strings from `create_ip`
arguments into Vivado's wrapper-generation output? Possibilities
include TCL directives Vivado already supports (likely insufficient),
TCL directives Xilinx would need to add (this is what we'd pitch), or
post-processing of generated wrappers by a separate tool (works but
fragile).

Worth pitching to Xilinx alongside htcl adoption: a documented
mechanism for `create_ip` to accept doc strings that flow into the
generated wrapper. Independently useful even for users who never adopt
htcl.

## Why this lives in `vw`

`vw` is already structured library-first: `vw-lib` is the core, the `vw`
CLI is a thin clap-based shim, and `vw-lib` is already consumed by
external tools (e.g., the remote build service for Vivado projects). This
is the same architectural pattern as Cargo's relationship to
rust-analyzer: one library underneath, thin CLIs on top, all tools sharing
the manifest, lockfile, and resolved dependency graph.

Adding htcl-language support, an analyzer, and a REPL as additional
subcommands of `vw` (or as sibling binaries that share `vw-lib`) gives us:

- **One manifest, one lockfile, one cache.** `vw.toml` and `vw.lock`
  cover VHDL and htcl dependencies uniformly. Packages can ship both
  languages from the same git source, versioned together (as
  discussed in earlier design conversations: a package version is a
  single value across all languages it contains).
- **One LSP serving both languages.** `vw analyzer` is designed from
  day one as a multi-language language server. htcl is wired up
  natively from the initial analyzer phase; VHDL arrives in a
  subsequent phase via a `vhdl_ls` proxy, with eventual replacement
  by Oxide's developing VHDL frontend. The user-facing surface (one
  LSP per workspace) is stable across that transition. See "LSP
  design" for the architecture.
- **Cross-language analysis.** An htcl file that wraps a user VHDL
  entity and the VHDL file defining that entity live in the same
  workspace, analyzed by the same tool through a shared backend
  abstraction. Go-to-definition can cross language boundaries.
- **Shared dependency resolution.** No new resolver, no new fetch
  logic, no new cache layout. We add an htcl file-selection layer on
  top of `vw-lib`'s existing dependency model, but the resolution
  mechanism is unchanged.
- **One mental model for users.** They learn `vw` once and get
  everything.

The model: `vw analyzer` is the LSP (modeled on `rust-analyzer`'s
relationship to Cargo). `vw repl` is the interactive htcl shell. `vw run`
(or similar) executes an htcl script against a Vivado worker. All of
these are thin subcommands over `vw-lib` plus a new `vw-htcl` crate that
holds the htcl-specific analysis.

## Architecture overview

```
                                    +-----------------------------+
                                    | vw-lib (existing)           |
                                    | - manifest / lockfile       |
                                    | - dependency resolver       |
                                    | - cache management          |
                                    | - VHDL file selection       |
                                    +--------------+--------------+
                                                   |
                       +---------------------------+---------------------------+
                       |                           |                           |
                       v                           v                           v
            +----------+-----------+   +-----------+----------+    +-----------+----------+
            | vw CLI (existing)    |   | vw-htcl (new)        |    | other vw-lib users   |
            | vw add / update /    |   | - htcl parser        |    | (remote build svc,   |
            | test / etc.          |   | - module resolver    |    |  future tools)       |
            +----------------------+   | - signature check    |    +----------------------+
                                       | - htcl -> Vivado     |
                                       |   TCL emission       |
                                       +-----------+----------+
                                                   |
                  +--------------------------------+--------------------------------+
                  |                                |                                |
                  v                                v                                v
        +---------+----------+        +------------+---------+         +------------+---------+
        | vw analyzer (new)  |        | vw repl (new)        |         | vw run (new)         |
        | LSP server, serves |        | interactive htcl     |         | execute htcl script  |
        | VHDL + htcl from   |        | against Vivado       |         | against Vivado       |
        | one process        |        | worker               |         | worker               |
        +--------------------+        +----------+-----------+         +----------+-----------+
                                                 |                                |
                                                 +--------------+-----------------+
                                                                |
                                                                v wire protocol (newline-
                                                                  delimited commands +
                                                                  structured responses)
                                                                |
                                                  +-------------+--------------+
                                                  | Vivado process             |
                                                  | (long-lived)               |
                                                  | - vivado-shim.tcl          |
                                                  |   (small TCL layer that    |
                                                  |    wraps commands and      |
                                                  |    emits JSON for          |
                                                  |    structured results)     |
                                                  +----------------------------+
```

Key invariants:

- `vw-lib` is unchanged in spirit; we add new crates beside it
  (`vw-htcl`, `vw-analyzer`, `vw-repl`, etc.) rather than restructuring
  what exists.
- All language semantics live in Rust. The Vivado shim is a dispatcher
  and serializer, nothing more.
- The wire protocol is newline-delimited commands in, structured (mixed
  text/JSON) responses out. Hand-written; not RPC-framework-heavy.
- Vivado runs as a long-lived worker process. Cold-start is too expensive
  for the hot path.
- The Vivado-driving binaries are self-contained; no Vivado-specific
  linking. Distribution is one (or a few) Rust binaries plus the shim TCL
  file.

## Language design

### Naming

Working name for the dialect: `htcl`. This is the language name, used as
the file extension (`.htcl`) and as the LSP language identifier. It is not
a separate tool — htcl is a thing `vw` supports, alongside VHDL.

Open question: final name. Avoid `tcl` in the name to reduce confusion
about it being a TCL implementation; it isn't.

### Relationship to TCL

htcl is **not** a TCL superset in the way TypeScript is a JS superset.
Existing Vivado TCL files are not valid htcl. The reasons:

- We want a structured proc-argument grammar that vanilla TCL can't parse.
- We want static module imports (`src` / `use`) that aren't `source`.
- We want to reject TCL features that defeat static analysis (`upvar`,
  `uplevel`, `trace`, dynamic command rewriting).

However, htcl emits TCL when talking to Vivado, and users can drop down to raw
TCL via an escape hatch for things htcl doesn't model. Existing TCL scripts can
be `source`d through the shim if needed.

### Proc grammar

Procs declare structured arguments with per-argument doc comments and attributes:

```htcl
proc axis_interface {
  ## Add a TKEEP sideband signal. Indicates valid bytes in the beat.
  @default(0)
  has_tkeep

  ## Add a TLAST sideband signal. Indicates the last beat in a packet.
  @default(1)
  has_tlast

  ## Width of TDATA in bytes.
  @default(8)
  @enum(1, 2, 4, 8, 16, 32, 64, 128)
  tdata_num_bytes

  ## Width of TUSER in bits. Only meaningful if has_tuser is set.
  @default(0)
  @requires(has_tuser)
  tuser_width
} {
  # body: emits CONFIG.* set_property calls
}
```

Call site uses keyword arguments:

```htcl
set lrq_request [axis_interface -has_tkeep 1 -tdata_num_bytes 128]
```

Attribute set for v1 (extensible):

- `@default(value)` — value if omitted
- `@required` — error if omitted
- `@enum(a, b, c)` — value must be one of these
- `@range(min, max)` — numeric bounds
- `@requires(other_arg)` — dependency between args
- `@conflicts(other_arg)` — mutual exclusion
- `@deprecated("message")` — soft warning

Open question: should attributes go before or after the doc comment, or be
order-insensitive? Recommend: doc comments first, then attributes in any order,
then the argument name. Matches Rust/TypeScript conventions.

### Module system

Replace `source` with `src` (or `use` — see open questions):

```htcl
src common/project        # relative to current file's directory
src /opt/xilinx/lib/foo   # filesystem-absolute (use sparingly, non-portable)
src @quartz/ip/bacd       # named dependency, resolved via vw.toml + vw.lock
```

Resolution rules:

- Leading identifier: relative to the directory of the importing file.
  Subdirectory traversal allowed (`src ip/cips` is fine).
- Leading `/`: filesystem-absolute. Permitted but discouraged; lint warning
  in v1, since these break across machines.
- Leading `@name/`: resolved via `vw.toml`'s `[dependencies.name]` entry.
  The cached path comes from `vw-lib`'s resolution (which reads `vw.lock`
  and the cache layout under `~/.vw/deps/`). The `@name` prefix means the
  same thing as it does to VHDL consumers of vw — same dependency entry,
  same commit, same cache directory.
- No upward traversal (`../`) in v1. Force cross-tree references to be
  absolute (filesystem or named).

Extension is implicit: `src foo/bar` resolves to `foo/bar.htcl` (or
whatever extension we settle on). Exactly one extension is recognized;
ambiguity is an error.

Idempotence: a module is loaded at most once per interpreter run, keyed by
canonical (realpath'd) file path. Repeated `src` calls are no-ops.

The "project root" — the base for filesystem-absolute imports' meaning of
the manifest, and the directory the analyzer walks for workspace symbols —
is the directory containing `vw.toml`. Same convention as for VHDL.

Open question: namespace semantics. Does `src foo/bar` populate
`foo::bar::*`, or do top-level definitions land in the global namespace as
they would with `source`? Recommend: top-level definitions are scoped to
the module's namespace by default, with an explicit `export` list
controlling what's visible. Importers use bare names after `src` (the
imports of the module are pulled into the importer's namespace). This is
the bigger semantic change; if too invasive for v1, fall back to
global-namespace semantics and add scoping later.

### Types — defer until we have usage experience

Types are out of scope for v1, and we should be in no hurry to add them. The
proc grammar, module system, and dependency manager are the highest-value
features and don't require a type system to be useful. We need real usage
experience with htcl-without-types before we can design types that pay for
themselves.

The risk of adding types prematurely is well-attested across other
ecosystems: type systems designed before the language has settled tend to
encode the wrong abstractions, become hard to evolve, and impose annotation
overhead that the underlying use cases don't justify. Better to live without
them, collect concrete cases where they would have caught real bugs or
documented intent better, and design to those cases later.

If types do land eventually, they will be:

- Optional and gradual. Unannotated code keeps working.
- Focused on HDL-specific concerns where the leverage is clear (likely
  candidates: IP handle types so CONFIG.* completion works without flow
  analysis, units like Hz/MHz/ns, bit widths). General-purpose typing of
  TCL values is not the goal.
- Driven by accumulated evidence, not speculative design.

The LSP's type-directed CONFIG.* completion (see LSP design section) is
achieved in v1 via flow tracking of IP handles, not a user-facing type
system. The user writes `set lrq [create_ip -name axis_register_slice ...]`
with no annotations; the analyzer infers that `$lrq` holds an
`axis_register_slice` handle. This is a narrow, internal analysis — not a
type system users interact with — and it covers the headline IP-completion
case without committing to broader type design.

## Dependency management

Dependency management is `vw-lib`. We do not design a new manifest, a new
lockfile, a new resolver, or a new cache. We extend the existing model in
the minimal ways htcl needs.

### Manifest and lockfile

`vw.toml` and `vw.lock`, as they exist today. Reference:
[vw README](https://github.com/oxidecomputer/vw/blob/main/README.md).

Each dependency entry already specifies `repo` + `branch`/`commit`/`tag`
and a `src` selector for VHDL files. For htcl consumption, we add an
optional `htcl` selector alongside `src`:

```toml
[dependencies.quartz]
repo = "https://github.com/oxidecomputer/quartz"
branch = "main"
src = "hdl/ip/vhd"          # VHDL files (existing; consumed by VHDL flow)
htcl = "hdl/ip/htcl"        # htcl files (new; consumed by vw-htcl)
recursive = true
```

Either or both selectors may be present. A package that ships only htcl
omits `src`; a package that ships only VHDL omits `htcl`; a package that
ships both (the common case for shared IP wrappers) has both.

Open question: whether to keep them as separate keys (`src` / `htcl`) or
generalize to a polymorphic selector keyed by file type. The separate-keys
version is the smallest change to vw-lib and the most explicit; keep it
unless there's a reason to generalize. If we later add other languages
(SystemVerilog, Quartus TCL), we add more keys.

### Versioning model (unchanged from vw)

- A package version is a single value across all languages it contains.
  VHDL and htcl contents of `quartz` ship together at the same commit; no
  per-language versions.
- `vw.lock` pins exact commit SHAs, not tags.

This was previously called out as a design decision; it is, but it's
already the model vw uses, so there's nothing to design.

### Resolution and fetch (unchanged from vw)

- `vw update` fetches and locks. Same command, same semantics, now also
  resolves htcl dependencies if `htcl` selectors are present.
- Cache at `~/.vw/deps/<name>-<commit>/`, unchanged.
- Authentication defers to git, unchanged.

### What `vw-htcl` does on top of `vw-lib`

A thin file-selection and module-resolution layer:

- Given a resolved dependency from `vw-lib`, find the htcl files within
  it using the `htcl` selector. Same selector semantics as the existing
  `src` field (directory / single file / glob).
- Build an index from `@name` to the resolved dependency's htcl root
  directory.
- The htcl module resolver consults this index when it sees `src
  @name/path`.

This is the only dependency-related code htcl needs to write. Everything
upstream of "I have a cache directory for `@quartz`" is already done.

### Path dependencies

`vw` currently supports git-sourced dependencies. For monorepo/sibling-
checkout development loops, htcl's plan called for `path = "../foo"`
dependencies. Confirm whether `vw-lib` currently supports this; if not,
adding it is a small extension that benefits both VHDL and htcl
consumers. Likely worth doing in `vw-lib` itself rather than as
htcl-specific behavior.

### Coordination questions for `vw-lib`

Things to confirm before phase 1 implementation, and possibly upstream
changes to schedule:

1. **Does `vw-lib` expose a stable Rust API for "give me the resolved
   path for dependency X"?** Yes (per the existing remote-build-service
   consumer), but confirm the exact shape — a `Resolved { name, commit,
   path, src_files }` struct, or similar — so the htcl resolver can
   consume it cleanly.
2. **Is the `src`-selector logic factored well enough to reuse for `htcl`
   selectors?** Ideally yes; the directory / single-file / glob handling
   is generic and shouldn't be reimplemented.
3. **Path dependencies:** present, absent, or in progress?
4. **Workspace concept:** if `vw.toml` ever grows multi-package workspace
   support (cargo-style), how does that interact with htcl? Likely fine,
   but worth a thought.

## Wire protocol

### Transport

Vivado is spawned once per workflow run (or per LSP session) and stays alive.
htcl talks to it over stdin/stdout pipes. No sockets, no daemon, no
multi-user complexity in v1.

### Request format

Newline-delimited commands. Each command is a JSON object:

```json
{"id": 42, "op": "eval", "tcl": "set_property -dict {CONFIG.HAS_TKEEP 1} $lrq"}
{"id": 43, "op": "eval_structured", "tcl": "report_property -all $cell"}
```

`id` is a monotonic request ID for matching responses.

Two ops in v1:

- `eval`: run the TCL, return the result as a string (or error info).
- `eval_structured`: run the TCL through a wrapper that emits JSON for known-
  structured commands. The shim has a dispatch table from command name to
  wrapper function.

### Response format

```json
{"id": 42, "ok": true, "result": ""}
{"id": 43, "ok": true, "result": {"CONFIG.HAS_TKEEP": "1", ...}}
{"id": 44, "ok": false, "error": {"message": "...", "code": "...", "info": "..."}}
```

For `eval_structured`, `result` is the JSON shape produced by the per-command
wrapper. For `eval`, it's a string.

### Vivado-side shim

A small TCL file (`vivado-shim.tcl`) loaded into Vivado at worker startup. It:

- Reads newline-delimited JSON from stdin.
- Dispatches to a handler per `op`.
- For `eval`, calls `uplevel #0 $tcl`, captures result or error, emits
  response.
- For `eval_structured`, parses the command name from the TCL, looks up a
  wrapper, runs the wrapper to produce JSON.
- Wrappers are hand-written per command family. Start with: `report_property`,
  `report_timing` (summary), `get_cells` / `get_pins` / `get_nets` /
  `get_clocks` lists, `list_property`. Grow as needed.

Most commands don't need a structured wrapper. Default to passthrough as a
string; opt into structure for the commands where text parsing on the Rust
side would be painful.

### Batching and fencing

v1: one request, one response. No batching. If round-trip latency becomes a
measured bottleneck, add a `batch` op that runs a list of commands with
fence semantics (stop on first error, or run-all-collect-errors).

## LSP design

LSP is a first-class concern, not a feature bolted on late. This
section describes the design in detail because LSP quality is the primary
goal of the project, and because the rest of the language design either
serves the LSP or constrains it.

### Scope: a multi-language LSP, htcl first, VHDL next

`vw analyzer` is designed from the start as a multi-language LSP for
the entire HDL workflow, not as an htcl-only LSP that might grow VHDL
later. The bulk of the initial implementation focus is htcl — that's
where the new language design and the headline complexity-management
features live — but the LSP server architecture, the workspace model,
and the configuration shape all assume a multi-language future and
must accommodate it from day one.

The motivating reality:

- Oxide's HDL work spans both htcl (this project) and VHDL (extensive
  existing codebase, the larger of the two by volume).
- Today, VHDL editor support comes from `vhdl_ls`, the open-source
  VHDL language server, configured via `vhdl_ls.toml` that `vw`
  generates. This works well and isn't going anywhere short-term.
- Long-term, Oxide is developing its own VHDL frontend and synthesizer
  as part of a complete VHDL stack. The eventual goal is for `vw
  analyzer` to integrate directly with that frontend, replacing
  `vhdl_ls`.

The path: `vw analyzer` serves htcl natively from day one, and
provides VHDL support initially by proxying to `vhdl_ls`, eventually
by integrating with the Oxide VHDL frontend. The user-facing surface
(one LSP serving both languages from a unified `vw.toml` workspace)
is stable across that transition; the implementation under the hood
changes.

Two consequences for the htcl-focused work in this plan:

1. The LSP server architecture is multi-language from phase 3, even
   while only htcl is wired up. The language-backend abstraction
   exists; it has exactly one implementation initially.
2. The htcl-side analysis must surface cross-language queries through
   that abstraction rather than directly to a specific VHDL
   implementation. This keeps the abstraction honest from day one and
   means the eventual swap from `vhdl_ls` to the Oxide frontend
   doesn't ripple into htcl-side code.

### Architectural principle: one source of truth per language

The LSP is **not** a separate analyzer with its own parser and signature
checker. It is the same code as the CLI, exposed over the LSP protocol.
For htcl, the parser, name resolver, signature checker, and
(eventually) any type analysis are written once and used by both `vw
run` / `vw check` and `vw analyzer`. For VHDL (later), the same
discipline applies via whichever backend serves VHDL at the time.

This matters because the dominant failure mode of language tooling is
divergence between "what the compiler does" and "what the IDE shows."
The moment they're separate implementations, they drift, and users
learn not to trust the IDE. Sharing the implementation is the only
durable fix.

Concretely:

- All htcl semantic analysis lives in the `vw-htcl` crate, consumed by
  every subcommand that needs it (`vw run`, `vw check`, `vw analyzer`,
  `vw repl`).
- VHDL analysis lives behind a language-backend abstraction (see
  below). Initially: `vhdl_ls` proxy. Later: direct integration with
  the Oxide VHDL frontend. htcl-side code talks to this abstraction
  for cross-language queries, never to a specific VHDL implementation.
- `vw analyzer` is the LSP server process. It does protocol plumbing
  (LSP request/response, text-sync, capability negotiation), dispatch
  by file type, and cross-language coordination. It contains minimal
  language logic of its own.
- `vw check` runs the same analyses the LSP runs on save, with the
  same diagnostics. CI uses `vw check`; editors use `vw analyzer`.
  Same diagnostics either way.

### Language backend abstraction

The LSP server dispatches per-file based on extension and routes
requests to a language backend. Each backend implements a common
trait (working name `LanguageBackend`):

```rust
trait LanguageBackend {
    fn diagnostics(&self, file: FileId) -> Vec<Diagnostic>;
    fn hover(&self, file: FileId, pos: Position) -> Option<Hover>;
    fn completion(&self, file: FileId, pos: Position) -> Vec<CompletionItem>;
    fn definition(&self, file: FileId, pos: Position) -> Vec<Location>;
    fn document_symbols(&self, file: FileId) -> Vec<DocumentSymbol>;
    // ... cross-language query surface, see below ...
    fn find_symbol(&self, query: SymbolQuery) -> Vec<SymbolInfo>;
}
```

(Exact shape is for implementation to decide; the point is the
abstraction exists from day one.)

Initial implementations:

- `HtclBackend` — uses `vw-htcl` directly. Native, in-process.
- `VhdlBackend` (initial) — `vhdl_ls` proxy. Spawns `vhdl_ls` as a
  subprocess and forwards file-scoped requests over the standard LSP
  protocol. The proxy generates `vhdl_ls.toml` from `vw.toml` (which
  `vw` already does for standalone editor support) and points the
  subprocess at it.

Later, the `VhdlBackend` is replaced by a direct integration with the
Oxide VHDL frontend — same trait, different implementation. Call
sites in `vw analyzer` and in `HtclBackend` don't change.

The cross-language query surface (`find_symbol` above, plus whatever
else accumulates as cross-language features grow) is the contract
that lets htcl ask "is there a VHDL entity named X?" without caring
which backend answers. For the `vhdl_ls` proxy, `find_symbol` is
implemented by querying `vhdl_ls` with `workspace/symbol` and
translating the results. For the Oxide frontend, `find_symbol` is a
direct API call. Same shape from the htcl side.

### Cross-language analysis (htcl ↔ VHDL)

A different case from the IP-XACT-generated wrapper flow described in
Strategic Context: hand-written htcl that wraps user VHDL entities. A
team's own VHDL design has entities with generics, and an htcl proc
gives those entities ergonomic instantiation interfaces with doc
comments, defaults, and validation. Because `vw analyzer` sees both
languages (through the backend abstraction), it can offer
cross-language navigation between the htcl wrapper and the underlying
VHDL entity:

- **Go-to-definition from htcl into VHDL.** An htcl proc that wraps a
  VHDL entity — e.g., `instantiate_uart` taking parameters that map
  to the `uart` entity's generics — can declare its target entity via
  an attribute (likely `@vhdl_entity(uart)`). When the user invokes
  go-to-definition on the proc call, the LSP server asks the VHDL
  backend for the location of entity `uart` and returns it. The
  htcl-side code that issues this query doesn't know whether
  `vhdl_ls` or the Oxide frontend answered.
- **Find references across languages.** "Find references" on a VHDL
  entity surfaces both VHDL instantiations (from the VHDL backend's
  references query) and htcl wrapper procs that target it (from the
  htcl backend's index of `@vhdl_entity` attributes).
- **Generic-to-argument mapping.** If an htcl wrapper declares which
  of its proc arguments map to which VHDL generics, the htcl backend
  queries the VHDL backend for the entity's generic list and checks
  for missing or extra mappings. Warns on drift when the entity's
  generic list changes.

Note: this is distinct from IP-XACT-generated wrappers. Those target
Vivado IP via `create_ip` and don't have a VHDL entity in the
workspace to navigate to — the entity comes out the other side as
generated RTL. Cross-language analysis applies to user-authored
htcl-over-VHDL, not to vendor-IP wrappers.

Open question: how aggressively to pursue cross-language features in
v1. The minimum is "go-to-definition from htcl into VHDL"; the rest
is nice to have. I'd recommend the minimum lands in v1 (it's the
headline demo for the multi-language model) and the more
sophisticated checks come after.

### Incremental analysis

A useful LSP must re-analyze on every keystroke. The analysis layer is
designed for this from the start, not retrofitted.

Approach:

- The unit of caching is the file (module). Parsing a file produces a syntax
  tree; resolving its imports produces a module-level binding. Files cache
  their parsed and resolved state, keyed by content hash.
- Cross-module analysis (resolving an import, looking up an external proc
  signature) reads from the cache. A change to a file invalidates that file
  and any file that depends on it, transitively.
- Consider `salsa` (the framework rust-analyzer uses) for memoization and
  invalidation. It's heavy machinery for a small project, but it solves
  exactly this problem and the alternative is hand-rolling the same thing
  badly. Decide after phase 0 whether to adopt it; for the smallest possible
  v1, a hand-rolled cache keyed on file mtimes is fine.
- Parsing should be tolerant of incomplete input. The user is in the middle
  of typing; the parser must produce a usable AST with error nodes rather
  than bailing on the first syntax error. This shapes the parser choice
  (see below).

### Parser

The parser is the foundation of every LSP feature. Built with
[`winnow`](https://docs.rs/winnow), the parser library used pervasively
across Oxide. Familiarity, code-review consistency, and shared idioms with
the rest of the codebase outweigh case-by-case evaluation of alternatives.

Requirements the implementation must meet within winnow:

- **Error-tolerant.** Recover from syntax errors and continue parsing. A
  half-typed proc declaration should still produce a tree where the rest
  of the file is analyzable. Winnow's `cut_err` and combinator-level
  recovery are the building blocks; design recovery points around
  statement boundaries (newline-terminated top-level forms, proc bodies,
  `src` statements).
- **Position-preserving.** Every node knows its source span. Winnow's
  `Located` adapter or equivalent span-tracking is used throughout. No
  AST node without a span.
- **CST-shaped, trivia-preserving.** The output is a concrete syntax tree
  that retains whitespace and comments, not a stripped abstract syntax
  tree. We need comments for doc-comment extraction and trivia for
  accurate formatting (`vw fmt` is a likely future feature). The AST
  layer used by name resolution and signature checking is derived from
  the CST.
- **Reusable across editor and CLI.** Same parser code runs in `vw run`,
  `vw check`, `vw analyzer`, `vw repl`. No editor-only or CLI-only
  variants.

Incremental reparse is deferred. Most htcl files will be small enough
that full reparse per edit is fast; revisit if measurement shows
otherwise. If incremental parsing becomes necessary later, the CST
boundary makes it tractable to swap in a different strategy for hot
paths without rewriting downstream analysis.

Open question: how to structure the CST → AST lowering. Two reasonable
shapes: (a) a single AST with optional trivia attached to nodes, or (b)
a separate AST that holds references back into the CST for source
positions. Pick after the first non-trivial grammar pass.

### Feature inventory

Each feature has acceptance criteria specific enough to be implementable.

#### Completion

What completion offers depends on cursor context. The completion system
needs a notion of "what kind of position is this," determined by the
surrounding syntax tree.

Positions and their completion sets:

- **Top-level statement.** Suggest: keywords (`proc`, `src`, `set`, control
  flow), in-scope procs, in-scope variables.
- **After `src `.** Suggest: relative module names (subdirectories and
  `.htcl` files reachable from the current file), `/` to start a filesystem
  path, `@name/` for declared dependencies. After `@name/`, suggest paths
  within that dependency.
- **Command position (start of a statement).** Suggest in-scope procs and
  Vivado builtins. For Vivado builtins, the suggestion source is a
  generated table from UG835 (see "Vivado builtins" below).
- **Argument position of a known proc call.** If the cursor is after
  `axis_interface ` and the next token is `-`, suggest the proc's keyword
  arguments. If the cursor is after `-has_tkeep `, suggest values
  appropriate to that argument's type / `@enum` set.
- **Inside a `$variable` reference.** Suggest in-scope variable names.
- **Inside an attribute (`@`).** Suggest known attribute names
  (`@default`, `@required`, `@enum`, etc.) and, where appropriate,
  their arguments.

Note: parameter completion on IP instantiation sites (the
highest-value HDL use case) is the same code path as proc-argument
completion above. An IP wrapper is an htcl proc; its parameters are
proc arguments with attributes; completion works the same way as for
any other proc. There is no special-case "IP property" completion.

Acceptance criteria:
- Completion responds in <50ms for files under 1000 lines.
- Completion items include `detail` (short type/signature info) and
  `documentation` (full doc comment) fields.
- Snippets supported for procs with required arguments — completing a proc
  call inserts the proc name plus placeholders for required keyword args.

#### Hover

Acceptance criteria:
- Hovering on a proc name shows the proc's doc comment, signature
  (arguments with their attributes), and source location.
- Hovering on a proc argument at a call site shows that argument's doc
  comment, default value, and any attributes (`@enum`, `@range`, etc.).
- Hovering on a `src` import shows the resolved file path and, if
  available, the module's top-level doc comment.
- Hovering on a Vivado builtin shows UG835-derived documentation.
- Hovering on an IP wrapper proc (imported from a vw package) shows
  its doc comment and per-parameter docs, same as any other proc. If
  the package was generated from IP-XACT, that documentation flows
  through unchanged.

#### Diagnostics

Diagnostics are produced by the same analyzer the CLI uses. The LSP just
ships them over the wire.

Categories:

- **Syntax errors.** Parse failures, recovered to the best position the
  parser can manage.
- **Unresolved imports.** `src foo/bar` where `foo/bar.htcl` doesn't exist.
- **Unknown procs.** Call to a name that isn't defined or imported.
- **Argument errors.** Unknown keyword argument, missing required argument,
  value outside `@enum` or `@range`, `@requires` / `@conflicts` violation.
- **Unused declarations.** Unused imports, unused local variables. Warning
  level, suppressible.
- **Deprecation warnings.** Call sites of procs marked `@deprecated`.

Note: there is no separate "IP property error" diagnostic category.
Unknown arguments to an IP wrapper proc are caught by the standard
"unknown keyword argument" check; out-of-range values are caught by
the standard `@enum` / `@range` check. The diagnostics machinery
doesn't distinguish IP wrappers from other procs.

Each diagnostic has: source range, severity, message, optional related
information (e.g., "this is the proc declaration whose required argument
you're missing"), optional code action (e.g., "add missing argument").

Acceptance criteria:
- Diagnostics update within 200ms of an edit.
- Every diagnostic has a precise source range, not just a line number.
- Diagnostics are stable: editing an unrelated part of a file doesn't
  cause diagnostics elsewhere to flicker.

#### Go-to-definition

- **Proc reference → proc declaration.** Across files, following imports.
- **Variable reference → assignment.** Within a scope; "definition" for a
  variable is its first assignment in the current scope or a containing
  scope.
- **`src` target → the imported file.** Open the imported `.htcl` file.
- **Vivado builtin → UG835 entry.** Either open a generated stub file with
  the documentation, or open the UG835 URL. Implementation-defined; the
  point is the user can find the docs.

#### Find references

- For procs, find all call sites and any explicit references (passing as a
  value, etc.).
- For variables, find all reads and writes in scope.
- For modules, find all `src` statements that import them.

Acceptance criteria:
- Find references on a proc returns results across the whole project,
  searching all `.htcl` files transitively reachable from the project root.
- Results include the source range and one line of context.

#### Rename

Lowest priority but high value when it works. Rename a proc, variable, or
module and update all references atomically.

Caveats:
- Renaming across the project boundary (into dependencies) is forbidden.
- Renaming requires the LSP to be confident about every reference; if any
  reference is ambiguous (e.g., dynamically constructed), abort with an
  error rather than rename incorrectly.

#### Document symbols and workspace symbols

- Document symbols: every proc, top-level variable, and module-level
  declaration in the current file. Used for the editor's outline view.
- Workspace symbols: same, across the project. Used for "go to symbol in
  project" pickers.

#### Code actions

A small set in v1, expanded over time:

- "Add missing required argument."
- "Remove unused import."
- "Convert raw `set_property -dict` to a structured IP configuration call."
  (Big one for migration off existing Vivado TCL.)
- "Extract selection to proc."

#### Formatting

`htcl fmt` is a separate CLI command; the LSP exposes it via the
`textDocument/formatting` request. Formatter implementation is a phase past
v1, but the architectural slot for it should exist from the start (the CST
must preserve enough information to reformat).

### Configuration completion on IP instances

The highest-impact LSP feature for HDL work is parameter completion at
IP instantiation sites. The user imports an IP from a vw dependency:

```htcl
src @xilinx-ip/axis_register_slice
# ...
set lrq [axis_register_slice -has_tkeep 1 -tdata_num_bytes |
                                                    ^cursor here
```

The analyzer offers completion for the IP's parameters (`tuser_width`,
`tdest_width`, etc.), with hover documentation, defaults, and `@enum`
constraint values pulled from the proc's declared signature.

Crucially: **this is the same code path as completion on any other
htcl proc.** The IP wrapper is a proc; the proc has structured
arguments with attributes (per the proc grammar); completion of those
arguments works the same as completion of arguments on a hand-written
proc. There is no special-case "IP property" subsystem in the
analyzer.

This is the architectural payoff of treating IP as ordinary htcl
packages distributed through vw dependencies: the LSP doesn't need to
know anything about IP-XACT, IP catalogs, or Vivado-specific
introspection. It just analyzes htcl.

Flow tracking of IP handles is still useful for downstream features —
"this variable is an instance of `axis_register_slice`, so when it's
passed to `connect_axis`, here's what we can validate" — but it's a
narrow extension of proc return-type tracking, not a separate
mechanism for IP. Defer until the cross-IP wiring story matures.

### Vivado builtins

htcl needs knowledge of Vivado's built-in TCL commands (`get_cells`,
`report_timing`, `current_design`, etc.) to provide completion and
hover for them. These aren't IP — they're the underlying Vivado
language surface htcl sits on top of.

Sources:

- UG835 (the Tcl Command Reference) parsed into a structured form. The
  doc has consistent enough structure to be machine-readable, though
  it's not trivial.
- `help <command>` output from a live Vivado, scraped at
  builtin-data-generation time.
- Hand-written annotations layered on top for things UG835 gets wrong
  or doesn't explain.

The result ships with vw (in a `vw-vivado-data` crate, or similar) as
a generated data file consumed by the analyzer. Regenerated per Vivado
release; the version targeted in `vw.toml` selects which data file is
used.

### LSP server implementation

- Crate: `vw-analyzer`, a binary crate. Invoked as `vw analyzer` from
  the `vw` CLI dispatcher, or directly via `vw-analyzer`.
- Framework: `tower-lsp` is the standard Rust LSP framework,
  well-maintained and used by rust-analyzer-adjacent projects. Use it
  unless there's a specific reason not to.
- Transport: stdio. The editor spawns `vw analyzer` and talks to it.
- Concurrency: file analysis runs on a worker thread pool; the protocol
  handler thread stays responsive to cancellation requests.
  Long-running analyses (full project re-resolution) are cancellable.
- Language scope: htcl and VHDL in one server process, dispatched
  per-file via the `LanguageBackend` abstraction (see "Language
  backend abstraction" above). htcl is wired up natively from phase 3;
  VHDL is wired up via the `vhdl_ls` proxy in a subsequent phase, with
  eventual replacement by the Oxide VHDL frontend. Cross-language
  queries are first-class through the backend trait.

### Editor integration

VS Code is the primary target. A minimal extension:

- Activates on `.htcl` and `.vhd`/`.vhdl` files, and on workspaces
  containing `vw.toml`.
- Spawns `vw analyzer` as the language server.
- Ships a TextMate grammar for htcl syntax highlighting (VS Code's
  native highlighting format). A tree-sitter grammar can come later if
  we want to support editors that consume those directly (Zed, Neovim
  with `nvim-treesitter`). The editor-side highlighting grammar is
  separate from the LSP parser; the LSP uses winnow, while highlighting
  is whatever the editor consumes.
- Provides commands: "vw: restart analyzer," "vw: update dependencies,"
  "vw: show IP property reference."

Open question: relationship to any existing vw VS Code extension. If one
exists, extend it; if not, this is a new package. Either way, one
extension per project, not separate VHDL and htcl extensions.

Other editors (Neovim, Emacs, Helix, Zed) get LSP support for free via
their generic LSP clients; we don't ship extensions for them initially,
but configuration snippets in the README are a low-cost way to support
them.

### LSP testing strategy

LSP regressions are easy to ship and hard to notice. Test infrastructure
from the start:

- Snapshot tests for analysis output. Each test is a small `.htcl` fixture;
  the expected output (diagnostics, symbol tables, completions at marked
  positions) is a checked-in snapshot. Mismatches fail the test.
- End-to-end LSP tests using a test client that speaks the protocol. Verify
  that a `textDocument/completion` request at a given position returns the
  expected set of items.
- Don't test through VS Code. Test the LSP server directly; the VS Code
  extension is a thin enough wrapper that manual smoke-testing is fine for
  it.

### Phasing within the LSP work

The analyzer is built incrementally alongside the rest of the language,
with `vw analyzer` introduced as a real subcommand at phase 3 and
growing features as later phases land:

- **Phase 3 (analyzer initial, htcl only):** `vw analyzer` binary
  exists. `LanguageBackend` abstraction in place with `HtclBackend` as
  the sole implementation. Provides diagnostics, document symbols,
  go-to-definition for `src` targets and proc references, hover for
  proc docs, completion for proc arguments. Crucially, this includes
  parameter completion on IP wrapper procs imported from vw packages
  — the headline IP-completion case falls out of the proc-argument
  completion path. This is the point where the analyzer is genuinely
  useful for htcl.
- **Phase 4 (structured wire responses):** No direct analyzer impact,
  but enables typed result handling that the REPL builds on.
- **Phase 5 (VHDL via vhdl_ls proxy):** `VhdlBackend` lands as a
  proxy to `vhdl_ls`. `vw analyzer` now serves both languages from a
  single process; the user-facing multi-language LSP surface is in
  place.
- **Phase 6 (cross-language):** htcl ↔ VHDL go-to-definition and find
  references, building on the backends from phase 5.
- **Phase 8 (polish):** Find references across the workspace, rename,
  workspace symbols, code actions, performance tuning, editor
  extension packaging.

Note: the analyzer benefits from being built alongside language
features rather than after them. Each language feature (modules, proc
grammar, cross-language) lands its analyzer support in the same phase
that introduces the feature. The "phase 8 polish" pass is for
analyzer-only features that don't have an underlying-language
counterpart.

The eventual replacement of the `vhdl_ls` proxy with direct Oxide VHDL
frontend integration is a "Later" item (see implementation plan).
Because the swap stays within the `LanguageBackend` abstraction,
no htcl-side code needs to change when it happens.

### Non-goals for the LSP

- Debugging protocol (DAP). Out of scope; debugging happens inside Vivado.
- Semantic tokens for syntax highlighting. Editor-side grammars are cheaper
  and good enough.
- Inlay hints. Possibly later; not needed for v1.
- Refactorings beyond rename and the small code-action set listed above.

## REPL design

The REPL is, architecturally, the same product as the analyzer with a
different presentation layer. The LSP serves an editor; the REPL serves
a TUI. Both query the same `vw-htcl` analysis. This isn't a coincidence
to exploit — it's the design.

The traditional captive-CLI help model (Vivado's `help foo`, Cisco IOS's
`?`) was a 1990s answer to "I don't have a graphics-capable terminal but
I have screen-clearing escape codes." It conflates discovery (what
exists?), reference (what does this do?), and navigation (where am I?)
into a single text-dump idiom that clutters scrollback and answers none
of those questions well. With a modern TUI and the analyzer's data
already on hand, we can do substantially better without reimplementing
anything.

Built with [`ratatui`](https://ratatui.rs/), the TUI library used
pervasively across Oxide. Line editing uses
[`reedline`](https://docs.rs/reedline) — Nushell's modern readline
replacement, well-suited to hint and menu rendering. Both choices are
Oxide-conventional rather than case-evaluated; same reasoning as winnow.

### Architectural principle: history hygiene

The defining UX commitment: anything the user explicitly ran (commands
and their results) belongs in scrollback. Anything that was a navigation
aid (completion menus, signature help, hover dialogs, help overlays)
does not. Navigation aids appear in transient overlays or inline
ghost-text and disappear when the user moves on. The scrollback is what
the user chose to do, not how they figured out what to do.

This is the failure mode of Vivado's REPL: a `help` invocation dumps
200 lines into history, and the user's actual work is buried. Ours
won't.

### Virtual document model

The REPL maintains an in-memory document representing the session:
successful evaluations are appended, and the current input line is
treated as the tail. The analyzer's queries operate on (document +
current input), with the cursor positioned within the current input.

Consequences:

- Variables and procs defined earlier in the session are in scope for
  completion, hover, and diagnostics on the current input.
- Diagnostics on the current line surface *before* the user submits it
  — a typo'd argument name is flagged inline, not after Vivado returns
  an error.
- Sourced modules contribute their definitions to the session document,
  so completion includes everything reachable from the import graph.

The same analyzer code that powers `vw analyzer`'s editor support
powers the REPL's interactive features. The analyzer doesn't know
whether it's serving an editor or a TUI.

### Feature inventory

#### Tab completion

The primary discovery mechanism. Triggered on Tab, optionally
auto-suggested as a menu after a short typing pause.

Completion sources match the LSP's: in-scope procs, proc arguments at
call sites (including arguments on IP wrapper procs imported from vw
packages), `@enum` values, variable names, module imports, and Vivado
builtins.

Rendered as a popup menu *below* the input line. Arrow keys navigate;
Enter or Tab accepts; Escape dismisses. The menu does not enter
scrollback.

#### Signature help while typing

When the user is partway through a proc call, a non-intrusive line
*below* the input shows the proc's signature with the current argument
highlighted. The current value's `@enum` or `@range` constraint is
shown alongside.

The signature line updates as the user types and disappears as soon as
they move past the call. Never enters scrollback.

#### Modal help overlay

Bound to F1 (or `?` if a more keyboard-friendly approach is preferred).
Pops a transient overlay — a centered panel or split-pane — with the
full documentation for the symbol under the cursor: proc signature,
all argument docs, attribute constraints, source location, related
procs. For IP wrappers, this surfaces the same documentation that's in
the proc's doc comments and parameter attributes — which (for
IP-XACT-sourced packages) carries the IP-XACT descriptions through to
the user.

Dismissed with Escape. Nothing lands in scrollback.

This is what Vivado's `help` command should have been: a brief takeover
of the screen that returns control unchanged.

#### Inline ghost-text suggestions

As the user types, faintly render the most likely completion in dim
text ahead of the cursor (fish-shell / Copilot style). Right-arrow or
Tab accepts. Any other key dismisses.

For HDL workflows where proc and argument names are long and
repetitive (`tdata_num_bytes`, `axis_register_slice`), this saves real
keystrokes. Reedline supports this natively.

#### Discoverable command palette

Bound to Ctrl-P (or similar). Opens a fuzzy-searchable overlay listing
in-scope procs, recent commands, and (optionally) workspace symbols.
Same data the LSP uses for `workspace/symbol`.

The failure mode of current Vivado REPLs is "I know I want to do X but
I don't remember what it's called." This is the fix.

#### Per-instance exploration

Dedicated mode for the common HDL workflow question: "I have an IP
instance; what are all its current properties and values in the live
Vivado design?" Triggered by `:describe <var>` or by a hotkey on a
variable in scope.

Renders a sortable, filterable table built from live `report_property`
data on the instance, joined with the IP wrapper proc's parameter
documentation (so each property has its description and constraints
visible). Navigable with arrow keys; Enter on a property opens its
full documentation. Escape closes.

Substantially better than `report_property` dumped into scrollback as
text.

#### Pretty-printed structured results

When `eval_structured` (phase 4) returns a typed result, the REPL
renders it as a navigable structure rather than a flat string. Timing
reports become collapsible trees; property dumps become tables; lists
of cells/pins/nets become selectable lists where each entry can be
hovered for details.

The text representation is still available — `:plain` or a config
option turns off pretty-printing for screencasts and pipe-friendly
output.

#### Lightweight text help fallback

A `:help foo` command (or similar) prints help to scrollback as plain
text. Useful for SSH over slow links, grepping history, screencasts,
and copying into chat/issues. The TUI overlay is the default for
interactive use; the text command exists for cases where the overlay
isn't what's wanted.

This is the one place we accept scrollback clutter, because it's the
user explicitly asking for it.

### Implementation notes

**Debouncing.** Analysis runs on keystrokes; if it takes more than
~20ms the UI feels laggy. Debounce completion and hover queries (fire
~50ms after input stability), and run analysis on a worker thread that
the ratatui frame loop polls.

**Cancellation.** A new keystroke invalidates the previous analysis
request. The analyzer is already cancellable (LSP requirement); the
REPL inherits that.

**History.** Persistent across sessions, stored in
`~/.local/state/vw/repl-history` (or platform-equivalent). Reedline
handles this.

**Multiline input.** htcl procs span multiple lines; the editor must
support multi-line buffers with proper indentation. Reedline supports
this; pair with the parser to detect when a buffer is syntactically
complete vs. needs more lines.

**Vivado worker lifecycle.** A REPL session corresponds to one
long-lived Vivado worker. Cold start happens at REPL launch (with a
spinner during the multi-second Vivado startup); the worker persists
until exit. `:restart` rebuilds the worker without exiting the REPL.

**Module hot reload.** If a sourced module changes on disk, the REPL
detects it (file watcher), re-sources, and updates the session
document. The user keeps any session-local definitions made after the
module was first loaded. Conflicts (same name now means something
different) are surfaced as warnings.

### Phasing within the REPL work

The REPL doesn't need to wait for every other phase to land. It can
ship with a meaningful subset early and grow:

- **Initial REPL (phase 7 below):** ratatui shell, reedline line
  editor, tab completion, signature help, history, multi-line input,
  Vivado worker integration, pretty-printed results (phase 4 has
  already landed by this point). Parameter completion on IP wrapper
  procs works the same as parameter completion on any other proc —
  no separate path.
- **`:describe` for live instances:** lands when wired up; depends on
  the structured-wire-response work from phase 4 to read live
  properties cleanly.
- **Polish phase (alongside LSP phase 8):** Command palette, modal
  help overlay, ghost-text suggestions, file-watcher-based module hot
  reload.

### Non-goals for the REPL

- Mouse interaction. Keyboard-only TUI. Mouse support is an
  accessibility win we can add later; not v1.
- Replacing the Vivado GUI. The REPL is for scripted/exploratory
  workflows; users who need waveform viewers and floorplanning still
  use Vivado proper.
- Persistent named sessions / tmux-style detach. Run inside tmux if
  you want that.
- Custom keybinding configuration in v1. Pick sane defaults; expose
  config later if requested.

## Implementation plan

The plan is organized around extending `vw` with new crates and
subcommands. The existing `vw-lib` and `vw` CLI are not restructured; we
add alongside.

New crates introduced over the phases:

- `vw-htcl` — htcl parser, AST, name resolution, signature checking,
  TCL emission. The language layer.
- `vw-vivado` — Vivado worker spawn/connect, wire protocol, embedded
  shim TCL. The execution layer.
- `vw-vivado-data` — generated database of UG835 builtin commands.
  Regenerated per Vivado release; not user-edited.
- `vw-analyzer` — LSP server. Binary.
- `vw-repl` — interactive shell. Binary.

Not in this project (separate downstream tooling):

- IP-XACT → htcl wrapper generation. A sideband tool that reads an
  IP's IP-XACT `component.xml` and emits an `.htcl` configuration
  interface (a wrapper proc whose parameters match the IP's
  parameters). Lives in its own repo, ships its own binary, produces
  vw-consumable packages. The output is ordinary htcl that vw doesn't
  need to know was generated. See "Strategic context" section.
- IP-XACT + configuration values → RSF. A separate tool that produces
  the register-spec file for a specific IP instantiation. Reads the
  IP-XACT memory map and the configuration values, emits RSF. Not in
  vw; see "RSF generation" in Strategic Context.

The `vw` CLI grows subcommands `run`, `check`, `repl`, `analyzer` (plus
existing `add`, `update`, `test`, etc.).

### Phase 0: skeleton

Goal: smallest end-to-end thing that proves the architecture.

- New crates `vw-htcl` and `vw-vivado` created in the vw repo.
- `vw run` subcommand added to the CLI dispatcher.
- htcl parser for a minimal subset: literals, variables, `set`, `proc`,
  command invocation, comments. No control flow yet. Built with
  [`winnow`](https://docs.rs/winnow); see the LSP design section's
  "Parser" subsection for the full rationale and requirements.
- Vivado worker spawn-and-connect logic.
- Vivado shim with `eval` op only.
- `vw run file.htcl` reads the file, sends each top-level command to
  Vivado, prints results.

Deliverable: `vw run hello.htcl` where `hello.htcl` is `puts "hello"`
prints `hello`.

### Phase 1: module system

Goal: `src` works.

- Use `vw-lib` to find the project root (location of `vw.toml`).
- Implement `src` with relative and filesystem-absolute resolution.
- `@name/...` resolution: query `vw-lib` for the dependency's resolved
  cache path, then index into it via the `htcl` selector.
- Module loading: parse file, execute top-level forms, track loaded set
  for idempotence.
- Decide and implement namespace semantics (see open question above).
- Coordinate `vw-lib` extensions: the `htcl` selector key in dependency
  entries; path dependencies if not already supported.

Deliverable: a multi-file project loads and runs, including imports from
a `vw`-managed dependency.

### Phase 2: proc grammar

Goal: structured proc declarations with attributes.

- Extend parser for the proc-arg grammar (doc comments, attributes,
  names).
- AST representation for procs with metadata.
- At call time, validate keyword args against the declared signature:
  required args present, no unknown args, `@enum` / `@range` /
  `@requires` / `@conflicts` checked.
- Generate a TCL-side `proc` that takes positional args in canonical
  order; callers pass keyword args, vw-htcl reorders them and emits a
  positional call.
- Introduce `vw check` subcommand that runs analysis and reports
  diagnostics without executing.

Deliverable: the `axis_interface` example from the language design
section works end-to-end with validation, and `vw check` flags malformed
calls.

### Phase 3: analyzer (LSP) — initial version

Goal: editor support lands as soon as the analysis is meaningful.

- `vw-analyzer` crate, `vw analyzer` subcommand.
- LSP server using `tower-lsp` over stdio.
- `LanguageBackend` trait introduced; `HtclBackend` is the only
  implementation initially. The dispatch by file extension is in
  place from the start (it just always routes to `HtclBackend`).
- Wire up the existing `vw-htcl` analysis through `HtclBackend`:
  diagnostics, document symbols, hover for proc docs, go-to-definition
  for `src` targets and proc references.
- Completion for proc arguments (using the signature data from phase
  2).
- VS Code extension stub: activates on `.htcl` and `vw.toml`, launches
  `vw analyzer`.

Deliverable: opening an htcl project in VS Code gives diagnostics,
hover, and basic completion. The LSP is genuinely useful for htcl
from this point forward; later phases add features and bring VHDL
into the same server.

### Phase 4: structured wire responses

Goal: avoid Rust-side TCL parsing for structured outputs.

- Add `eval_structured` op to wire protocol.
- Write Vivado-shim wrappers for the initial command set
  (`report_property`, `get_cells`-family, etc.).
- Rust-side types for the parsed results.

Deliverable: `report_property` returns a typed Rust value, not a string,
in the executor.

### Phase 5: VHDL via vhdl_ls proxy

Goal: bring VHDL into `vw analyzer` so it serves both languages from a
single process; the user-facing surface for a unified LSP is in place.

- `VhdlBackend` implementation that spawns `vhdl_ls` as a subprocess
  and proxies LSP requests for `.vhd` / `.vhdl` files.
- Generate `vhdl_ls.toml` from `vw.toml` (reuse the existing `vw`
  logic for this) and point the subprocess at it. Regenerate when
  `vw.toml` changes.
- File-type dispatch in `vw-analyzer` now routes htcl files to
  `HtclBackend` and VHDL files to `VhdlBackend`.
- Cross-language query surface (`find_symbol` etc.) implemented on
  `VhdlBackend` via `workspace/symbol` and related `vhdl_ls`
  queries.
- Cancellation, lifecycle, and error handling for the subprocess.
- Performance: confirm the proxy adds acceptable overhead. If
  noticeable, profile and optimize.

Deliverable: a single `vw analyzer` process serves htcl and VHDL.
Editors configured to use it see consistent behavior across both
languages without needing a separate `vhdl_ls` configuration.
Cross-language queries from htcl to VHDL work but aren't yet
user-facing (next phase wires up the htcl-side attributes).

### Phase 6: cross-language analysis

Goal: htcl ↔ VHDL navigation (the user-facing cross-language
features, building on phase 5's backend wiring).

- `@vhdl_entity(name)` attribute on htcl procs declaring the entity
  they wrap.
- `HtclBackend` resolves entity references by issuing `find_symbol`
  to `VhdlBackend`. Go-to-definition surfaces the resulting location.
- Find-references on a VHDL entity surfaces both VHDL instantiations
  (from `VhdlBackend`) and htcl wrappers (from `HtclBackend`'s index
  of `@vhdl_entity` attributes).
- Generic-to-argument mapping: optional in this phase, depending on
  how much work the `find_symbol` extension to "give me this entity's
  generics" turns out to be.

Deliverable: clicking through an htcl proc into its VHDL entity
works, in both directions.

### Phase 7: REPL

Goal: ship the REPL as a meaningful interactive environment. See the
dedicated "REPL design" section above for the full treatment.

- `vw-repl` crate, `vw repl` subcommand.
- Built with `ratatui` and `reedline`.
- Initial feature set (per the REPL phasing subsection): tab completion,
  signature help, persistent history, multi-line input, Vivado worker
  lifecycle management, pretty-printed structured results (relies on
  phase 4).

Deliverable: a meaningfully better experience than the Vivado console
for exploring a live design — discoverable commands, inline validation,
overlay-based help that doesn't clutter scrollback.

### Phase 8: LSP polish

Goal: bring the analyzer up to "rust-analyzer-quality" expectations for
the features that matter most.

- Find references across the workspace.
- Rename (cautious; abort on ambiguity).
- Workspace symbols.
- Code actions (add missing required argument, remove unused import,
  convert raw `set_property -dict` to a structured call).
- Performance tuning; consider `salsa` if hand-rolled caching shows its
  limits.

Deliverable: an analyzer that meets the acceptance criteria in the LSP
design section.

### Later (not in initial plan)

- Type system (typed IP handles, units, phases, constraint scopes).
- Quartus backend.
- **Oxide VHDL frontend integration.** Replace the `vhdl_ls` proxy
  `VhdlBackend` with a direct integration with Oxide's developing
  VHDL frontend. Same `LanguageBackend` trait, different
  implementation. Timing depends on the frontend's maturity; the
  `LanguageBackend` abstraction exists from phase 3 specifically to
  make this swap possible without rippling into htcl-side code.
- Tracing / profiling of TCL execution.
- Distributed worker pools for parallel synthesis runs.
- `vw fmt` (htcl formatter).
- Mechanism for htcl parameter doc comments to propagate into
  Vivado-generated wrappers (requires Xilinx-side support; see
  "Wrapper documentation" in Strategic Context).

Not in this project at all (separate downstream tooling):

- IP-XACT → htcl wrapper generation (a sideband tool).
- IP-XACT + configuration values → RSF generation (a separate tool;
  see "RSF generation" in Strategic Context).

## Open questions to resolve with the author

1. **Final name for the htcl dialect.** Working name; pick something
   durable before shipping anything publicly. This is just the language
   name now, not a tool name.
2. **Module namespace semantics.** Global (TCL-compatible, simple) or
   scoped with explicit exports (better complexity management, bigger
   change)? Recommend scoped, but flag for discussion.
3. **`src` vs `use` vs `import` vs `mod` keyword.** Recommend `use` for
   familiarity (Rust) and to avoid the `src/` directory collision.
4. **Shim distribution.** Ship the shim TCL embedded in the `vw-vivado`
   binary, written to a temp file at worker startup? Or expect it on
   disk somewhere? Embedded is simpler for users; do that unless there's
   a reason not to.
5. **`vw-lib` extensions to confirm or schedule:**
   - Stable Rust API for "give me the resolved cache path for dependency
     X" — confirm shape.
   - Generalization of the dependency selector to per-language keys
     (`src` for VHDL, `htcl` for htcl), or staying with `src` plus a
     parallel `htcl` field.
   - Path dependencies (`path = "..."`) — present, absent, or in
     progress?
6. **Cross-language wrapper attribute name.** `@vhdl_entity(name)` is
   the working syntax for declaring which VHDL entity an htcl proc
   wraps. Confirm or rename.
7. **Showcase IP selection.** For the Vivado-team pitch, which IPs do
   we cover in the initial generated `xilinx-ip` package? The
   IP-XACT → htcl generator (a separate sideband tool) produces
   wrappers mechanically, but the showcase needs to demonstrate
   quality at a level that earns the conversation. Pick a small set
   where the generated wrappers will look genuinely good, plus one or
   two complex IPs (DCMAC, CIPS) where the value of source-controlled
   configuration is most visible.
8. **`vhdl_ls` proxy specifics.** Phase 5 wires up VHDL via a
   subprocess proxy to `vhdl_ls`. Open: does `vhdl_ls`'s
   `workspace/symbol` interface answer the cross-language queries we
   need (entity location, generic lists)? If not, what's the
   smallest extension to either the proxy or to `vhdl_ls` itself that
   closes the gap? Confirm before phase 5 starts.
9. **Evidence-gathering for eventual types.** Not a v1 question, but
   worth a habit from day one: when working with htcl, keep a log of
   cases where a type system would have caught a real bug or documented
   intent meaningfully. Revisit the types decision only when there's a
   concrete case file to design against.

## Non-goals

- TCL language compatibility. We are not implementing TCL; we are implementing
  a different language that happens to share TCL's value model and emits TCL
  to Vivado.
- General TCL extension authorship. The Vivado shim is the only TCL we write
  intentionally; it stays small.
- Replacing Vivado's interpreter in-process. We talk to it over a pipe.
- Supporting every Vivado command natively. Most commands pass through as
  strings; we add structured wrappers only where they pay off.
- A package registry. Git-source dependencies cover the realistic needs; a
  registry is a separate company.
- **IP-XACT awareness in vw, the analyzer, or the REPL.** IP-XACT is a
  source format for *generating* htcl IP wrapper packages via a
  separate sideband tool. The tooling described in this plan consumes
  only htcl; it has no IP-XACT-specific code paths, data structures,
  or features. See "Strategic context" for the rationale.
- **A replacement for IP-XACT.** htcl is a configuration interface
  layer that sits above IP-XACT (specification) and below generated
  RTL/RSF (instantiation). It does not describe ports, memory maps,
  or any other aspect of an IP's structure — those remain IP-XACT's
  responsibility. See "Conceptual layering" in Strategic Context.
- **Port-level analysis of generated RTL.** htcl wrappers don't
  describe the ports their instantiation will emit; ports come from
  the VHDL/Verilog Vivado generates. Cross-IP wiring analysis is
  possible but lives in the VHDL analyzer, not in htcl.
- **Memory-map description.** Not htcl's job. Register interfaces are
  generated from IP-XACT plus configuration values by a separate
  pipeline targeting RSF; see "RSF generation" in Strategic Context.

## Reference points

- `vw`: https://github.com/oxidecomputer/vw — the host project for this
  work. `vw-lib` is the existing library that handles dependency
  resolution, caching, and manifest/lockfile management. The plan
  extends `vw` with htcl-language support and an analyzer/repl modeled
  on rust-analyzer's relationship to Cargo.
- rust-analyzer + Cargo: the architectural model. rust-analyzer reads
  `Cargo.toml` / `Cargo.lock`, resolves dependencies through Cargo's
  data model, and provides editor support without reimplementing the
  build tool. `vw analyzer` plays the same role for `vw.toml` /
  `vw.lock`.
- UG835: Vivado Design Suite Tcl Command Reference — the authoritative
  source for what commands exist and what they return.
- IP-XACT (IEEE 1685): the schema for IP component metadata. *Not used
  internally by vw, the analyzer, or the REPL.* IP-XACT is the source
  format consumed by a separate sideband tool that generates `.htcl`
  IP packages, which vw then resolves like any other dependency.
- TypeScript: model for "additive features over an existing language"
  done well. Discipline: existing code keeps working (except htcl
  breaks this intentionally for the proc-arg case), new features are
  opt-in, output is consumable by tools that don't know about the new
  features.
