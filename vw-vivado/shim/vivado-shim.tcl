# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.
#
# vw <-> Vivado wire protocol shim.
#
# Sourced by `vivado -mode tcl -source vivado-shim.tcl`. The shim
# connects back to vw over a loopback TCP socket on the port given in
# `$env(VW_PROTOCOL_ADDR)` and then reads newline-delimited JSON
# requests on that socket, writing responses on the same socket.
#
# Why a socket and not stdin/stdout: user TCL is free to call `puts`,
# Vivado prints its own banners and source-echo, and mixing all of
# that with the wire protocol on one stream forces either text
# markers (which a hostile or unlucky `puts` could spoof) or fragile
# OS-specific channels like FIFOs and chardevs. A loopback TCP socket
# works identically on Linux, macOS, and Windows, can't be polluted
# by anything the user writes to stdout, and frees vw to treat
# Vivado's stdout however it wants (forward for `vw run`, capture for
# the REPL/LSP).

package require json

namespace eval ::vw {
    variable protocol_sock {}
    # The eval id currently being processed. `puts` writes that would
    # go to stdout while `capturing` is set are forwarded immediately
    # as `{"id":N,"stream":"stdout","data":...}` notifications tagged
    # with this id, so vw can stream output as it's produced rather
    # than waiting for the final response. Required for any
    # long-running command (synth_design, route_design, ...).
    variable current_eval_id 0
    variable capturing 0
    # Reentrancy guard for the send_msg_id override. Without this, a
    # message emitted *during* our own stack-walking or JSON encoding
    # would recurse back into the override and either deadlock or
    # double-emit. We just fall back to the original handler when
    # already inside our wrapper.
    variable in_send_msg_id 0
    # Cap on stack frames captured per message. Vivado's internal
    # call chains can be 50+ frames deep through tclapp loaders;
    # rendering all of them would drown the actual message. The
    # cap is per-message, not per-session, so a future deeper trace
    # still gets its first N frames.
    variable stack_frame_cap 20
    # Set at startup from `VW_TRACE_STACK_CAPTURE`. When true,
    # `capture_stack` emits a per-frame `[vw-stack]` log line with
    # the info-frame dict, level-args probe, and keep/drop reason
    # for every frame it examines. Useful when a warning tag
    # renders with fewer frames than expected: the log shows
    # exactly which frames existed and why each was kept or
    # filtered. Zero cost when unset.
    variable trace_stack_capture 0
    catch {
        set trace_stack_capture \
            [expr {$::env(VW_TRACE_STACK_CAPTURE) eq "1"}]
    }
}

# ---------- puts capture ----------
#
# Rename the real `puts` so we can install a wrapper that, while
# capturing, forwards each stdout write to vw as a streaming
# notification. Anything that targets a specific channel (stderr,
# the protocol socket, a file) passes through unchanged. Outside of
# eval (`capturing == 0`), stdout writes also pass through — Vivado's
# own messages between commands stay on the process's stdout where vw
# handles them per its `--verbose` setting.

rename puts ::vw::real_puts

proc puts {args} {
    set len [llength $args]
    set start 0
    set nonewline 0
    if {$len > 0 && [lindex $args 0] eq "-nonewline"} {
        set nonewline 1
        set start 1
    }
    set remaining [expr {$len - $start}]
    if {$::vw::capturing} {
        if {$remaining == 1} {
            # `puts ?-nonewline? string` — implicit stdout
            set str [lindex $args $start]
            if {!$nonewline} { append str "\n" }
            # Catch-wrap because attach_stack_if_message is
            # defined later in this script. During shim sourcing
            # there's a tiny window where puts exists (this proc)
            # but the helper doesn't yet; we'd rather pass the
            # raw string through than crash the puts itself.
            catch {set str [::vw::attach_stack_if_message $str 2]}
            ::vw::stream_stdout $::vw::current_eval_id $str
            return
        } elseif {$remaining == 2 \
                && [lindex $args $start] eq "stdout"} {
            set str [lindex $args [expr {$start + 1}]]
            if {!$nonewline} { append str "\n" }
            catch {set str [::vw::attach_stack_if_message $str 2]}
            ::vw::stream_stdout $::vw::current_eval_id $str
            return
        }
    }
    # Fall through to the real puts for: any non-stdout channel, or
    # any stdout write when not capturing.
    ::vw::real_puts {*}$args
}

# ---------- JSON helpers ----------

# Hand-encode a string per RFC 8259. Vivado's bundled Tcllib doesn't
# include `json::write`, so we provide the minimum we need.
#
# Implementation: `string map` does the bulk-substitution in one
# native Tcl C call, vs. a per-char Tcl loop (which is what we
# used to do). The difference is dramatic at scale — a 1MB puts
# output (the kind `puts [util::props -object $cpm5]` produces)
# went from minutes of per-char `string index`/`scan`/`switch`
# iteration to ~100ms via `string map`. Rare control chars
# (codepoints < 0x20 other than the named whitespace escapes)
# trigger a slow per-char fallback; in practice Vivado property
# values don't contain them, so the fast path covers everything.
proc ::vw::json_string {value} {
    # Order matters: backslash must be substituted FIRST so the
    # backslashes we introduce for the other escapes aren't
    # themselves re-escaped.
    set escaped [string map [list \
        "\\" "\\\\" \
        "\"" "\\\"" \
        "\b" "\\b" \
        "\f" "\\f" \
        "\n" "\\n" \
        "\r" "\\r" \
        "\t" "\\t"] $value]
    # Fast path: no remaining control chars → just wrap in quotes.
    if {![regexp {[\x00-\x08\x0B\x0E-\x1F]} $escaped]} {
        return "\"$escaped\""
    }
    # Slow path: per-char loop for the remaining control chars.
    # Only hit when the string contains rare control codepoints
    # — Vivado property values shouldn't, but a user `puts` of
    # binary-ish data might.
    set out "\""
    set len [string length $escaped]
    for {set i 0} {$i < $len} {incr i} {
        set ch [string index $escaped $i]
        scan $ch %c codepoint
        if {$codepoint < 0x20} {
            append out [format "\\u%04x" $codepoint]
        } else {
            append out $ch
        }
    }
    append out "\""
    return $out
}

# ---------- response helpers ----------

# Send a streaming stdout chunk during eval. These notifications are
# distinguishable from the final response by the presence of a
# `stream` field (and absence of `ok`).
proc ::vw::stream_stdout {id data} {
    variable protocol_sock
    set j [::vw::json_string $data]
    ::vw::real_puts $protocol_sock \
        "{\"id\":$id,\"stream\":\"stdout\",\"data\":$j}"
    flush $protocol_sock
}

