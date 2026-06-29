# Enums (tagged sum types) in htcl

Enums are htcl's way to model values that can be one of several
distinct shapes. They're tagged unions: every value carries a
variant tag at runtime, the compiler auto-generates the
boilerplate (constructors, repr, accessors), and overloaded
handlers dispatch on the tag with no runtime string introspection
on the compiler side.

This is the principled answer to "I have a value that's
*sometimes* a scalar and *sometimes* a nested dict" — the
canonical case being Vivado property values, where
`get_property NAME $obj` returns a string but
`get_property CONFIG.PS_PMC_CONFIG $obj` returns an embedded
property dict.

See also: [htcl-return-types.md](htcl-return-types.md) for the
broader type system, [authoring-htcl-libraries.md](authoring-htcl-libraries.md)
for the surrounding arg/return-type annotation grammar.

## Syntax

Declaration:

```
enum Property = {
  Scalar: string
  Nested: dict<string, Properties>
}

type Properties = dict<string, Property>
```

The variants block is **brace-wrapped and newline-separated** —
the same shape as `proc {arg1; arg2}`. Each variant is
`IDENT (':' TYPE)?`; the payload type is optional, so
empty-payload variants are first-class:

```
enum Direction = {
  North
  South
  East
  West
}
```

Qualified variant types for use in arg annotations:

```
proc handle_prop {v: Property::Scalar} string { return "scalar: $v" }
proc handle_prop {v: Property::Nested} string { return "nested children: [llength $v]" }
```

The compiler sees that two `handle_prop` procs share a name, that
each first arg is a different variant of `Property`, and
synthesizes a public `handle_prop` dispatcher — no user-written
`proc handle_prop {v: Property} ...` boilerplate.

## Runtime representation

Every variant value is a Tcl list:

- **With payload**: `[list <Variant> <payload>]` — two elements.
  `Property::Scalar "foo"` → `[list Scalar foo]`.
- **Without payload**: `[list <Variant>]` — single element.
  `Direction::North` → `[list North]`.

The variant short-name (`Scalar`, not `Property::Scalar`) is
enough for dispatch because the dispatcher already knows which
enum it's switching on.

## Auto-emitted machinery

For an enum declaration the compiler emits a single
`namespace eval <EnumName> { … }` block. For
`enum Direction = { North; South: int; East; West }` it looks
roughly like:

```tcl
namespace eval Direction {
  # Constructors — one per variant.
  proc North {}  { return [list North] }
  proc South {v} { return [list South $v] }
  proc East {}   { return [list East] }
  proc West {}   { return [list West] }

  # Accessors — explicit unwrap entry points wrappers use when
  # bridging to extern:: calls.
  proc tag {v}     { return [lindex $v 0] }
  proc payload {v} { return [lindex $v 1] }

  # repr — switches on tag, calls payload type's repr. Renders
  # as `Variant(<inner>)` for payload variants, bare `Variant`
  # for empty ones.
  proc repr {v} { … }

  # from / to — identity (exist so generics over enums type-check
  # uniformly with newtypes).
  proc from {v} { return $v }
  proc to {v}   { return $v }
}
```

**No user-written triplet is required for an enum** (newtypes
require `repr`/`from`/`to`; enums get them auto-generated). If
the user wants custom rendering, they can override the proc
post-hoc — same as any other htcl proc.

## Bridging to extern (lowering)

EDA builtins (`extern::create_bd_cell`, `extern::get_property`,
etc.) don't understand tagged tuples — they expect bare Tcl
primitives. Any time an enum value flows into an `extern::` call,
it has to be unwrapped first.

**v1 policy: explicit unwrap, no auto-lowering.** Wrappers (the
procs that own the `extern::` boundary) explicitly extract the
payload via `<Enum>::payload`:

```
proc vivado_cmd::set_string_prop {obj: bd_cell; name: string; val: Property} unit {
  # Property::payload extracts the inner value; if val is
  # Scalar("foo"), this yields "foo". The wrapper is responsible
  # for knowing this is the right shape — the compiler doesn't
  # auto-coerce.
  extern::set_property -dict [list $name [Property::payload $val]] -objects $obj
}
```

