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
            ::vw::stream_stdout $::vw::current_eval_id $str
            return
        } elseif {$remaining == 2 \
                && [lindex $args $start] eq "stdout"} {
            set str [lindex $args [expr {$start + 1}]]
            if {!$nonewline} { append str "\n" }
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
    ::vw::dispatch $line
}

close $sock
exit 0