proc ::vw::send_ok {id result} {
    variable protocol_sock
    set j_result [::vw::json_string $result]
    # Use real_puts explicitly so the wrapper above can never
    # accidentally divert protocol traffic into a stream notification.
    ::vw::real_puts $protocol_sock \
        "{\"id\":$id,\"ok\":true,\"result\":$j_result}"
    flush $protocol_sock
}

proc ::vw::send_err {id message {code ""} {info ""}} {
    variable protocol_sock
    set j_msg [::vw::json_string $message]
    set fields "\"message\":$j_msg"
    if {$code ne ""} {
        append fields ",\"code\":[::vw::json_string $code]"
    }
    if {$info ne ""} {
        append fields ",\"info\":[::vw::json_string $info]"
    }
    ::vw::real_puts $protocol_sock \
        "{\"id\":$id,\"ok\":false,\"error\":{$fields}}"
    flush $protocol_sock
}

proc ::vw::log {msg} {
    puts stderr "\[vw-shim\] $msg"
    flush stderr
}

# Global-namespace call helper for wrappers.
#
# Each generated `vivado_cmd::<cmd>` wrapper forwards to the
# underlying Vivado builtin via this proc so the forwarded call
# runs with the interp's current namespace set to `::`. That
# matters for builtins like `synth_ip` which source XDC files
# whose scripts use *unqualified* names (`create_clock`,
# `get_ports`, …). Without a global-namespace call, Tcl resolves
# those unqualified names against whatever the wrapper's own
# namespace happens to be (`::vivado_cmd::`), and picks the
# wrapper again — which then throws on the XDC's positional
# args because kwargs expects `-flag` form.
#
# We define the proc at `::` (via `namespace eval ::`) so its
# execution context is `::`. The body invokes its args via
# `{*}$cmd {*}$args` — a direct arg-expansion, NOT a string
# re-parse. That's the critical difference from
# `namespace eval :: [list …]` / `namespace inscope :: …` /
# `uplevel #0 [list …]`, all of which serialize their args to a
# script string and lose Tcl_Obj internal reps. bd_cell handles
# would round-trip to plain paths like `/cpm5`, which Vivado's
# `set_property -objects` then rejects as "Invalid option value".
namespace eval :: {
    proc _vw_global_call {cmd args} {
        {*}$cmd {*}$args
    }
}

# ---------- kwargs runtime ----------
#
# Wrapper procs lowered from htcl declare themselves as
# `proc <name> {args} { ::vw::kwargs $args {param default ...} ; <body> }`.
# This helper parses `args` against `sig` (a dict of `param default
# param default ...`) and uses `upvar 1` to set each parameter as
# a local in the caller's frame. After this returns, the wrapper
# body sees `$dir`, `$cell`, `$name`, etc. just as if they were
# standard Tcl parameters with defaults.
#
# Why this exists: htcl is keyword-only at the call site, but Tcl
# proc dispatch is positional. Without this runtime parsing, the
# only way to make `wrap -name x` work would be to rewrite the
# call site to positional form at compile time — which our lowerer
# used to do, but only for top-level calls, not for calls inside
# proc bodies / namespace eval / [ ... ]. Moving the keyword parse
# to runtime makes every call site work uniformly.
#
# Arg shapes supported:
#   `-flag value`     — value-bearing flag, sets $flag = value
#   `-flag`           — bare boolean flag (end of args), sets $flag = 1
#   `-flag -other ...`— bare boolean flag (next token is another
#                       known flag), sets $flag = 1, continues
#
# The bare-flag heuristic matches Vivado's calling convention:
# their APIs and internal Tcl use `-quiet`/`-verbose`/etc. as bare
# booleans. The "is next token a known flag?" disambiguator avoids
# eating a legitimate value that happens to start with `-` (e.g.
# `-filter -name` where the user intended `-filter` to take `-name`
# as its value — but a leading `-` value is exotic enough that we
# accept the ambiguity).
proc ::vw::kwargs {argv sig} {
    # Initialize each parameter to its declared default. Also
    # initialize a `__vw_kw_<name>_set` flag to 0 — wrappers can
    # check this to distinguish "user supplied this arg" from
    # "we filled in the default", which matters for
    # set_property -dict where setting unsupplied properties
    # (with their defaults) re-validates the whole cell and
    # rejects values Vivado considers out of range for the
    # cell's current state.
    foreach {name default} $sig {
        upvar 1 $name var
        set var $default
        upvar 1 __vw_kw_${name}_set seen
        set seen 0
    }
    set n [llength $argv]
    set i 0
    while {$i < $n} {
        set flag [lindex $argv $i]
        if {![string match -* $flag]} {
            error "kwargs: expected -flag, got '$flag'"
        }
        set key [string range $flag 1 end]
        if {![dict exists $sig $key]} {
            set allowed [join [dict keys $sig] ", "]
            error "kwargs: unknown flag '$flag'; allowed: $allowed"
        }
        # Decide whether the current flag is bare or takes a value.
        # Bare iff: at end of args, OR next token is another known
        # -flag.
        set bare 1
        set next_i [expr {$i + 1}]
        if {$next_i < $n} {
            set peek [lindex $argv $next_i]
            if {![string match -* $peek]} {
                set bare 0
            } else {
                set peek_key [string range $peek 1 end]
                if {![dict exists $sig $peek_key]} {
                    # Peek looks like a flag but isn't ours — assume
                    # it's a value for the current flag (e.g. a CLI
                    # path or arg starting with `-`).
                    set bare 0
                }
            }
        }
        upvar 1 $key var
        upvar 1 __vw_kw_${key}_set seen
        set seen 1
        if {$bare} {
            set var 1
            incr i
        } else {
            set var [lindex $argv $next_i]
            incr i 2
        }
    }
}

# ---------- bulk property fetch ----------
#
# `::vw::props_dict <obj>` returns a paired Tcl list (NAME VAL
# NAME VAL …) of every property on `<obj>`. The point is a
# single Vivado RPC instead of N: htcl wrappers that want the
# full property bag (e.g. `util::props`) would otherwise issue
# one `extern::get_property` per property × hundreds of
# properties on an IP cell. The PTY round-trip is the dominant
# cost; doing the iteration entirely Vivado-side cuts it to
# constant per call.
proc ::vw::props_dict {obj} {
    set out [list]
    foreach name [list_property $obj] {
        lappend out $name [get_property $name $obj]
    }
    return $out
}

