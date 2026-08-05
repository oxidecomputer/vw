# Authoring htcl libraries

This document is a reference for engineers writing **htcl modules
that wrap underlying EDA IP, commands, or workflows**. The intended
output of such a module is one or more `proc` declarations that
downstream code can call from a workflow script to configure
hardware, drive a build, or otherwise script an EDA tool.

The document covers:

1. What htcl is and what it is for.
2. The full surface of the language — syntax, semantics, attributes.
3. Where htcl behaves the same as Tcl and where it differs.
4. How to validate an htcl program with `vw`.

## 1. What htcl is

htcl — "**h**olistic Tcl" — is a small structured dialect of Tcl
for HDL workflow scripting. It is the language `vw` uses to drive
EDA backends (today, Vivado over a pipe) and to give engineers a
source-controlled, reviewable, tool-checkable surface for everything
that would otherwise live as ad-hoc Tcl: IP configuration, block
design construction, project setup, simulation harnessing, and
custom commands that wrap a vendor tool.

htcl is **not** specific to any single source of those wrappers. An
htcl library may be:

- **Hand-written** by an engineer, wrapping a Vivado/Quartus
  built-in or an in-house Tcl helper with a typed, doc-commented
  interface.
- **Generated** from a vendor IP-XACT description (e.g. `vw ip
  generate` emits an htcl wrapper for a Xilinx IP). The result is
  ordinary htcl — there is nothing IP-XACT-shaped about it once
  generated.
- **Generated from anything else** that has no IP-XACT — a custom
  IP repo, a curated set of Tcl recipes, a board-bring-up script.

What unifies them is the structured `proc` surface: every wrapper
is a proc with documented keyword arguments, default values, and
constraints the analyzer can check.

At analysis time, the syntax tree drives the LSP (`vw analyzer`) —
completion, hover, signature help, error reporting. At run time,
htcl is lowered to plain Tcl and shipped to the backend.

### Module shape

An htcl library is one or more `.htcl` files. Most libraries place
their entry point at a conventional path (`src/<name>.htcl`) and
may `src`-import additional files. A `proc` declared in any
imported file becomes callable in the consumer's scope. Proc names
are flat unless wrapped in a `namespace eval` block, which groups
related helpers under a `<ns>::<name>` prefix — see §2.10 below.

## 2. The language

### 2.1 File overview

An htcl file is a sequence of statements separated by newlines or
semicolons. Each statement is one of:

| Statement | Purpose |
|---|---|
| Comment `# ...` | Free-form comment; ignored. |
| Doc comment `## ...` | Attached to the next `proc` or proc-arg; surfaces in hover. |
| Command `name word word ...` | A call to any command (Tcl builtin, EDA builtin, or htcl proc). |
| `set <name> <value>` | Variable assignment (same as Tcl). |
| `proc <name> { args } { body }` | Structured proc declaration (the main authoring construct). |
| `src <path>` | Import another htcl module (htcl-specific; no Tcl analogue). |

Whitespace is significant only as a word separator. Indentation is
free-form.

### 2.2 Word forms

Every command word can be written in one of three forms — the same
three Tcl supports:

| Form | Example | Semantics |
|---|---|---|
| Bare | `foo` | A literal word. May contain `$var` and `[cmd]`. |
| Quoted | `"hello $world"` | Variable and command substitution still happen; whitespace is preserved. |
| Braced | `{a b c}` | Literal text; no substitution. |

Inside `[ … ]` command substitution, newlines are treated as
whitespace and do not need backslash continuations. **This is the
canonical form for call sites that don't fit on one line** — wrap
the call in brackets, bind the result with `set`, and let each
keyword argument live on its own line without backslash noise:

```htcl
set cpm5_pcie1 [
  create_cpm5_cpm_pcie1
    -cell cpm5
    -max_link_speed 32.0_GT/s
    -modes PCIE
]
```

A bare word ending in `\` does continue onto the next line (the
classic Tcl form), but bracket-bound `set` is the preferred style.

### 2.3 Comments and doc comments

htcl has two distinct comment forms with different purposes:

- **Regular comments — `# ...`.** Free-form. Use these to record
  rationale at call sites, to label sections of a workflow script,
  to leave notes — anything that's "for the reader." The analyzer
  ignores them.
