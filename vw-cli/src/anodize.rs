// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `vw cosim anodize` — running the anodizer on purpose.
//!
//! Anodization is normally invisible: `vw bench run` regenerates the Rust
//! structs for `serialize_rust`-tagged VHDL records when the design has
//! changed, says nothing about it, and gets on with the benches. That is
//! right for a build and no use at all for working on the anodizer, where
//! what is wanted is to watch it run and see what it produced.
//!
//! So this runs the same generator with its cache turned off, reports what it
//! found, and then compiles a bench against the result — because anodizer
//! emitting Rust that does not compile is the failure that would otherwise
//! turn up much later, as a bench that will not build with an error pointing
//! into a file nobody wrote.
//!
//! The two halves of this file are the two places it can run. What they have
//! in common is everything that decides what happens and how it looks: the
//! work is `vw_remote::anodize::run` in both cases, and the display below is
//! driven by the same events whichever machine produced them.

use camino::Utf8Path;
use colored::*;
use futures::StreamExt;

use crate::cloud::{CloudError, Session};
use vw_remote::AnodizeEvent;

/// What can stop an anodization before it reaches a verdict.
///
/// Separate from [`CloudError`] because half of these happen with no cloud
/// involved, and "talking to the service" is a confusing thing to be told
/// about a `--local` run that crashed on this machine.
#[derive(Debug, thiserror::Error)]
pub enum AnodizeError {
    #[error(transparent)]
    Cloud(#[from] CloudError),
    #[error(
        "the anodizer stopped without saying whether it worked — it panicked, \
         and the message above it is from vw rather than from your design"
    )]
    Panicked,
    #[error("the instance ended the run without reporting whether it worked")]
    Abandoned,
}

/// Anodize on an environment's vivado instance.
///
/// Returns whether it worked, so the caller can decide the exit code; an
/// anodization that failed is not a vw failure, it is the answer.
pub async fn run(
    session: &Session,
    environment: &str,
    bench: Option<&str>,
    standard: vw_lib::VhdlStandard,
) -> Result<bool, AnodizeError> {
    let standard = standard.to_string();
    let upgraded = vw_api_client::retrying(|| {
        session
            .client
            .anodize(environment, bench, Some(standard.as_str()))
    })
    .await
    .map_err(|e| session.error(e))?
    .into_inner();

    let mut socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
        upgraded,
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    )
    .await;

    while let Some(message) = socket.next().await {
        let text =
            match message.map_err(|e| CloudError::Transport(e.to_string()))? {
                tokio_tungstenite::tungstenite::Message::Text(text) => text,
                tokio_tungstenite::tungstenite::Message::Close(_) => break,
                _ => continue,
            };

        let event = serde_json::from_str::<AnodizeEvent>(&text)
            .map_err(|e| CloudError::Transport(e.to_string()))?;
        if let Some(success) = show(event) {
            return Ok(success);
        }
    }

    Err(AnodizeError::Abandoned)
}

/// Anodize on this machine.
///
/// What `--local` does everywhere: the same work in the same order, for
/// somebody with nvc and a rust toolchain to hand who should not be forced
/// through a network to use them.
pub async fn run_locally(
    workspace: &Utf8Path,
    bench: Option<&str>,
    standard: vw_lib::VhdlStandard,
) -> Result<bool, AnodizeError> {
    let request = vw_remote::AnodizeRequest {
        bench: bench.map(str::to_owned),
        standard: standard.to_string(),
    };

    // The same channel the socket would carry, ending at this terminal
    // instead. Running the work in a task rather than inline so its events
    // are shown as they happen — a cargo build that printed nothing until it
    // finished would be the one thing `--local` must not do differently.
    let (events, mut incoming) = tokio::sync::mpsc::unbounded_channel();
    let working = tokio::spawn({
        let workspace = workspace.to_owned();
        async move {
            vw_remote::anodize::run(&workspace, &request, events).await;
        }
    });

    let mut outcome = None;
    while let Some(event) = incoming.recv().await {
        if let Some(success) = show(event) {
            outcome = Some(success);
        }
    }
    // A verdict is the normal way out. Not getting one means the worker died,
    // which for this command means a panic somewhere in the type walk — and
    // saying so is the difference between a vw bug and a design problem.
    let panicked = working.await.is_err();

    outcome.ok_or(if panicked {
        AnodizeError::Panicked
    } else {
        AnodizeError::Abandoned
    })
}

/// Show one event. Returns the verdict once there is one.
///
/// A failure is a verdict rather than an error: the anodizer not working is
/// the answer this command was run to get, and dressing it up as something
/// having gone wrong with vw would bury the message that says what.
fn show(event: AnodizeEvent) -> Option<bool> {
    match event {
        AnodizeEvent::Anodizing { sources, tagged } => {
            println!(
                "{:>12} {tagged} tagged of {sources} design source{}",
                "Anodizing".bright_green().bold(),
                if sources == 1 { "" } else { "s" },
            );
        }
        AnodizeEvent::Generated { path, lines } => {
            println!(
                "{:>12} {path} ({lines} line{})",
                "Generated".bright_green().bold(),
                if lines == 1 { "" } else { "s" },
            );
        }
        // Not a failure, and worth saying plainly: a developer who has just
        // written `attribute serialize_rust` and sees this has learnt that
        // the file they edited is not in the design set, which is the thing
        // they need to know.
        AnodizeEvent::NothingTagged { sources } => {
            println!(
                "{:>12} no `serialize_rust` attribute in {sources} design \
                 source{} — nothing to generate",
                "Skipped".yellow().bold(),
                if sources == 1 { "" } else { "s" },
            );
        }
        AnodizeEvent::Building { bench } => {
            println!("{:>12} {bench}", "Building".bright_green().bold());
        }
        AnodizeEvent::NotExercised { bench } => {
            println!(
                "{:>12} {bench} builds, but nothing in it names \
                 `generated_structs` — the generated Rust was not compiled",
                "Note".yellow().bold(),
            );
        }
        // Printed as cargo wrote it: cargo already says these things better
        // than anything here could.
        AnodizeEvent::Line { text } => println!("{text}"),
        AnodizeEvent::Done { success } => {
            if success {
                println!("{:>12}", "Done".bright_green().bold());
            } else {
                // Deliberately not "the anodizer emitted bad Rust": the
                // build failing says the bench did not compile against what
                // was generated, and cargo above has already said why —
                // which is as often a missing dependency or the wrong
                // toolchain as it is the generator.
                eprintln!(
                    "{} the bench did not build against the generated structs",
                    "error:".bright_red(),
                );
            }
            return Some(success);
        }
        AnodizeEvent::Fatal { message } => {
            eprintln!("{} {message}", "error:".bright_red());
            return Some(false);
        }
    }
    None
}
