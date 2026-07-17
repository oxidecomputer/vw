# `launch_runs` — a possible future

vw currently drives Vivado through direct commands: `synth_ip`,
`synth_design`, `place_design`, `route_design`, etc. These are the
non-project-mode idiom, and vw calls them against an on-disk
project (`create_project -dir …`). That hybrid works, but Vivado
periodically complains — `[Vivado 12-5447] synth_ip is not
supported in project mode`, `[Project 1-5563] BD generation state
is stale`, `[Vivado 12-13650] IP file has been moved from its
original location`. Each was a rabbit hole; each ended with a
targeted `set_msg_config -suppress` in `~/src/htcl/vw/module.htcl`.

The "correct" project-mode alternative is Vivado's **runs**
infrastructure: named jobs like `synth_1`, `impl_1`, and one per-IP
OOC synth run (`<ip>_synth_1`), driven by `launch_runs
<name>` + `wait_on_run <name>`. Vivado tracks status/progress in
the `.xpr` and materializes DCPs into standard project subdirs.
Instead of us calling `synth_ip` / `synth_design` /
`place_design` / `route_design` directly, we'd do something like:

```tcl
create_ip_run [get_files primary_clock.xci]   ;# if not auto-created
launch_runs {primary_clock_synth_1 …} -jobs 4
wait_on_run primary_clock_synth_1
launch_runs synth_1                            ;# top synth uses cached IP DCPs
wait_on_run synth_1
launch_runs impl_1 -to_step write_bitstream
wait_on_run impl_1
```

Notes about state of confidence: I've read Vivado's documented
runs flow but haven't personally driven a large project through
it end-to-end. Estimates below are honest guesses; some things
may prove easier or harder than described.

## In favor

- **Ends the fight with Vivado.** Every warning we've been
  fighting comes from using non-project commands in a project.
  `launch_runs` is what project mode expects. The whole class
  should go away — clean baseline, no more `-suppress` list to
  maintain.
- **Parallel IP synth for free.** `launch_runs -jobs N` runs
  multiple IPs concurrently. Right now `synth_ip` is serial per
  IP. Meaningful wall-clock savings on designs with many
  top-level IPs.
- **Vivado handles IP-DCP freshness.** Runs track IP source
  changes and re-run only what's needed. Our custom
  fingerprint/manifest machinery for the IP part
  (`synth_needs_update`, etc.) largely becomes redundant —
  Vivado's own run status is authoritative.
- **`get_msg_config -count` may start working.** The per-process
  CW-count bug (which drove us to build our own Rust-side
  counter with a `critical_warning_count` RPC) exists because
  `place_design`'s sub-processes aren't tracked. `launch_runs`
  uses managed run processes, which the counter is documented
  to handle correctly.
- **Compatibility with GUI Vivado.** Anyone opening the `.xpr`
  in the IDE sees a "normal" project with proper runs, not a
  hybrid state driven from Tcl.

## Against

- **Async by default.** `launch_runs` returns immediately; the
  run happens in the background. `wait_on_run` blocks until
  completion. Different mental model from `synth_design`
  (blocking, in-process). Not fatal, but error handling
  changes — a run can be in states like `Running`, `Complete!`,
  `Failed`, etc., and we'd need to query.
- **Sub-processes = separate log streams.** Each run spawns a
  child Vivado process. Its stdout/stderr goes into
  `<project>/<name>.runs/<run>/runme.log`, not our PTY stream.
  We'd lose the real-time output the user sees now — or we'd
  need to tail those log files back into our stream. A real UX
  regression to solve.
- **Losing our checkpoint conventions.** Today we produce
  `target/synth/<top>.dcp`, `target/place/<top>.dcp`,
  `target/route/<top>.dcp` at predictable workspace-relative
  paths. Runs put DCPs at `<project>/<name>.runs/<run>/<top>.dcp`
  — Vivado-managed. We'd either symlink or change our conventions.
- **Our per-phase caching becomes tangled.**
  `synth_needs_update` / `place_needs_update` /
  `route_needs_update` — fingerprinted source tracking with
  sidecar `.manifest` files — was designed around us owning DCP
  paths and lifecycle. With runs, Vivado owns them and has its
  own freshness logic. We'd either delete our machinery (defer
  to Vivado's, which we don't currently trust as authoritative
  for our source-tree changes — the `synth` fingerprint covers
  htcl / vw.toml files Vivado's runs don't know about) or bridge
  them (complicated).