# ---------- user-set property tracking ----------
#
# Vivado offers no per-property "is this at the default?" API
# accessible from Tcl (`get_property`, `list_property`,
# `report_property -all`, `bd::get_properties` all return every
# property's current value with no user-vs-default distinction).
# The only system-of-record is `write_bd_tcl`'s output, which
# requires a full BD serialization round-trip per query.
#
# Instead we keep our own tally: every time
# `vivado_cmd::set_property` is invoked (the htcl-level chokepoint
# all wrappers and user code go through), it records the
# (object, name, value) triples here. `::vw::user_props_dict` /
# `::vw::user_props_nested` read back from this side-channel —
# returning ONLY the properties the user / wrapper explicitly
# pushed, never the ones Vivado cascaded as derived defaults.
#
# Cost: O(properties set) for record (dict insertion), O(props
# returned) for retrieval (dict walk). No file I/O, no full-
# design serialization. Persists across batches in the worker
# until `:restart`.
#
# Caveat: tracks only properties set via the
# `vivado_cmd::set_property` wrapper. Direct `extern::set_property`
# bypasses the recording. That's by design — the wrappers are
# the documented boundary, and bypassing them is an explicit
# opt-out of the tracking machinery.

namespace eval ::vw {
    variable user_set_props
}

# Record one or more (name, value) pairs as user-set on `obj`.
# `args` is the paired list (name1 val1 name2 val2 …) — same
# shape the wrapper builds before calling `set_property -dict`.
# Last-set wins per property name within an object.
proc ::vw::record_user_props {obj args} {
    variable user_set_props
    if {![info exists user_set_props]} {
        array set user_set_props {}
    }
    set key [::vw::_user_props_key $obj]
    if {![info exists user_set_props($key)]} {
        set user_set_props($key) [dict create]
    }
    set current $user_set_props($key)
    foreach {n v} $args {
        dict set current $n $v
    }
    set user_set_props($key) $current
}

# Canonical key for the per-object side-channel storage.
#
# `PATH` uniquely identifies a BD cell (`/cips/cpm5` etc.), but
# it's not defined on every Vivado object type — a project-level
# IP handle from `create_ip` / `get_ips` doesn't have one, and
# querying it emits `[Vivado 12-1341] Failed to get property
# 'PATH' on IP 'foo'` via `send_msg_id`. Tcl's `catch` doesn't
# suppress that because Vivado routes the error through the
# message bus rather than returning a Tcl-level error. `-quiet`
# on the getter does suppress it — that's why we use it here.
#
# The lookup falls through PATH → NAME → the raw object string
# so any object type produces a stable key without noise.
proc ::vw::_user_props_key {obj} {
    set p ""
    catch { set p [get_property -quiet PATH $obj] }
    if {$p ne ""} { return $p }
    catch { set p [get_property -quiet NAME $obj] }
    if {$p ne ""} { return $p }
    return $obj
}

# Parse a `::set_property` argv into (dict-pairs, objects) and
# funnel each object's pairs into [`record_user_props`]. Called by
# the [`install_set_property_recorder`] wrapper before the real
# set_property runs — so the tally reflects what the caller *tried*
# to set even if Vivado later rejects the write.
#
# Recognizes the two forms our IP wrappers actually emit:
#
#   set_property -dict {NAME1 VAL1 NAME2 VAL2 …} -objects $cell
#   set_property -name NAME -value VAL -objects $cell
#
# Positional-form `set_property NAME VAL $obj` also works. Unknown
# flags (`-quiet`, `-verbose`) are consumed silently. If we can't
# recover both a name/value dict and an object list, we return
# without recording — the write still happens; the tally just
# stays where it was.
proc ::vw::_record_set_property_call {args} {
    set pairs [list]
    set objects [list]
    set positional [list]
    set explicit_name ""
    set explicit_value ""
    set i 0
    while {$i < [llength $args]} {
        set tok [lindex $args $i]
        switch -- $tok {
            -dict {
                foreach {n v} [lindex $args [expr {$i + 1}]] {
                    lappend pairs $n $v
                }
                incr i 2
            }
            -objects {
                set objects [lindex $args [expr {$i + 1}]]
                incr i 2
            }
            -name {
                set explicit_name [lindex $args [expr {$i + 1}]]
                incr i 2
            }
            -value {
                set explicit_value [lindex $args [expr {$i + 1}]]
                incr i 2
            }
            -quiet -
            -verbose {
                incr i
            }
            default {
                lappend positional $tok
                incr i
            }
        }
    }
    # Positional shape: `set_property NAME VALUE OBJECTS`.
    if {[llength $positional] == 3 && [llength $pairs] == 0
        && $explicit_name eq ""} {
        lappend pairs [lindex $positional 0] [lindex $positional 1]
        if {[llength $objects] == 0} {
            set objects [lindex $positional 2]
        }
    }
    # Trailing positional target: `set_property -dict {…} $cell`.
    if {[llength $objects] == 0 && [llength $positional] == 1} {
        set objects [lindex $positional 0]
    }
    if {$explicit_name ne "" && [llength $pairs] == 0} {
        lappend pairs $explicit_name $explicit_value
    }
    if {[llength $pairs] == 0 || [llength $objects] == 0} {
        return
    }
    foreach obj $objects {
        catch { ::vw::record_user_props $obj {*}$pairs }
    }
}

# Install a `::set_property` wrapper that funnels every write
# through [`_record_set_property_call`] before delegating to
# whatever `::set_property` currently is (Vivado's C++ builtin, or
# the marker wrapper installed by
# [`install_all_context_wrappers`]). Regeneration-proof — the
# `vivado_cmd::set_property` htcl wrapper is generated from the
# Vivado command reference and re-emitted by `regenerate.sh`, so
# manual edits there don't survive. Keeping the recording here in
# the shim means the side-channel tally stays populated regardless
# of what the generated wrapper looks like.
#
# Called at startup AFTER [`install_all_context_wrappers`] so the
# recorder ends up OUTSIDE the marker wrapper: recording happens
# first, then BEGIN, then the real write, then END.
proc ::vw::install_set_property_recorder {} {
    if {[info commands ::vw::orig_set_property_for_record] ne ""} {
        return
    }
    if {[info commands ::set_property] eq ""} { return }
    rename ::set_property ::vw::orig_set_property_for_record
    proc ::set_property {args} {
        catch { ::vw::_record_set_property_call {*}$args }
        uplevel 1 [list ::vw::orig_set_property_for_record {*}$args]
    }
    ::vw::log "installed set_property recorder"
}