Newtypes don't need unwrap — `bd_cell` IS a string at runtime,
so `extern::foo $cell` already passes the right thing.

Compiler-side **auto-lowering** (walk the expression tree, find
every enum-typed value being passed to an extern, insert the
unwrap automatically) requires full type inference across
expressions and is out of scope for v1. The explicit
`<Enum>::payload` form is principled (no magic at call sites)
and gives wrapper authors visibility into where lowering
happens.

## Lifting from extern

The other direction — taking an EDA function's raw Tcl return
value and tagging it into a typed enum — is per-function
business. `extern::get_property NAME $obj` returns a scalar;
`extern::get_property CONFIG.PS_PMC_CONFIG $obj` returns an
embedded dict. Whether a given property is one or the other is
**metadata the wrapper queries from the EDA tool** (e.g.
`extern::report_property -type`), not a shape-of-string
heuristic.

**v1 policy: lifting lives in the wrapper, not the compiler.**
Each wrapper that returns an enum decides which variant to
construct. The compiler doesn't try to be smart — there's no
shape-guessing path in compiler-emitted code.

To avoid every wrapper reinventing the wheel, the
`~/src/htcl/amd/vivado-cmd/lift.htcl` library provides a small
set of reusable helpers:

```
# Structural check: is the string a well-formed Tcl list with
# an even length and bare-ident keys? Used by wrappers that
# already have other evidence the value MIGHT be a paired dict
# and need a sanity check — NOT as a primary classifier.
proc lift::looks_like_paired_dict {raw: string} bool { … }

# Vivado-specific: lift a property value to Property using
# `extern::report_property -type` metadata. The classifier IS
# the heuristic — but it's named, scoped, and called from one
# place instead of being baked into the compiler.
proc lift::vivado_property {obj: bd_cell; name: string; raw: string} Property { … }
```

Wrappers compose these. Custom cases write their own lifters —
the helper library is convenience, not a requirement.

**Future direction**: F# data providers as inspiration for
`vw ip generate`. Given an IP-XACT schema, the generator could
emit not just wrappers but also the per-component tagging logic
— declarative schema in, typed lifting out. Worth investigating
once the v1 enum machinery is in user hands and we see which
lifting patterns actually recur.

## Overload dispatch

When two or more procs share a name AND each one's first arg is
declared as a different variant of the same enum, the compiler
treats them as **a single overloaded function** rather than a
duplicate-definition warning.

```
proc handle_prop {v: Property::Scalar} string { return $v }
proc handle_prop {v: Property::Nested} string {
  set parts [list]
  foreach {k val} $v { lappend parts "$k=[handle_prop $val]" }
  return [join $parts ", "]
}
```

The compiler:

1. **Verifies exhaustiveness** — every variant of `Property`
   must have a handler. Missing variants are a hard error
   pointing at the first overload, listing the gaps.
2. **Verifies tail-arg agreement** — every overload must
   declare identical args after the dispatched first one
   (same names, attributes, type annotations).
3. **Verifies return-type agreement** — every annotated
   return must be identical. Mixing annotated and unannotated
   is an error.
4. **Renames specializations** to `__handle_prop__Scalar`
   and `__handle_prop__Nested` internally. User procs whose
   names start with `__` are forbidden — that prefix is
   reserved for compiler-emitted names.
5. **Synthesizes a public dispatcher**:
   ```tcl
   proc handle_prop {v args} {
     switch -- [lindex $v 0] {
       Scalar  { return [__handle_prop__Scalar  [lindex $v 1] {*}$args] }
       Nested  { return [__handle_prop__Nested  [lindex $v 1] {*}$args] }
     }
   }
   ```
   The payload is unwrapped before the specialization runs, so
   the body of `proc handle_prop {v: Property::Scalar}` sees
   `$v` as the bare string — matches Haskell `case` semantics.