- **Doc comments — `## ...`.** Semantically significant
  documentation that attaches to the next `proc` declaration or
  the next proc-arg. Tooling — the LSP for hover and signature
  help, and documentation generators — consumes them. Use them
  only inside a library, on definitions; they serve no purpose at
  call sites.

```htcl
## Configure an AXIS register slice.   ;# doc-comment on the proc
proc create_axis_register_slice {
  ## Block-design cell name.            ;# doc-comment on this arg
  cell_name
  ...
}

# Internal streaming bus between DMA and classifier.   ;# call-site
# 128-bit because the classifier hits 100 Gb/s.        ;# rationale —
set dma_to_classifier [                                 ;# use #, not ##
  create_axis_register_slice
    -cell_name dma_to_classifier
    -tdata_width 128
]
```

Multiple `##` lines stack into one block of doc-comment text on
the item they precede.

### 2.4 The `proc` declaration

This is the central authoring construct. The args list inside the
first `{ … }` is *structured*: each arg is a single identifier
preceded by optional doc comments and optional `@attribute(...)`
annotations. The args grammar is:

```
args      := arg_item*
arg_item  := doc_comment* attribute* IDENT
attribute := '@' IDENT ( '(' value ( ',' value )* ')' )?
value     := integer | string | ident
```

Example:

```htcl
## Configure a Versal CIPS instance.
##
## Sets the requested CONFIG.* properties on the supplied block-design cell.
proc create_versal_cips {
  ## Block-design cell handle to set the property on.
  cell

  ## Boot the secondary PCIe controller as well as the primary.
  @enum(0, 1) @default(0) boot_secondary_pcie_enable

  ## Inner dict for the PMC subsystem.
  @default("") ps_pmc_config
} {
  set_property -dict [list \
    CONFIG.BOOT_SECONDARY_PCIE_ENABLE $boot_secondary_pcie_enable \
    CONFIG.PS_PMC_CONFIG              $ps_pmc_config \
  ] $cell
}
```

Notes:

- **htcl is keyword-only.** Every proc you declare accepts its
  arguments as `-flag value` pairs at the call site. There is no
  positional call syntax — even an arg with no `@default` (an
  implicitly-required arg) must be passed as `-arg value`. The
  args list above is the **declaration order**, used by the
  validator for documentation and stable diagnostics, not by Tcl
  for dispatch.
- Args with no `@default` are **required**. Omitting them at a
  call site is a compile-time error from the validator.