# Return the recorded (name, value) paired list for `obj`. Empty
# list when nothing has been recorded.
proc ::vw::user_props_dict {obj} {
    variable user_set_props
    if {![info exists user_set_props]} { return [list] }
    set key [::vw::_user_props_key $obj]
    if {![info exists user_set_props($key)]} { return [list] }
    set out [list]
    dict for {k v} $user_set_props($key) {
        lappend out $k $v
    }
    return $out
}

# Same shape as `::vw::props_nested` but seeded from the recorded
# user-set property tally instead of from `list_property` +
# `get_property`. Each value is classified via the structural
# `_lift_value` helper and inserted by dot-split path so the
# result is a nested `Properties` with only the explicitly-set
# sub-keys present.
proc ::vw::user_props_nested {obj} {
    set plain [dict create]
    foreach {name raw} [::vw::user_props_dict $obj] {
        set leaf [::vw::_lift_value $raw]
        dict set plain {*}[split $name "."] $leaf
    }
    return [::vw::_wrap_nested $plain]
}

# `::vw::props_nested <obj>` returns the FULL output `util::props`
# wants — a nested `Properties` dict where dotted property names
# (CONFIG.X.Y) expand into hierarchy, and each leaf value is
# already a `[list Scalar <v>]` or `[list Nested <inner>]` tuple.
#
# Lives in the shim (plain Tcl) rather than in user-side htcl
# because:
#  - One Vivado RPC for the entire fetch + classification +
#    nesting pipeline, vs. one RPC for the fetch + thousands
#    of htcl-proc kwargs-envelope invocations per recursive
#    sub-key for the post-processing.
#  - CPM5 has ~200 top-level properties, each whose value is
#    itself a paired-dict with dozens of sub-keys. The htcl-
#    side post-processing was hitting tens of thousands of
#    kwargs invocations × envelope overhead → minutes. Native
#    Tcl inside Vivado does the same work in well under a
#    second.
#
# The structural classifier (`::vw::_lift_value`) mirrors what
# `lift::lift_recursive` did in user-htcl: pure shape inference,
# no Vivado lookups. The wrap step (`::vw::_wrap_nested`) walks
# the plain nested dict once and tags intermediate levels as
# `Property::Nested(...)`. Leaves already carry their tag from
# `_lift_value`.
proc ::vw::props_nested {obj} {
    set plain [dict create]
    foreach name [list_property $obj] {
        set raw [get_property $name $obj]
        set leaf [::vw::_lift_value $raw]
        dict set plain {*}[split $name "."] $leaf
    }
    return [::vw::_wrap_nested $plain]
}

# Structural inference on a raw property value. Returns a
# `[list Scalar v]` or `[list Nested inner]` tuple. Mirror of
# lift::looks_like_paired_dict + lift::lift_recursive in plain
# Tcl with no kwargs envelope.
proc ::vw::_lift_value {raw} {
    if {[catch {llength $raw} n]} { return [list Scalar $raw] }
    if {$n == 0 || $n % 2 != 0} { return [list Scalar $raw] }
    foreach {k _v} $raw {
        if {![regexp {^[A-Za-z_][A-Za-z0-9_.]*$} $k]} {
            return [list Scalar $raw]
        }
    }
    set inner [dict create]
    foreach {k v} $raw {
        dict set inner $k [::vw::_lift_value $v]
    }
    return [list Nested $inner]
}

# Walk a plain nested Tcl dict and wrap each intermediate
# level as `[list Nested <inner>]`. A value is a leaf when
# it's a 2-element list whose head is "Scalar" or "Nested"
# (the existing Property tuple shape). Anything else is a
# sub-dict to descend into.
proc ::vw::_wrap_nested {plain} {
    set out [dict create]
    dict for {k v} $plain {
        if {[llength $v] == 2 \
            && ([lindex $v 0] eq "Scalar" \
                || [lindex $v 0] eq "Nested")} {
            dict set out $k $v
        } else {
            dict set out $k [list Nested [::vw::_wrap_nested $v]]
        }
    }
    return $out
}

# `::vw::config_from_dotted_pairs {pairs}` — lift a flat paired-
# list of `dotted.key raw-value` entries into a nested tagged
# `Properties` value. Same transform `::vw::props_nested`
# performs on `list_property` output — split each key on `.`,
# insert at the resulting path in a plain nested dict, `_wrap_nested`
# to tag intermediate levels, `_lift_value` to tag each leaf.
#
# The generated `<ip>::configure` procs call this in their bodies
# to convert the assembled `_vw_d` (built by `lappend _vw_d
# CONFIG.<PARAM> <value>` loops) into the proper `<ip>::Config`
# shape: a Properties value where CONFIG at the top wraps a
# `Property::Nested` containing every `<PARAM>` sub-key. Consumers
# then use `dict get [<ip>::Config::to -v $cfg] CONFIG` +
# `Property::as_nested -v ...` to extract the sub-tree, matching
# the pattern `props::get` documents.
proc ::vw::config_from_dotted_pairs {pairs} {
    set plain [dict create]
    foreach {name raw} $pairs {
        set leaf [::vw::_lift_value $raw]
        dict set plain {*}[split $name "."] $leaf
    }
    return [::vw::_wrap_nested $plain]
}

# `::vw::config_to_dotted_flat {nested}` — inverse of the lift.
# Walks a nested tagged Properties tree and emits a flat paired
# list `TOP.LEAF value TOP.LEAF value ...` matching the shape
# Vivado's `set_property -dict` expects for an IP cell.
#
# **Depth invariant.** The generated `<ip>::configure` procs
# always assemble `_vw_d` with keys of the form `CONFIG.<PARAM>`
# (one dot at the top, two path segments). `Properties::from_
# dotted_pairs` splits on `.` and inserts, producing a two-level
# tagged structure: root → Nested-wrapped CONFIG → tagged entries
# (one per Vivado property).
#
# So the flatten pairs off exactly two levels: iterate the root
# dict for TOP keys (`CONFIG`), unwrap that Nested to get the
# entries, then emit `TOP.LEAF = untag(value)` per entry. A Scalar
# entry unwraps to its bare string. A Nested entry — e.g.
# `CONFIG.CPM_CONFIG` where the caller passed a Properties value —
# unwraps to its raw paired-list dict (recursively stripping any
# further tags inside), which is what Vivado stores as the value
# of a nested-dict property.
#
# Naively recursing past the two-level structure would emit
# `CONFIG.CPM_CONFIG.CPM_PCIE0_MODES` which Vivado then rejects
# with `[BD 41-1276] Cannot set the parameter … Parameter does
# not exist`, since CPM_CONFIG is a single property (accepting a
# nested dict value), not a namespace.
proc ::vw::config_to_dotted_flat {nested} {
    set out [list]
    dict for {top_key top_val} $nested {
        set top_tag [lindex $top_val 0]
        set top_payload [lindex $top_val 1]
        if {$top_tag ne "Nested"} {
            # Unexpected shape at the root — configure-built Configs
            # always wrap the top namespace as Nested via
            # _wrap_nested. Emit under the raw key rather than
            # silently drop.
            lappend out $top_key [::vw::_untag_recursive $top_val]
            continue
        }
        dict for {leaf_key leaf_val} $top_payload {
            lappend out "$top_key.$leaf_key" \
                [::vw::_untag_recursive $leaf_val]
        }
    }
    return $out
}

