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
proc ::vw::json_string {value} {
    set out "\""
    set len [string length $value]
    for {set i 0} {$i < $len} {incr i} {
        set ch [string index $value $i]
        scan $ch %c codepoint
        switch -- $ch {
            "\\" { append out "\\\\" }
            "\"" { append out "\\\"" }
            "\b" { append out "\\b" }
            "\f" { append out "\\f" }
            "\n" { append out "\\n" }
            "\r" { append out "\\r" }
            "\t" { append out "\\t" }
            default {
                if {$codepoint < 0x20} {
                    append out [format "\\u%04x" $codepoint]
                } else {
                    append out $ch
                }
            }
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
    # Initialize each parameter to its declared default.
    foreach {name default} $sig {
        upvar 1 $name var
        set var $default
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
        if {$bare} {
            set var 1
            incr i
        } else {
            set var [lindex $argv $next_i]
            incr i 2
        }
    }
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
    set out [list]
    set depth [info frame]
    set level_depth [info level]
    # Skip our own frame plus whatever the caller asked us to skip.
    set start [expr {1 + $skip_caller_frames}]
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
        if {$entry ne ""} { lappend out $entry }
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
            # Skip 1 caller frame so the deepest frame in the
            # rendered stack is the one that called send_msg_id,
            # not the user proc that called our wrapper.
            set stack [::vw::capture_stack 1]
            set out "${sev_norm}: \[${id}\] ${msg}"
            foreach frame $stack {
                append out "\n  at ${frame}"
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
    # Retry the override install on each eval until it succeeds —
    # the proc bails out cheaply once installed.
    catch {::vw::install_send_msg_override}
    ::vw::dispatch $line
}

close $sock
exit 0