- The proc body is plain Tcl text. The body refers to each arg by
  its bare name (`$cell`, `$boot_secondary_pcie_enable`, …) — the
  same way it would for a standard Tcl proc with named parameters.
  The lowerer wires up these locals from the caller's `-flag
  value` pairs at runtime via a generated `::vw::kwargs` prelude.
- Because the keyword parse happens at runtime (inside the
  wrapper), call sites work uniformly at **any** nesting: at the
  top level of a file, inside another proc's body, inside a
  `namespace eval`, inside a `[ ... ]` command substitution, or
  through an `eval`/`uplevel`. The lowerer doesn't need to see
  the call site to translate it.
- `proc` itself may be declared at any depth, but only **top-level**
  proc declarations are visible to the call-site validator. Procs
  defined inside another proc's body ship as raw text and miss
  the kwargs-prelude treatment — avoid nested proc declarations
  in htcl, or write them in raw Tcl form (`proc inner args { ...
  }`).

### 2.5 Argument attributes

Attributes go on the line(s) before the arg's identifier. They are
parsed positionally and may stack. Values are written with strings
quoted, integers bare, identifiers bare.

| Attribute | Meaning | Example |
|---|---|---|
| `@default(<value>)` | Default value used when caller omits this arg. Presence makes the arg optional. | `@default(0) boot_secondary_pcie_enable` |
| `@enum(<v1>, <v2>, ...)` | Caller's value must match one of the listed literals. Validated when the value is a literal. | `@enum(0, 1) enable` |
| `@range(<lo>, <hi>)` | Caller's integer value must satisfy `lo <= n <= hi`. | `@range(1, 16) num_lanes` |
| `@requires(<other>)` | This arg, if set, requires `-<other>` to also be set. | `@requires(has_tuser) tuser_width` |
| `@conflicts(<other>)` | This arg cannot coexist with `-<other>`. | `@conflicts(slave_mode) master_mode` |
| `@deprecated[(msg)]` | Warning at call sites; optional human message. | `@deprecated("use -mode instead") legacy_mode` |

`@enum` and `@range` only check **literal** call-site values. A
value that is itself an interpolation (`$var`, `[cmd]`) is not
statically checkable and silently passes — the runtime sees
whatever the interpolation produces.

### 2.5.0a Argument types

An argument may carry a type annotation in a `: TYPE` suffix on
the arg name:

```
proc plumb_pin {
  ## What to name the external port.
  name: string

  ## Identity of the pin to make external.
  pin: bd_pin
} unit {
  …
}
```

The annotation uses the same type vocabulary as the return-type
slot (§2.5.1) — primitives (`string`, `int`, `bool`, `unit`),
newtypes (`bd_cell`, `bd_pin`, …), and generics (`list<T>`,
`dict<K, V>`, with arbitrary nesting). Compatible with the
existing attribute grammar (`@default(0) count: int`).

Annotations are optional — untyped args still parse. The
analyzer shows annotated args as `-name: TYPE` in hover and
signature help; the validator uses them to shape-check newtype
`<T>::repr` / `from` / `to` triplets.

See [htcl-return-types.md](htcl-return-types.md) for the full
type vocabulary, newtype declaration syntax, and worked
examples. For values that can take one of several shapes
(e.g. heterogeneous EDA return values), see
[htcl-enums.md](htcl-enums.md) for tagged sum types with
auto-generated constructors, repr, and overload dispatch.

### 2.5.1 Return types

A proc may carry a return-type annotation in a 4th-word slot
between the args block and the body:

```
proc make_widget { @arg(name) ... } widget {
  …body…
}
```

The annotation drives the REPL printer (the result is formatted
through the type's `repr` proc) and the analyzer's hover /
signature-help (`proc NAME → TYPE`). Procs without an annotation
parse and behave identically to today — adoption is gradual.

Available shapes:

- Primitives: `string`, `int`, `bool`, `unit`. Built into the
  compiler; no declaration needed.
- Generics: `list<T>`, `dict<K, V>`, with arbitrary nesting.
- User newtypes: any identifier introduced via `type NAME =
  UNDERLYING`, accompanied by `<NAME>::repr` / `from` / `to`.

`unit` is the type for side-effecting procs that don't return a
meaningful value (logging, configuring, connecting). The REPL
suppresses the empty Result entry on `unit`-typed expressions.

See [htcl-return-types.md](htcl-return-types.md) for the full
type vocabulary, newtype declaration syntax, and worked examples.

### 2.6 Call sites

The canonical call-site shape is `set <name> [<call> <args…>]` —
bind the call's return value to a name, and let the brackets handle
multi-line wrapping. Each keyword argument goes on its own line, no
backslash continuations needed:

```htcl
set cips [
  create_versal_cips
    -name cips
    -cpm_config cpm5
]

# 250 MHz / 195 MHz aren't in the preset list but the clock
# generator will synthesize them — chosen for the eth core.
set ps_pmc_config [
  create_versal_cips_ps_pmc_config
    -cell cips
    -clock_mode Custom
    -design_mode 1
    -pcie_apertures_dual_enable 0
    -pcie_apertures_single_enable 0
    -pmc_crp_pl0_ref_ctrl_freqmhz 250
    -pmc_crp_pl1_ref_ctrl_freqmhz 195
    -ps_board_interface Custom
    -ps_pcie1_peripheral_enable 0
    -ps_pcie2_peripheral_enable 1
    -ps_pcie_reset {ENABLE 1}
    -ps_use_pmcpl_clk0 1
    -ps_use_pmcpl_clk1 1
    -ps_use_pmcpl_iro_clk 0
    -smon_alarms Set_Alarms_On
    -smon_enable_temp_averaging 0
    -smon_temp_averaging_samples 0
]
```

A one-line call works the same way — just don't break across lines:

```htcl
set cpm5 [create_cpm5 -name cpm5]
```

Rules the validator enforces on every call to a known proc:

- Each `-flag` must be one of the declared args.
- Each `-flag` is given exactly one value (the next word).
- Each `-flag` appears at most once (a duplicate is a warning).
- Every required arg (no `@default`) must be present.
- `@requires` / `@conflicts` relationships are checked across
  present args.
- Literal values are checked against `@enum` / `@range`.
- `@deprecated` flags produce warnings.

Calls to commands that aren't declared `proc`s in the loaded
documents are **not** validated — they're assumed to be EDA or
Tcl builtins and are passed through verbatim.

### 2.7 Variables and substitution

Variables work as in Tcl:

```htcl
set ref_clk 100
puts "ref clock is $ref_clk MHz"