# Recursively strip `Property::Scalar` / `Property::Nested` tags
# from a tagged Properties value. Scalar returns its bare string.
# Nested returns its inner dict with each value recursively
# untagged. Used inside `config_to_dotted_flat` to convert a
# Nested-typed property's tagged payload into the raw paired
# dict Vivado expects as that property's value.
proc ::vw::_untag_recursive {tagged} {
    if {[llength $tagged] != 2} { return $tagged }
    set tag [lindex $tagged 0]
    set payload [lindex $tagged 1]
    if {$tag eq "Scalar"} {
        return $payload
    } elseif {$tag eq "Nested"} {
        set out [list]
        dict for {k v} $payload {
            lappend out $k [::vw::_untag_recursive $v]
        }
        return $out
    }
    return $tagged
}

# ---------- send_msg_id override ----------
#
# Why we override: when Vivado emits a WARNING/ERROR/INFO/CRITICAL
# WARNING via ::common::send_msg_id, the raw line goes to stdout
# with no call-context — the user sees `WARNING: [Common 17-1496]
# ...` and has no way to tell which Tcl proc triggered it. Hooking
# the Tcl entry point lets us capture the call stack at emit time
# and render it as `at file:line in proc` continuation lines.
#
# Tradeoffs to be aware of:
#   - The original ::common::send_msg_id is NOT called. That means
#     `set_msg_config -id X -suppress` won't suppress Tcl-emitted
#     messages (it still works for messages Vivado's C code emits,
#     which our PTY-level filter handles). Acceptable for v1; we
#     can replicate suppression here if it becomes a real need.
#   - Messages emitted from Vivado's C code (synth, route, etc.)
#     bypass this override and are caught by the PTY-line filter
#     in the worker, with no stack — that's a fundamental limit.

# True when `str`'s first line looks like a Vivado-standard
# message: starts (after optional leading whitespace) with
# ERROR:/WARNING:/CRITICAL WARNING:/INFO:. Used by the puts
# wrapper to decide whether to attach a stack — we only want
# traces on message-formatted output, not on every `puts hi`.
proc ::vw::is_vivado_message {str} {
    set first $str
    set nl [string first "\n" $str]
    if {$nl >= 0} {
        set first [string range $str 0 [expr {$nl - 1}]]
    }
    set trimmed [string trimleft $first]
    if {[string match "ERROR:*" $trimmed]} { return 1 }
    if {[string match "CRITICAL WARNING:*" $trimmed]} { return 1 }
    if {[string match "WARNING:*" $trimmed]} { return 1 }
    if {[string match "INFO:*" $trimmed]} { return 1 }
    return 0
}

# Severity of a Vivado-style message: one of `ERROR`, `CRITICAL`,
# `WARNING`, `INFO`. Returns empty for non-messages. Used by
# `attach_stack_if_message` to decide whether the stack is worth
# attaching (INFO is suppressed by default — see VW_INFO_WITH_STACK).
proc ::vw::message_severity {str} {
    set first $str
    set nl [string first "\n" $str]
    if {$nl >= 0} {
        set first [string range $str 0 [expr {$nl - 1}]]
    }
    set trimmed [string trimleft $first]
    if {[string match "ERROR:*" $trimmed]} { return "ERROR" }
    if {[string match "CRITICAL WARNING:*" $trimmed]} { return "CRITICAL" }
    if {[string match "WARNING:*" $trimmed]} { return "WARNING" }
    if {[string match "INFO:*" $trimmed]} { return "INFO" }
    return ""
}

# If `str` looks like a Vivado-style message, append the current
# Tcl call stack as `\n  at <frame>` continuation lines and
# return the augmented string. Otherwise return `str` unchanged.
#
# `skip_caller_frames` tells the stack walk how many wrapper
# layers to step past so the deepest reported frame is the user's
# code, not our shim's plumbing. For the puts wrapper that's 2
# (this helper + the puts wrapper itself).
proc ::vw::attach_stack_if_message {str skip_caller_frames} {
    if {![::vw::is_vivado_message $str]} {
        return $str
    }
    # INFO messages are noisy under heavy Vivado activity (CIPS
    # customization emits dozens per call). By default we suppress
    # their stack so the scrollback stays scannable. The user opts
    # in with `vw repl --info-with-stack` (or the `vw run` flag),
    # which sets VW_INFO_WITH_STACK=1 on the spawned process.
    # WARNING / ERROR / CRITICAL always keep their stacks.
    if {[::vw::message_severity $str] eq "INFO"} {
        set env_default 0
        catch { set env_default $::env(VW_INFO_WITH_STACK) }
        if {$env_default ne "1"} {
            return $str
        }
    }
    set stack [::vw::capture_stack $skip_caller_frames]
    if {[llength $stack] == 0} {
        return $str
    }
    set has_trailing_nl 0
    set body $str
    if {[string index $str end] eq "\n"} {
        set has_trailing_nl 1
        set body [string range $str 0 end-1]
    }
    foreach frame $stack {
        append body "\n  at $frame"
    }
    if {$has_trailing_nl} { append body "\n" }
    return $body
}

