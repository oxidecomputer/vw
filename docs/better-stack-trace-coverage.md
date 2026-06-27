# Better stack-trace coverage via lowered proc-body instrumentation

## Problem

Some Vivado warnings and errors arrive with no stack trace because they
bypass every Tcl-level emission path our shim hooks. The canonical case
is `[IP_Flow 19-7090] Invalid parameter '…' provided, Ignoring`, emitted
from inside `set_property`'s C++ property validator. By the time those
bytes reach the worker's PTY reader, Tcl has already returned control to
the C++ caller and the Tcl call stack that produced them is gone —
neither the `puts` override nor the `send_msg_id` override fired.

Today we handle this by **per-command instrumentation**: a wrap around
`::set_property` in `vw-vivado/shim/vivado-shim.tcl::install_set_property_context`
captures the Tcl call stack via `info frame` just before delegating into
the underlying C++ command, then ferries the captured frames to the
worker through PTY-side `__VW_CTX_*` markers. The worker tags any
Warning/Error chunks arriving while a context is active.

This works for `set_property`. It doesn't work for any other Vivado
command that emits async warnings the same way — `create_bd_cell`,
`connect_bd_intf_net`, `validate_bd_design`, etc. Each one needs its own
wrap.

## Proposal

Replace per-command wraps with **universal coverage** by instrumenting
every lowered htcl proc body to maintain an explicit htcl-coordinate
stack in Tcl globals. When a warning arrives via PTY, the worker reads
the current top of that stack as the context.

This is the moral equivalent of "what if we just fed every statement
through Rust one at a time" — it gives us full visibility into the
runtime call chain without rewriting Tcl's proc dispatch. The data
lives in Tcl globals because that's where execution actually happens,
but we control what goes in and when, and the data is the same htcl
coordinates we'd track if we were stepping each statement from Rust.

### What the lowerer emits

`vw-htcl/src/lower.rs::lower_proc_decl` currently emits:

```tcl
proc configure_cips {args} { ::vw::kwargs $args {…}
…body statements at their source line numbers…
}
```

We'd extend it to bracket each body statement with push/pop calls:

```tcl
proc configure_cips {args} { ::vw::kwargs $args {…}
  ::vw::stack push "ip/cips.htcl:14 in ::configure_cips"
  <stmt 1>
  ::vw::stack swap "ip/cips.htcl:17 in ::configure_cips"
  <stmt 2>
  ::vw::stack swap "ip/cips.htcl:23 in ::configure_cips"
  <stmt 3>
  …
  ::vw::stack pop
}
```