# Braces suppress substitution.
puts {$ref_clk is literal here}
```

`$name` references resolve against the nearest enclosing scope —
local `set`s first, then the enclosing proc's parameter list.
There is no static type checking on variable values.

### 2.8 Imports — `src`

```htcl
src @amd-htcl/cpm5              ;# named workspace dependency
src @amd-htcl/cips
src "lib/utils.htcl"             ;# relative to the importing file
src "/abs/path/to/file.htcl"     ;# absolute filesystem path
```

The `src` statement loads and inlines another htcl module. Path
forms:

- `@<name>/<subpath>` — resolved via `vw.toml`'s workspace
  dependencies (the same dependency resolver `vw` uses for VHDL
  deps).
- Anything starting with `/` — filesystem-absolute.
- Anything else — relative to the directory of the importing file.

The path word must be a literal (bare or quoted text with no
`$var` or `[cmd]` parts). The loader is idempotent on canonical
paths: a file imported twice loads once.

By the time an htcl program reaches the backend, all `src` imports
have been flattened into a single Tcl stream.

### 2.9 A complete library example

Hand-written wrapper around an IP, with no IP-XACT involved:

```htcl
## A minimal AXIS interface configurator.
##
## Wraps the underlying create_bd_cell and set_property calls so that
## a consumer can request a configured AXIS slice with a few keyword
## arguments.
proc create_axis_register_slice {
  ## Block-design cell name to instantiate at.
  cell_name

  ## Width of the data bus in bits.
  @enum(8, 16, 32, 64, 128, 256, 512) @default(64) tdata_width

  ## Include byte-strobe sideband.
  @enum(0, 1) @default(0) has_tkeep

  ## Width of the optional user sideband; required when -has_tuser is on.
  @range(1, 32) @requires(has_tuser) tuser_width

  ## Set when a user sideband is desired.
  @enum(0, 1) @default(0) has_tuser

  ## Newer designs should use -has_tuser instead.
  @deprecated("use -has_tuser") legacy_tuser_mode
} {
  create_bd_cell -type ip -vlnv xilinx.com:ip:axis_register_slice:1.1 $cell_name
  set_property -dict [list \
    CONFIG.TDATA_NUM_BYTES [expr {$tdata_width / 8}] \
    CONFIG.HAS_TKEEP       $has_tkeep                \
    CONFIG.HAS_TUSER       $has_tuser                \
    CONFIG.TUSER_WIDTH     $tuser_width              \
  ] [get_bd_cells $cell_name]
}
```

A call site, with rationale captured in plain comments:

```htcl
src @oxide-ip/axis

# Internal streaming bus between the DMA and the packet classifier.
# 128-bit because the classifier hits 100 Gb/s line rate; tuser carries
# the classification verdict (5 bits today, room for one more flag).
set dma_to_classifier [
  create_axis_register_slice
    -cell_name dma_to_classifier
    -tdata_width 128
    -has_tkeep 1
    -has_tuser 1
    -tuser_width 6
]
```

### 2.10 Namespaces — `namespace eval`

When several procs share a logical prefix (`project::set_*`,
`ip::*`, `log::*`), wrapping them in a `namespace eval` block lets
each member be defined with a short bare name while still being
*called* under the qualified `<ns>::<name>` form:

```htcl
namespace eval project {
  ## Set the target HDL language for new sources in a project.
  proc set_target_language {
    proj
    @enum(VHDL, Verilog) language
  } {
    set_property -name TARGET_LANGUAGE -value $language -objects $proj
  }

  ## Set the default library new sources land in.
  proc set_default_library {
    proj
    @default(xil_defaultlib) library
  } {
    set_property -name DEFAULT_LIB -value $library -objects $proj
  }
}