# Walk the Tcl call stack starting at the caller of our override
# (`info frame 1` — skipping our wrapper itself) and build a list
# of "at file:line in proc" strings, deepest-first. Uses both
# `info frame` (gives script file/line) and `info level` (gives
# proc name + args) for each depth; merges whatever's available.
# Capped at `$::vw::stack_frame_cap` frames so a 50-deep tclapp
# loader chain doesn't drown the actual message.
#
# Returns at least one entry even when nothing is locatable —
# `(stack: depth=N, no locatable frames)` so the user can
# distinguish "override didn't fire" from "override fired but
# Tcl gave us nothing to render."
proc ::vw::capture_stack {skip_caller_frames} {
    variable stack_frame_cap
    variable trace_stack_capture
    set out [list]
    set depth [info frame]
    set level_depth [info level]
    # Skip our own frame plus whatever the caller asked us to skip.
    set start [expr {1 + $skip_caller_frames}]
    if {$trace_stack_capture} {
        ::vw::log "\[vw-stack\] BEGIN capture skip=$skip_caller_frames\
                   depth=$depth level_depth=$level_depth start=$start"
    }
    for {set i $start} {$i <= $depth} {incr i} {
        if {[llength $out] >= $stack_frame_cap} { break }
        set frame ""
        catch {set frame [info frame -$i]}
        # `info level -k` is indexed independently of `info frame`
        # — k=0 is the current proc, k=-1 the caller, etc. We map
        # frame index i to level index k by clamping; mismatches
        # are common (frames can include non-proc evals) but worth
        # trying as a fallback.
        set level_args ""
        set k [expr {$i - $skip_caller_frames - 1}]
        if {$k > 0 && $k < $level_depth} {
            catch {set level_args [info level -$k]}
        }
        set entry [::vw::format_frame $frame $level_args]
        if {$trace_stack_capture} {
            set kept "kept"
            if {$entry eq ""} { set kept "dropped" }
            ::vw::log "\[vw-stack\] i=$i k=$k $kept frame=\{$frame\}\
                       level_args=\{$level_args\}\
                       entry=\"$entry\""
        }
        if {$entry ne ""} { lappend out $entry }
    }
    if {$trace_stack_capture} {
        ::vw::log "\[vw-stack\] END capture out=\{[join $out { | }]\}"
    }
    if {[llength $out] == 0} {
        lappend out "(stack: info-frame-depth=$depth\
                     info-level-depth=$level_depth\
                     — no locatable frames; message likely\
                     emitted from byte-compiled or C-bridged Tcl)"
    }
    return $out
}

# Turn one `info frame` dict (and an optional `info level` args
# list as a fallback proc-name source) into the human-readable
# string we render. Drops frames that have nothing locatable at
# all — they're just noise.
proc ::vw::format_frame {frame level_args} {
    set proc ""
    catch {set proc [dict get $frame proc]}
    set file ""
    catch {set file [dict get $frame file]}
    set line ""
    catch {set line [dict get $frame line]}
    set cmd ""
    catch {set cmd [dict get $frame cmd]}
    # `info level -k` returns the proc invocation as `procname
    # arg1 arg2 ...`; the first element is the proc name.
    if {$proc eq "" && $level_args ne ""} {
        set proc [lindex $level_args 0]
    }

    # Drop frames that are part of our own plumbing — they're
    # always noise to the user. The signal in a stack trace is
    # "which line of MY code led to this message"; frames in
    # the shim file, the ::vw:: namespace, our send_msg_id
    # override, or the ::log:: helpers are all infrastructure.
    if {[string match "*vivado-shim.tcl" $file]} {
        return ""
    }
    if {[string match "::vw::*" $proc]} {
        return ""
    }
    if {$proc eq "::common::send_msg_id"} {
        return ""
    }
    if {[string match "::log::*" $proc]} {
        return ""
    }

    set location ""
    if {$file ne "" && $line ne ""} {
        set location "${file}:${line}"
    } elseif {$line ne ""} {
        # `eval` frames without a source file — common for our
        # `uplevel #0 $tcl` shim entry — still tell the user
        # "line N of the script you submitted."
        set location "<input>:${line}"
    }
    if {$location ne "" && $proc ne ""} {
        return "${location} in ${proc}"
    } elseif {$location ne ""} {
        return $location
    } elseif {$proc ne ""} {
        return $proc
    } elseif {$cmd ne ""} {
        # Last-ditch: no proc and no location, but we know what
        # command this frame was running. Truncate so a very long
        # command doesn't blow out the trace.
        set short [string range $cmd 0 80]
        if {[string length $cmd] > 80} { append short "..." }
        return "(cmd: $short)"
    }
    return ""
}

# Severity normalizer. Vivado is inconsistent about case and
# uses underscores in CRITICAL_WARNING; we normalize to the same
# uppercase, space-separated form the PTY-line classifier expects
# so the worker can route warnings/errors to the right StreamKind.
proc ::vw::normalize_severity {sev} {
    set s [string toupper [string trim $sev]]
    switch -- $s {
        "CRITICAL_WARNING" -
        "CRITICAL WARNING" { return "CRITICAL WARNING" }
        "ERROR" -
        "FATAL" -
        "FATAL_ERROR" { return "ERROR" }
        "WARNING" { return "WARNING" }
        "INFO" -
        "STATUS" { return "INFO" }
        default { return $s }
    }
}

