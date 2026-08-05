# Return-type annotations in htcl

htcl procs may declare a return type in a 4th-word slot between the
args block and the body:

```
proc make_widget { @arg(name) ... } widget {
  …body…
}
```

The annotation is purely additive — procs without it parse and run
identically to today. With it, the REPL printer and the analyzer
(hover, signature help) start treating the proc's result as
typed.

## Syntax

Three pieces:

```
proc NAME { ARGS } TYPE { BODY }
```

The args list and body keep their existing shapes (see
[htcl.md](htcl.md) for
the arg attribute grammar). The new `TYPE` slot is a single htcl
word:

- A bare identifier: `string`, `int`, `bool`, `unit`, `bd_cell`,
  `widget`, …
- A generic with no whitespace: `list<bd_cell>`,
  `dict<string,int>`, `list<dict<string,bd_cell>>`.
- A brace-wrapped type when the expression contains spaces:
  `{dict<string, int>}` — the parser strips the outer braces
  before type-parsing.

### Grammar

```
Type ::= IDENT ('<' Type (',' Type)* '>')?
```

Nested generics work to arbitrary depth.

For heterogeneous values that can take one of several distinct
shapes — e.g. Vivado property values that are sometimes scalars
and sometimes embedded dicts — use **enums** (tagged sum types).
See [htcl-enums.md](htcl-enums.md) for the full design.

## Type vocabulary

### Primitives (built into the compiler)

| Type      | Repr                                                    |
| --------- | ------------------------------------------------------- |
| `string`  | identity                                                |
| `int`     | `[format %d $v]`                                        |
| `bool`    | `true` / `false`                                        |
| `unit`    | empty string; the REPL suppresses the Result entry      |

`unit` is the "I don't return a meaningful value" type — use it on
side-effecting procs (logging, connecting, configuring) so the
REPL stops trying to render their empty return as output.

### Newtypes (user-declared)

Any other type is a newtype, introduced by:

```
type NAME = UNDERLYING
```

Every newtype declaration **must** be accompanied by three procs
in a namespace matching the type name — the validator rejects the
program otherwise:

| Proc           | Signature                          | Purpose                                      |
| -------------- | ---------------------------------- | -------------------------------------------- |
| `<T>::repr`    | `proc <T>::repr { v } string { … }` | Render an instance for display.              |
| `<T>::from`    | `proc <T>::from { v } <T> { … }`    | Validate + lift an underlying value into T.  |
| `<T>::to`      | `proc <T>::to { v } <U> { … }`      | Extract the underlying value back out of T.  |

The `from` proc is the one place to validate — e.g. reject
strings that don't match the Vivado-path shape `^/[\w/]+$` before
they get treated as a `bd_cell`.

### Example: Vivado's typed handles

The whole `bd_*` family lives in
`~/src/htcl/amd/vivado-cmd/types.htcl`:

```
type bd_cell = string

proc bd_cell::repr {v} string { return $v }
proc bd_cell::from {v} bd_cell {
  if {![regexp {^/[\w/]+$} $v]} {
    error "bd_cell::from: '$v' is not a valid block-design path"
  }
  return $v
}
proc bd_cell::to {v} string { return $v }
```

All `bd_pin`, `bd_intf_pin`, etc. follow the same template.

### Example: a domain newtype

A user library can introduce its own types the same way:

```
type pcie_lane_count = int

proc pcie_lane_count::repr {v} string {
  return "x$v"  ;# render as "x1", "x2", "x4", "x8", "x16"
}

proc pcie_lane_count::from {v} pcie_lane_count {
  if {$v ni {1 2 4 8 16}} {
    error "pcie_lane_count must be one of {1 2 4 8 16}, got $v"
  }
  return $v
}

proc pcie_lane_count::to {v} int { return $v }
```

Now a proc annotated `} pcie_lane_count {` will render `x4` in
the REPL instead of `4`.

### Generics

`list<T>` and `dict<K, V>` work over any composition of primitives
and newtypes — the compiler monomorphizes a `repr` proc per unique
instantiation, dispatching to the user's per-type `<T>::repr` at
element boundaries.

```
proc list_of_cells {} list<bd_cell> { return [list /a /b /c] }
```

The REPL invokes the compiler-generated
`list_bd_cell::repr` on the result, which iterates the list and
joins each element's `bd_cell::repr` rendering with newlines:

```
› list_of_cells
  /a
  /b
  /c
```

For dicts the rendering is `KEY VAL` pairs, one per line:

```
proc props {} dict<string,string> { … }

› props -object $cips
  CLASS bd_cell
  NAME cips
  …
```

## What the type drives

| Subsystem              | Behavior                                                                                 |
| ---------------------- | ---------------------------------------------------------------------------------------- |
| REPL result printer    | Wraps the expression with the type's `repr` proc; `unit` suppresses the Result entry.   |
| Analyzer hover         | Shows `proc NAME → TYPE` in the hover popup.                                            |
| Analyzer signature help | Appends ` → TYPE` to the signature label.                                              |

Unannotated procs keep the legacy heuristic formatter as a fallback,
so adopting annotations is gradual — annotate as you go.

## Argument types

Arguments use the same vocabulary as return types, declared with
a `: TYPE` suffix on the arg name:

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

Rules:

- Same grammar as return types — primitives, newtypes, and
  generics with arbitrary nesting.
- Compatible with existing attributes (`@default(0) count: int`,
  `@enum(Master, Slave) mode: string`).
- Optional. Untyped args still parse — adoption is gradual.

What the annotation drives:

- **Validator shape checks.** Newtype `<T>::repr/from/to` procs
  get a full shape check: `repr` must take `v: T` and return
  `string`; `from` must take `v: <underlying>` and return `T`;
  `to` must take `v: T` and return `<underlying>`. Annotations
  are *optional* on these procs — unannotated args/returns pass
  as "trust the user". Annotated mismatches are a hard error.
- **Analyzer display.** Hover and signature help render the arg
  as `-name: TYPE` instead of the bare `-name` form.

Out of scope for v1 (future work): call-site validation
("you're passing a `string` where a `bd_cell` is expected"),
unions, and inference for unannotated args.

## Authoring conventions

- **Annotate as you write.** Same effort as documenting an arg,
  same payoff as a TypeScript return-type hint.
- **Prefer specific named newtypes over `string`** for values
  that have a well-defined shape (paths, IDs, port names). The
  `from`/`to` triplet documents the invariant and the `from`
  validator catches typos at the boundary.
- **Use `unit` for side-effecting procs.** Anything that calls
  `set_property`, `connect_*`, `puts`, or `log::*` is almost
  certainly `unit`. The REPL won't bother trying to display
  whatever Tcl-internal value falls out.
- **Generics nest freely.** `dict<string,list<bd_cell>>` is fine.
  Don't be afraid to be specific.

## Limitations (v1)

- **No arg-type annotations yet.** Args still use only the
  attribute grammar (`@default`, `@enum`, etc.). The
  `<T>::repr/from/to` validator only checks the procs EXIST,
  not that their signatures match shape — that arrives with arg
  types.
- **No inference.** Unannotated procs are simply untyped; we
  don't walk the body to derive a return type from `return`.
- **No union or function types.** Start small; extend the grammar
  as need shows up.

For the implementation, see `vw-htcl/src/repr.rs` (codegen),
`vw-htcl/src/type_parse.rs` (mini-parser), and the design notes
in [the original plan](../docs/plans/return-types.md) if it
survives the cleanup.