# At a call site:
project::set_target_language -proj $proj -language VHDL
project::set_default_library  -proj $proj
```

The analyzer treats each inner `proc` exactly as if it had been
written `proc project::set_target_language { ... } { ... }` at the
top level — same `@enum` / `@default` / `@requires` validation,
same hover, same signature help, same completion. The only
difference is source organization.

Mechanics:

- The `name` word can be a multi-segment Tcl namespace
  (`namespace eval foo::bar { ... }`); the analyzer uses the
  entire name as the prefix.
- `namespace eval` blocks nest. An inner `proc baz` inside
  `namespace eval outer { namespace eval inner { ... } }`
  registers as `outer::inner::baz`.
- A call from *inside* a namespace body to a sibling member must
  still use the qualified name (no automatic same-namespace
  resolution in v1). Write `project::helper $x`, not bare
  `helper $x`.
- Lowering walks namespace bodies recursively, so inner procs get
  their attributes stripped and the same `::vw::kwargs` runtime
  prelude that top-level procs get.

## 3. How htcl differs from Tcl

htcl is a strict superset of the Tcl subset most engineers actually
write — anything you'd type in a Vivado console as a one-off
command parses as htcl. The structural differences come from htcl
adding new constructs and tightening the rules around `proc`
declarations.

### 3.1 What htcl adds

| Construct | htcl | Tcl |
|---|---|---|
| Doc comments | `## ...` carry to the next `proc` / proc-arg and feed hover. | Plain `#`; no first-class doc concept. |
| Structured `proc` args | Each arg is a doc-commented, attribute-tagged identifier. | Args are flat names or `{name default}` pairs. |
| Keyword call sites | `create_x -foo a -bar b` | Positional `create_x a b`. |
| Static validation | `@enum`, `@range`, `@requires`, `@conflicts`, etc. checked at parse-time. | None — errors only at runtime. |
| Module imports | `src @dep/file` or `src "rel/path.htcl"`. | `source ./foo.tcl`, with no dependency resolution. |
| Bracket-body line continuation | Newlines inside `[ … ]` are whitespace. | Newline terminates a command unless `\`-escaped. |

### 3.2 What htcl restricts or interprets differently

- **`proc` args are structured.** A v1 htcl `proc` cannot declare
  its args as `{name default}` pairs or as `args` for varargs the
  way pure Tcl can. Every arg is a single bare identifier,
  optionally preceded by attributes. Defaults live in
  `@default(...)`.
- **Required args come from the absence of `@default`.** Any arg
  without `@default` is required. There is no `args` catch-all.
- **Call sites must use keyword form.** When the validator sees a
  call to a known proc, positional words other than `-flag value`
  pairs are reported as errors. Calls to *unknown* commands
  (presumed EDA/Tcl builtins) pass through verbatim with no shape
  check.
- **Doc comments are semantically significant.** `##` on a `proc`
  or proc-arg is consumed by tooling — the LSP for hover,
  documentation generators for output — so removing or relocating
  one changes observable behavior. Regular `#` comments behave
  exactly like Tcl comments.
- **`src` is parsed structurally.** The path word must be a
  literal; `src $name` is rejected because the analyzer needs to
  follow imports statically.
- **Top-level only for declarations and validated calls.** The
  validator builds its signature table from top-level `proc`
  declarations. A proc declared inside another proc's body still
  parses, but its signature is not used to check call sites.
  Likewise, the lowering pass rewrites top-level call sites to
  known procs; calls *inside* a proc body are shipped verbatim.
  Write your library entry points at the top level.

### 3.3 What is unchanged

Everything else is plain Tcl:

- `$var`, `[cmd]`, `"..."`, `{...}`, backslash escapes.
- `set`, `expr`, `if`, `foreach`, `puts`, `list`, `dict`, …
- The proc body is just Tcl text. Anything you can do in Tcl
  works inside a proc body; htcl makes no attempt to constrain it.

If in doubt, write Tcl. htcl only diverges in service of the
structured proc surface; the body of every command is shipped
through to the backend as written.

## 4. Checking an htcl program with `vw`

`vw check` parses, validates, and reports errors and warnings for
one or more `.htcl` files without executing anything. It uses the
same analyzer pipeline the LSP uses, so a clean `vw check` means
the LSP will also be quiet.

### 4.1 Basic invocation

```bash
vw check src/cips.htcl
vw check src/lib/*.htcl
```

Output on a clean file:

```
    Checking cips
```

Output with errors:

```
error: src/cips.htcl:42:23: value 3 for -boot_secondary_pcie_enable is not in @enum. Possible values are 0, 1
error: src/cips.htcl:58:1: missing required argument -cell
src/cips.htcl: 2 error(s), 0 warning(s)
```

Each line carries an absolute file path, line, and column, in
`path:line:col: message` format. Spans inside `src`-imported files
are mapped back to their originating file, so an error in an
imported module reports the imported file's path — not the entry
point's.

### 4.2 What `vw check` enforces