`push` adds a new frame, `swap` replaces the current top in-place (so
we don't grow the stack one entry per statement), `pop` removes it at
proc exit.

Top-level (non-proc) statements get the same treatment in
`dispatch_eval`'s shipped script.

### Shim helpers

```tcl
namespace eval ::vw::stack {
    variable frames {}
    proc push {frame} {
        variable frames
        lappend frames $frame
    }
    proc swap {frame} {
        variable frames
        if {[llength $frames] > 0} {
            lset frames end $frame
        } else {
            lappend frames $frame
        }
    }
    proc pop {} {
        variable frames
        set frames [lrange $frames 0 end-1]
    }
    proc snapshot {} {
        variable frames
        return $frames
    }
}
```

The shim's existing `attach_stack_if_message` (puts override path) keeps
using `info frame` — it works fine. For the PTY-bypass path, we replace
the per-command marker wraps with a single hook that emits the snapshot
whenever Vivado is about to do something async. The cleanest version:
emit the snapshot **on every statement boundary**, so the worker always
has the latest context without needing per-command opt-in.

Concretely, every `stack swap` call also writes the new frame to the
PTY as a marker:

```tcl
proc swap {frame} {
    variable frames
    if {[llength $frames] > 0} {
        lset frames end $frame
    } else {
        lappend frames $frame
    }
    ::vw::emit_pty_ctx_replace $frames
}
```

`emit_pty_ctx_replace` writes one `__VW_CTX_BEGIN__` / frames /
`__VW_CTX_READY__` group, replacing whatever the worker currently has
active. No `__VW_CTX_END__` is sent — the context is always "the most
recent statement we entered." It gets replaced on the next statement
and reset when an eval completes (worker clears on `EvalDone`).

### Worker

The worker already handles `__VW_CTX_BEGIN__` / `__VW_CTX_FRAME__:` /
`__VW_CTX_READY__` / `__VW_CTX_END__` markers via
`worker.rs::consume_ctx_marker` and tags warnings/errors via
`emit_pty_chunk`. No changes needed there beyond:

- Treat absence of `__VW_CTX_END__` as "always active until eval ends."
  The worker should clear `active_pty_context` on `EvalDone` to avoid a
  context from one user submission contaminating the next.
- Drop `install_set_property_context` from the shim — universal
  coverage subsumes it.

### Rust-side resolver

`vw-repl/src/app.rs::resolve_stack_frames` already rewrites
`<input>:N in ::procname` to absolute `(file, line)` via the session's
proc table. No changes needed — the marker frames are already in that
shape.

## Tradeoffs

### Pros

- **Universal coverage.** Any Vivado command emitting async warnings
  gets a stack trace, not just `set_property`. We stop playing whack-
  a-mole every time a new IP throws a different warning class.
- **Removes per-command wraps.** `install_set_property_context` and
  any future siblings (`install_validate_bd_design_context`, etc.)
  all go away.
- **Statement-precise.** Currently the tagged frame for an
  IP_Flow warning points at the `set_property` call site. Under
  this scheme it points at the actual `create_versal_cips` call in
  the user's `configure_cips` body, because the most-recent `swap`
  captured *that* statement before Tcl dispatched into the wrapper
  that eventually called `set_property`. Closer to "what line of my
  code is responsible."

### Cons

- **Codegen overhead.** Every lowered proc body grows by ~one
  `::vw::stack swap` call per statement. For a file like
  `vivado-cmd/module.htcl` that's significant but not catastrophic;
  the strings are short and Tcl's bytecode compiler handles them
  cheaply. For `cpm5/module.htcl` (~880 procs, each with ~5–200
  statements) the lowered text grows by a similar factor — measure
  before / after on the `--load` cold-start time to know if it
  matters.
- **PTY marker volume.** Each `swap` writes a marker group
  (~3 lines) to the PTY. For an eval that fires 1000 statements,
  that's 3000 marker lines to filter out on the worker side.
  Cheap per line but adds up. Mitigate by only emitting markers when
  *about to* call into a typed external — but that gets us back to
  per-command opt-in.
- **Coupling to Tcl's eval order.** The htcl-coordinate stack only
  stays accurate if every statement reaches its `swap` call. If a
  Tcl `error`/`break`/`continue` jumps out of a body mid-statement,
  the `pop` at proc exit cleans up, but a partially-walked body
  could leave a stale frame as "current" until the next `swap` or
  `EvalDone` clears it. In practice this only affects the window
  between the error and the next event — same scope as the current
  per-command wrap.
- **Visible inside `if` / `foreach` bodies?** Control-flow constructs
  in htcl are braced Tcl scripts that Tcl evaluates internally —
  the lowerer doesn't walk into braced sub-scripts today, so an
  `if { … } { call X }` would only emit `swap` for the outer `if`,
  not for the inner `call X`. Whether that resolution matters
  depends on how often the user wants line-precise info for code
  inside an `if`. Could be added later by lowering braced bodies as
  scripts too.

## When to do it

Open question. The current per-command wrap covers `set_property`,
which empirically catches ~all the IP_Flow validation warnings the
user has hit so far. Universal coverage becomes worth the codegen
overhead when:

- A second Vivado command starts emitting async warnings we want
  traced (an obvious sign: someone adds an
  `install_<cmd>_context` proc and we realize it's the third one).
- The `set_property` wrap starts missing cases (e.g. Vivado adds a
  property-setter path that bypasses `::set_property`).
- We want statement-precise warning attribution rather than
  "warning happened during a `set_property` call in proc X" — i.e.
  the difference between `at ip/cips.htcl:69` (the `create_versal_cips`
  call) vs `at vivado-cmd/cmd/set_property.htcl:80` (inside the
  wrapper that eventually invoked `set_property`).

Until one of those bites, the per-command wrap is the cheaper bet.
This file is the breadcrumb for when it doesn't.

## References

- `vw-vivado/shim/vivado-shim.tcl::install_set_property_context` — the
  current per-command implementation
- `vw-vivado/src/worker.rs::consume_ctx_marker`,
  `vw-vivado/src/worker.rs::emit_pty_chunk` — the marker-consumer side
- `vw-htcl/src/lower.rs::lower_proc_decl` — where the
  push/swap/pop emission would go
- `vw-repl/src/app.rs::resolve_stack_frames` — the htcl-coordinate
  resolver, unchanged by this proposal