6. **Registers a synthetic public signature** in the proc table
   under the public name. Specializations register under their
   mangled names so analyzer drill-down still finds them.

### What's NOT allowed

Two procs sharing a name where the first args aren't both
variants of one enum is a **hard error** ("ad-hoc overloading
not supported"). Examples:

- `proc foo {x: int}` + `proc foo {x: string}` — different
  primitives, no enum to dispatch on.
- `proc foo {x: Property::Scalar}` + `proc foo {x: Color::Red}`
  — different enums.
- `proc foo {x: Property::Scalar}` + `proc foo {x: Property::Scalar}`
  — duplicate variants.

If you legitimately want a single function that handles
unrelated types, rename one of them or wrap the union in an
enum.

## Recursive types

Enums and the types they reference can be mutually recursive:

```
enum Property = {
  Scalar: string
  Nested: Properties
}
type Properties = dict<string, Property>
```

`Property` references `Properties`, which references `Property`.
Codegen handles this fine — Tcl resolves proc references at call
time, not parse time, so the order in which the namespaces are
emitted doesn't matter. The validator's type-decl-table
collection runs to completion before per-type checks fire, so
forward references work.

## Worked example: `util::props`

The motivating case. Vivado property values are heterogeneous —
some scalars (`NAME cips`), some embedded dicts
(`CONFIG.PS_PMC_CONFIG CLOCK_MODE Custom DESIGN_MODE 1 …`).

Pre-enum (today): `util::props` returns `dict<string, string>`.
Embedded dict values render as long single lines that wrap at
the terminal — visually confusing.

With enums:

```
# types.htcl
enum Property = {
  Scalar: string
  Nested: Properties
}
type Properties = dict<string, Property>

# lift.htcl — the heuristic lives in a named, scoped place.
proc lift::vivado_property {obj: bd_cell; name: string; raw: string} Property {
  set kind [extern::report_property -type $obj $name]
  if {$kind eq "bool" || $kind eq "string" || $kind eq "long"} {
    return [Property::Scalar $raw]
  }
  # Composite: recurse through the embedded dict.
  set inner [dict create]
  foreach {k v} $raw {
    dict set inner $k [lift::vivado_property $obj "$name.$k" $v]
  }
  return [Property::Nested $inner]
}

# util.htcl — the wrapper just builds the typed result; the
# compiler handles the rendering via the auto-generated
# Property::repr and the monomorphized Properties::repr.
proc util::props {object: bd_cell} Properties {
  set result [dict create]
  foreach name [extern::list_property $object] {
    set raw [extern::get_property $name $object]
    dict set result $name [lift::vivado_property $object $name $raw]
  }
  return $result
}
```

In the REPL:

```
› util::props -object $cips
  CLASS Scalar(bd_cell)
  NAME Scalar(cips)
  CONFIG.PS_PMC_CONFIG Nested(
    CLOCK_MODE Scalar(Custom)
    DESIGN_MODE Scalar(1)
    PCIE_APERTURES_DUAL_ENABLE Scalar(0)
    …
  )
  …
```

— recursive structure rendered with no string-shape heuristics
in the compiler-emitted code. The `Property::repr` switch
dispatches on tag, `Properties::repr` (auto-monomorphized from
`dict<string, Property>`) iterates pairs and recurses.

## Out of scope for v1

- **Compiler-side auto-lowering at extern:: call sites** — would
  need cross-expression type inference. Wrappers explicitly
  unwrap via `<Enum>::payload`.
- **Ad-hoc overloading** (procs sharing a name where args aren't
  variants of one enum) — hard error; add as a distinct feature
  later if needed.
- **Multi-arg dispatch** (Julia-style) — first-arg dispatch only.
- **Generic enums** (`enum Result<T,E> = Ok: T | Err: E`) — needs
  type-parameter machinery; defer.
- **Pattern guards / nested patterns** — single arm per variant.
- **F#-style data-provider generation** for IP-XACT schemas —
  declarative schema → typed lifting code is a multi-week
  project of its own. Note as future direction.