# Install our wrapper *after* Vivado has had a chance to define
# ::common::send_msg_id. If the proc doesn't exist yet (very early
# init, headless mode without the common namespace), we silently
# skip — Vivado's PTY emission still works, just without our
# stack capture.
#
# Logs status once per successful install and once per skipped
# attempt (with the reason), so the user can see in the REPL
# whether the override is live without enabling --verbose.
proc ::vw::install_send_msg_override {} {
    if {[info commands ::vw::orig_send_msg_id] ne ""} {
        # Already installed — silent on the retry path so we don't
        # spam the log on every eval.
        return
    }
    set candidates [info commands ::common::send_msg*]
    if {[info commands ::common::send_msg_id] eq ""} {
        ::vw::log "::common::send_msg_id not present;\
                   ::common::send_msg* = {$candidates};\
                   stack-capture override NOT installed"
        return
    }
    rename ::common::send_msg_id ::vw::orig_send_msg_id

    # The Vivado-Tcl signature is `send_msg_id id severity msg
    # [optional args]`. We accept the same.
    proc ::common::send_msg_id {id severity msg args} {
        # Reentrancy guard — if our stack walk somehow triggers
        # another send_msg_id, fall back to the original.
        if {$::vw::in_send_msg_id} {
            return [uplevel 1 [list ::vw::orig_send_msg_id \
                $id $severity $msg {*}$args]]
        }
        set ::vw::in_send_msg_id 1
        set ok [catch {
            set sev_norm [::vw::normalize_severity $severity]
            # INFO is noisy — suppress the stack by default, matching
            # the puts-wrapper path. The user opts in with `vw repl
            # --info-with-stack` (worker exports VW_INFO_WITH_STACK=1).
            # WARNING / ERROR / CRITICAL always keep their stacks.
            set attach_stack 1
            if {$sev_norm eq "INFO"} {
                set env_default 0
                catch { set env_default $::env(VW_INFO_WITH_STACK) }
                if {$env_default ne "1"} { set attach_stack 0 }
            }
            set out "${sev_norm}: \[${id}\] ${msg}"
            if {$attach_stack} {
                # Skip 1 caller frame so the deepest frame in the
                # rendered stack is the one that called send_msg_id,
                # not the user proc that called our wrapper.
                set stack [::vw::capture_stack 1]
                foreach frame $stack {
                    append out "\n  at ${frame}"
                }
            }
            if {$::vw::capturing} {
                ::vw::stream_stdout $::vw::current_eval_id "$out\n"
            } else {
                # Outside an eval — fall back to the original so the
                # message still appears wherever Vivado normally
                # would have put it.
                ::vw::orig_send_msg_id $id $severity $msg {*}$args
            }
        } err]
        set ::vw::in_send_msg_id 0
        if {$ok != 0} {
            # Our override threw — never let that prevent Vivado from
            # at least seeing the message. Fall through to original.
            ::vw::log "send_msg_id override failed: $err"
            return [uplevel 1 [list ::vw::orig_send_msg_id \
                $id $severity $msg {*}$args]]
        }
    }
    ::vw::log "installed send_msg_id override"
}

# Commands wrapped with [`install_command_context`] so any
# traceless WARNINGs / ERRORs Vivado's C++ side emits during the
# call get the Tcl call stack attached. Each name MUST be a global
# Tcl command (no leading `::` — the wrapper installs as `::$name`).
# The list is open-ended: add a command here whenever a user hits a
# new noisy builtin and the worker filter shows the warning landing
# without a stack. There's no observable cost — the wrapper is a
# thin around-trace, only the message-tagging window is widened.
set ::vw::context_wrapped_commands {
    set_property
    generate_netlist_ip
}

# Wrap a single Vivado command so we can attach the Tcl call stack
# to warnings/errors its C++ implementation emits. The C++ paths
# (notably `[IP_Flow 19-7090] Invalid parameter '…' provided,
# Ignoring` for `set_property`, `[Coretcl 2-176] No IPs found` for
# `generate_netlist_ip`) bypass `::common::send_msg_id` and write
# directly through Vivado's internal message bus to the PTY —
# there's no Tcl frame to grab by the time the bytes arrive at the
# Rust worker. So we capture the stack here, while the Tcl
# interpreter is *about* to enter the C++ command, emit it as a
# marker the worker recognizes and strips, then the worker tags any
# warnings that arrive while the marker is active. Markers go via
# `::vw::real_puts stdout` so they bypass our own `puts` override
# and land on the PTY directly. Idempotent — re-running this on
# every eval is harmless once the wrapper is in place.
proc ::vw::install_command_context {name} {
    set orig "::vw::orig_${name}_for_ctx"
    if {[info commands $orig] ne ""} {
        return
    }
    if {[info commands ::$name] eq ""} { return }
    rename ::$name $orig
    # Build the wrapper body with `$orig` interpolated, NOT
    # `$name` — the wrapper has to forward to the renamed original
    # without a name-lookup detour. `set rc` is computed and
    # forwarded so the wrapped command's return value, error code,
    # and -errorinfo all flow back unchanged to the caller.
    proc ::$name {args} [string map [list @ORIG@ $orig] {
        # Skip 1 = this wrapper's own frame, so the deepest reported
        # frame is the user proc that called the wrapped command.
        set frames [::vw::capture_stack 1]
        ::vw::emit_pty_ctx_begin $frames
        set rc [catch {
            uplevel 1 [list @ORIG@ {*}$args]
        } result options]
        ::vw::emit_pty_ctx_end
        return -options $options $result
    }]
    ::vw::log "installed context wrap for ::$name"
}

# Install context wrappers for every command in
# `::vw::context_wrapped_commands`. Called once after the protocol
# socket opens and again at the top of every eval — each
# `install_command_context` is idempotent, so re-attempts are cheap
# once installed and recover gracefully when a command first
# appears after a later library was sourced.
proc ::vw::install_all_context_wrappers {} {
    foreach name $::vw::context_wrapped_commands {
        catch {::vw::install_command_context $name}
    }
}

# Push a context marker onto the PTY. Format: a sentinel-prefixed
# line per frame plus begin/end bookends, so the Rust PTY filter
# can match line-by-line without needing a base64 decoder.
proc ::vw::emit_pty_ctx_begin {frames} {
    ::vw::real_puts stdout "__VW_CTX_BEGIN__"
    foreach f $frames {
        ::vw::real_puts stdout "__VW_CTX_FRAME__:$f"
    }
    ::vw::real_puts stdout "__VW_CTX_READY__"
    flush stdout
}

proc ::vw::emit_pty_ctx_end {} {
    ::vw::real_puts stdout "__VW_CTX_END__"
    flush stdout
}

# ---------- user-proc body wrap ----------
#
# Traceless warnings from Vivado's C++ (e.g. `[Coretcl 2-176] No
# IPs found`) bypass `::common::send_msg_id`, so the per-command
# wrappers above only tag warnings emitted while THAT specific
# command is in flight. Real-world Vivado calls are deeper — a
# warning inside `generate_netlist_ip` may actually come from a
# nested `get_ips` call inside the C++ path — and enumerating
# every possible culprit isn't tractable.
#
# So we also instrument every USER-defined proc: rewrite each new
# proc's body to emit a marker BEGIN/READY at entry (with
# `capture_stack` from *inside* the body — the frame stack
# includes the proc itself) and an END on exit. The Rust
# marker-stack tracks nesting, so nested user procs each get their
# own frames and the innermost wins for tagging. Result: a warning
# fired anywhere under `configure_clock`'s call chain lands with
# `at ip/clock.htcl:N in ::configure_clock` attached, even when
# Vivado's C++ emits it silently.
#
# Only fires while `::vw::capturing == 1` — i.e. during a user
# eval — so Vivado's own Tcl-lib initialization defines procs
# unchanged. `::vw::*` procs are also skipped so our own plumbing
# doesn't recurse into itself.

