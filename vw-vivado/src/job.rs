// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! A Vivado worker doing one job for the session that started it.
//!
//! Build steps that need a Vivado of their own — an IP module's synthesis,
//! the user logic's, the link — each get a fresh worker in a working
//! directory of their own. The worker evaluates a fixed list of Tcl steps
//! and exits; its whole transcript is kept in that directory, and what the
//! session sees of it arrives through the calling worker's stream sink (see
//! [`RpcContext`]), as though the calling worker had said it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use vw_eda::{EdaBackend, StreamKind};

use crate::rpc::RpcContext;
use crate::worker::{register_helper, VivadoBackend, VivadoConfig};

/// How much of a job's output reaches the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Report {
    /// Warnings and worse. For jobs that run side by side, whose progress
    /// chatter interleaved would be noise.
    Problems,
    /// Everything, as though the session were running it itself.
    All,
}

/// One job for one worker.
pub(crate) struct Job {
    /// What the job is, for the session's messages: "IP synthesis of cips".
    pub label: String,
    /// The Vivado to run.
    pub vivado: PathBuf,
    /// The job's own directory: the worker's cwd, and where its log goes.
    /// Emptied before the job starts.
    pub dir: PathBuf,
    /// Steps whose output stays in the job's log: setting up, before the
    /// job's work proper begins.
    pub setup: Vec<String>,
    /// The job's work.
    pub steps: Vec<String>,
    pub report: Report,
}

impl Job {
    /// The worker's complete transcript.
    pub fn log(&self) -> PathBuf {
        self.dir.join("vivado.log")
    }

    /// Run every step, stopping at the first that fails. Answers how long
    /// it took.
    pub async fn run(&self, ctx: &RpcContext) -> Result<f64, String> {
        let started = Instant::now();
        let _ = std::fs::remove_dir_all(&self.dir);
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| format!("creating {}: {e}", self.dir.display()))?;

        let mut backend = VivadoBackend::spawn(VivadoConfig {
            vivado: Some(self.vivado.clone()),
            working_dir: Some(self.dir.clone()),
            raw_log: Some(self.log()),
            ..Default::default()
        })
        .await
        .map_err(|e| format!("starting Vivado: {e}"))?;
        // Interrupting the session's worker interrupts this one too.
        let _helper = match (ctx.caller_pid, backend.child_pid()) {
            (Some(caller), Some(helper)) => {
                Some(register_helper(caller, helper))
            }
            _ => None,
        };

        let forwarding = Arc::new(AtomicBool::new(false));
        let forward = forwarding.clone();
        let output = ctx.output.clone();
        let report = self.report;
        // Problems say where they came from, since the session can't tell
        // which of its workers said them otherwise.
        let tag = format!("  at {} (log {})", self.label, self.log().display());
        backend.set_stdout_sink(Box::new(move |kind, chunk: &str| {
            if !forward.load(Ordering::Relaxed) {
                return;
            }
            let problem = matches!(
                kind,
                StreamKind::Warning
                    | StreamKind::CriticalWarning
                    | StreamKind::Error
            );
            let text = if problem {
                format!("{}\n{tag}\n", chunk.trim_end_matches('\n'))
            } else if report == Report::All {
                chunk.to_string()
            } else {
                return;
            };
            let _ = output.send((kind, text));
        }));

        for tcl in &self.setup {
            backend.eval(tcl).await.map_err(|e| format!("{e}"))?;
        }
        forwarding.store(true, Ordering::Relaxed);
        for tcl in &self.steps {
            backend.eval(tcl).await.map_err(|e| format!("{e}"))?;
        }
        let _ = backend.shutdown().await;
        Ok(started.elapsed().as_secs_f64())
    }
}

/// `path` as one Tcl word.
pub(crate) fn tcl_word(path: &Path) -> String {
    format!("{{{}}}", path.display())
}

/// `paths` as a Tcl list.
pub(crate) fn tcl_list<P: AsRef<Path>>(paths: &[P]) -> String {
    paths
        .iter()
        .map(|p| tcl_word(p.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}