- **CW-gated checkpoint writes get harder.** Our current "skip
  DCP write if the phase emitted CWs" logic wraps a synchronous
  `place_design`. With `launch_runs`, the CWs happen in a
  sub-process; we'd read them after the fact from the run log.
  Doable but different plumbing.
- **`vw::configure_ip` gets restructured.** Currently we run
  user `ip::configure` code that calls `create_ip`,
  `create_bd_design`, etc. — those all still work under runs,
  but the "synthesize them" step moves from `synth_ip` (which
  we call directly) to `launch_runs`. Reasonable, but touches
  more code and re-plumbs the CW-gate.
- **`vw test` isolation.** Today `vw test` uses `-in_memory`
  and never touches runs. Would need to keep that path working,
  so `vw::synth` (and friends) would have TWO paths: in-memory
  (current code) and on-disk (launch_runs). Complexity tax.
- **Real effort.** Rough estimate — at least a day of focused
  work, maybe two, including debugging Vivado's async behavior
  when things go wrong. The wall-clock cost of "just suppress
  the warning" is 30 seconds.

## When it's worth doing

Suppression is 30 seconds and buys the same end user
experience — clean logs, working flow. `launch_runs` is
architecturally correct and would end the entire class of
project-mode/non-project-command friction, but it's real
engineering and forces us to solve the sub-process-log-streaming
and DCP-path-convention problems.

Prefer `launch_runs` if any of:

- You anticipate more warnings of this shape and want to close
  the door on them permanently.
- Parallel IP synth would meaningfully speed up iteration.
- You want vw's `.xpr` to be openable in the Vivado GUI as a
  "normal" project.

Otherwise, keep the current flow and add targeted
`set_msg_config -suppress` calls as new warnings surface.

## What would move if we did this

Concrete files/procs to expect touching, so scope is calibratable:

- `~/src/htcl/vw/module.htcl` — `vw::configure_ip`,
  `vw::synth`, `vw::place`, `vw::route` would be replaced or
  substantially rewritten. New helpers like `vw::_wait_run`
  and a per-run CW-log-scraper.
- `~/src/htcl/amd/vivado-cmd/cmd/` — the missing
  `create_ip_run.htcl`, `launch_runs.htcl`, `wait_on_run.htcl`,
  `get_runs.htcl` wrappers may need to be authored or
  regenerated from Vivado man pages.
- `vw-lib/src/lib.rs` — the `synth_source_fingerprint` /
  `place_source_fingerprint` / `route_source_fingerprint`
  helpers and their manifest sidecars are either deleted or
  their scope narrows to "source-tree freshness" (delegating
  DCP freshness to Vivado's runs). Removes ~200 lines but
  requires re-thinking cross-stage invalidation.
- `vw-vivado/src/handlers.rs` — matching RPCs
  (`{synth,place,route}_needs_update`,
  `{synth,place,route}_mark_checkpoint`) either shrink or go.
- `vw-vivado/src/worker.rs` — need a way to tail
  `<project>/<name>.runs/*/runme.log` files during
  `wait_on_run` so the user sees Vivado's progress instead of a
  silent block.
- Path conventions — `target/{synth,place,route}/<top>.dcp` are
  hardcoded in vw::synth/place/route. Either symlink from those
  paths to the runs-generated DCPs, or update every caller.

That last point (path conventions) is where documentation
outside the code lives — `docs/whypoints.md`,
`docs/new-structure.md` — and would need a pass.

## Current suppressions (for reference when evaluating)

Every `set_msg_config -suppress` we've added because of the
project-mode/non-project-command hybrid, so the value proposition
of removing them is concrete:

- `[Vivado 12-5447] synth_ip is not supported in project mode` —
  in `vw::synth`, before `synth_ip` on top-level standalone IPs.
- `[Project 1-5563] File '<bd>.bd' generation state is stale` —
  in `vw::configure_ip`, before flipping
  `synth_checkpoint_mode None` on BDs.
- `[Vivado 12-13650] IP file has been moved from its original
  location` — the one that prompted this doc. Not yet
  suppressed; deferred pending this decision.

If we go the `launch_runs` route, all three should become
unnecessary.