# Compute the fully-qualified name a `proc NAME ...` invocation
# would produce, given the caller's namespace. Used by the
# ::proc override's filter — we only wrap user procs that end up
# in the top-level `::` namespace, which is where htcl-lowered
# procs live.
proc ::vw::qualify_proc_name {name caller_ns} {
    if {[string match ::* $name]} { return $name }
    set caller_ns [string trimright $caller_ns ::]
    if {$caller_ns eq ""} { return ::$name }
    return ${caller_ns}::$name
}

# Install the ::proc override. Idempotent — re-running is cheap
# once the wrapper is in place. The original `proc` is renamed to
# `::vw::orig_proc_for_body_wrap` and delegated to.
proc ::vw::install_proc_body_wrap {} {
    if {[info commands ::vw::orig_proc_for_body_wrap] ne ""} {
        return
    }
    rename ::proc ::vw::orig_proc_for_body_wrap
    # Marker template: `@BODY@` gets literal-substituted with the
    # user's body via `string map`, avoiding format/subst
    # interpolation risks. `catch` preserves rc/result/errorcode/
    # errorinfo across the wrap so the wrapped proc behaves
    # identically to the unwrapped original.
    variable proc_body_template {
        ::vw::emit_pty_ctx_begin [::vw::capture_stack 0]
        set _vw_ctx_rc [catch {@BODY@} _vw_ctx_result _vw_ctx_opts]
        ::vw::emit_pty_ctx_end
        return -options $_vw_ctx_opts $_vw_ctx_result
    }
    ::vw::orig_proc_for_body_wrap ::proc {name spec body} {
        # Delegate straight through when we're not inside a user
        # eval — Vivado's own lib procs go untouched.
        if {!$::vw::capturing} {
            return [uplevel 1 [list ::vw::orig_proc_for_body_wrap \
                $name $spec $body]]
        }
        set caller_ns [uplevel 1 { namespace current }]
        set qualified [::vw::qualify_proc_name $name $caller_ns]
        # Skip our own helpers and any Vivado internal proc that a
        # user eval might reach into. `::vw::*` covers our shim,
        # `::tcl::*` guards Tcl's core, everything else in the
        # top-level or user namespaces gets the wrap.
        if {[string match ::vw::* $qualified]
            || [string match ::tcl::* $qualified]} {
            return [uplevel 1 [list ::vw::orig_proc_for_body_wrap \
                $name $spec $body]]
        }
        # Detect a re-wrap: if the body already contains our
        # marker call, don't stack another layer around it. Redefs
        # of a user proc during eval (e.g. re-`src`ing a library)
        # would otherwise nest wrappers on every reload.
        if {[string first "::vw::emit_pty_ctx_begin" $body] >= 0} {
            return [uplevel 1 [list ::vw::orig_proc_for_body_wrap \
                $name $spec $body]]
        }
        set new_body [string map \
            [list @BODY@ $body] $::vw::proc_body_template]
        uplevel 1 [list ::vw::orig_proc_for_body_wrap $name $spec $new_body]
    }
    ::vw::log "installed ::proc body wrap"
}

# ---------- dispatch ----------

proc ::vw::dispatch {line} {
    if {[catch {::json::json2dict $line} req]} {
        ::vw::send_err 0 "protocol parse error: $req"
        return
    }
    if {![dict exists $req id] || ![dict exists $req op]} {
        ::vw::send_err 0 "missing id or op"
        return
    }
    set id [dict get $req id]
    set op [dict get $req op]
    switch -- $op {
        eval {
            if {![dict exists $req tcl]} {
                ::vw::send_err $id "eval request missing tcl field"
                return
            }
            set tcl [dict get $req tcl]
            set ::vw::current_eval_id $id
            set ::vw::capturing 1
            set rc [catch {uplevel #0 $tcl} result opts]
            set ::vw::capturing 0
            if {$rc != 0} {
                set ecode ""
                set einfo ""
                catch {set ecode [dict get $opts -errorcode]}
                catch {set einfo [dict get $opts -errorinfo]}
                ::vw::send_err $id $result $ecode $einfo
            } else {
                ::vw::send_ok $id $result
            }
        }
        shutdown {
            ::vw::send_ok $id ""
            ::vw::log "shim shutting down"
            exit 0
        }
        default {
            ::vw::send_err $id "unknown op: $op"
        }
    }
}

# ---------- main ----------

if {![info exists ::env(VW_PROTOCOL_ADDR)]} {
    ::vw::log "VW_PROTOCOL_ADDR not set; exiting"
    exit 1
}

if {![regexp {^(.*):(\d+)$} $::env(VW_PROTOCOL_ADDR) -> ::vw::host ::vw::port]} {
    ::vw::log "invalid VW_PROTOCOL_ADDR: $::env(VW_PROTOCOL_ADDR)"
    exit 1
}

if {[catch {socket $::vw::host $::vw::port} sock]} {
    ::vw::log "failed to connect to $::vw::host:$::vw::port: $sock"
    exit 1
}

set ::vw::protocol_sock $sock
fconfigure $sock -buffering line -translation lf

::vw::log "connected to $::vw::host:$::vw::port"

# Try installing the send_msg_id override now. If Vivado hasn't
# defined ::common::send_msg_id yet (unusual but possible in
# headless / minimal-mode configurations), the override will be
# re-attempted on the first eval — it's idempotent.
catch {::vw::install_send_msg_override}
catch {::vw::install_all_context_wrappers}
catch {::vw::install_set_property_recorder}
catch {::vw::install_proc_body_wrap}

# Silence Vivado's per-command performance report — the
# `<cmd>: Time (s): cpu = … Memory (MB): peak = …` chatter that
# appears after any command whose elapsed time exceeds
# `tcl.statsThreshold` seconds (default: quite low, so most
# `create_bd_cell` calls trip it). Users who WANT the stats can
# re-lower the threshold in their own script; setting it high
# by default keeps the REPL / `vw run` output focused on the
# user's actual results.
catch {set_param tcl.statsThreshold 9999999}

while {1} {
    if {[gets $sock line] < 0} {
        if {[eof $sock]} {
            ::vw::log "protocol socket closed; exiting"
            break
        }
        continue
    }
    set line [string trim $line]
    if {$line eq ""} { continue }
    # Retry installs on each eval until they succeed — both procs
    # bail out cheaply once installed.
    catch {::vw::install_send_msg_override}
    catch {::vw::install_all_context_wrappers}
catch {::vw::install_set_property_recorder}
catch {::vw::install_proc_body_wrap}
    ::vw::dispatch $line
}

close $sock
exit 0