The validator runs the rules described above:

- **Parse errors.** Anything that doesn't lex/parse cleanly:
  unterminated brace groups, missing values for `-flag` words,
  malformed attributes.
- **Proc shape.** Duplicate proc declarations are an error (the
  later one wins, matching Tcl's redefine semantics).
- **Call sites against known procs.**
  - Unknown `-flag`: `undefined argument -<name>. Possible values
    are <list>`.
  - Missing value: `argument -<name> is missing a value`.
  - Missing required: `missing required argument -<name>`.
  - Duplicate flag (warning): `duplicate argument -<name>`.
  - `@enum` violation: `value <v> for -<name> is not in @enum. ...`.
  - `@range` violation: `value <n> for -<name> is out of @range(...)`.
  - `@range` on a non-integer literal: `argument -<name> expects
    an integer, found <v>`.
  - `@requires` unmet: `argument -<a> requires -<b> to also be
    set`.
  - `@conflicts` triggered: `argument -<a> conflicts with -<b>`.
  - `@deprecated` (warning): `argument -<name> is deprecated[: msg]`.
- **`src` imports.** Unknown dependency, missing file, non-literal
  path, and parse errors inside imported files.

### 4.3 What `vw check` does *not* enforce

- Variable type, range, or existence inside a proc body — the
  body is opaque to the analyzer in v1.
- EDA- or Tcl-builtin call shapes. A call to `set_property` or
  `create_bd_cell` passes through unchecked.
- Values that go through `$var` or `[cmd]` substitution. `@enum`
  and `@range` only see literal call-site words.
- Module-level public/private. Every top-level proc in every
  loaded file is in scope.

### 4.4 Related `vw` commands

- `vw run <file.htcl>` — parses, validates, and executes through
  the EDA backend. With `--check` it stops after the parse and
  reports errors. Useful when you want to confirm a file is
  shippable without spinning up the backend.
- `vw analyzer` — the LSP server (stdio). Editors point at it for
  completion, hover, signature help, goto, and live error
  reporting. The errors are exactly what `vw check` reports.
- `vw ip generate <component.xml>` — generates an htcl wrapper
  from an IP-XACT component. The generated file is itself a fully
  valid htcl library; reading one is a fast way to see a real
  wrapper's shape.

### 4.5 Suggested authoring loop

1. Sketch the proc signature: name the args, attach doc comments,
   set `@default` for everything optional, mark `@enum` /
   `@range` where the underlying domain is known and exhaustive.
2. Write the body in plain Tcl — `set_property`, `create_bd_cell`,
   whatever the backend needs.
3. Run `vw check` to confirm the proc parses cleanly.
4. Add a call site in a separate `.htcl` file and run `vw check`
   on that to confirm the validator agrees with the signature.
5. Open the file in an editor with `vw analyzer` configured for
   `.htcl` and verify hover and completion behave as expected —
   the doc comments you wrote are what consumers will read.
6. Once the surface looks right, run the call site through
   `vw run` to see the lowered Tcl actually do something on the
   backend.

## Reference summary

```text
File                  := Statement*
Statement             := Command | Comment | DocComment | Proc | Src
Command               := Word Word*
Word                  := Bare | Quoted | Braced
Comment               := '#' .* NEWLINE
DocComment            := '##' .* NEWLINE     ; attaches to next proc / proc-arg
Proc                  := 'proc' Name '{' ArgList '}' '{' Body '}'
ArgList               := ArgItem*
ArgItem               := DocComment* Attribute* Ident
Attribute             := '@' Ident ( '(' Value (',' Value)* ')' )?
Value                 := Integer | String | Ident
Src                   := 'src' PathWord
PathWord              := '@'<dep>'/'<sub> | '/<abs>' | '<rel>'
```

Canonical call-site shape:

```text
set <name> [<proc> <-flag value>...]
```

Attributes recognized by the validator:

```text
@default(<value>)          ; default value, makes arg optional
@enum(<v>, <v>, ...)       ; allowed literal values
@range(<lo>, <hi>)         ; integer range, inclusive
@requires(<other>)         ; presence implies -<other> present
@conflicts(<other>)        ; presence forbids -<other>
@deprecated[(<msg>)]       ; warns at call sites
```

Errors and warnings surface through:

- `vw check <files…>` — one-shot CLI.
- `vw run <file> --check` — same checks, no execution.
- `vw analyzer` — same checks, live in the editor.
