//! Version `ANODIZE` of the vw APIs.
//!
//! Adds running the anodizer on purpose, rather than only as a step of a
//! build. No type from a prior version changed, so there is nothing to
//! convert.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What to anodize, and what to check the result against.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct AnodizeQuery {
    /// A bench crate to compile against what was generated.
    ///
    /// Absent runs the generator and stops there. Anodizer failing is easy to
    /// see; anodizer succeeding and emitting Rust that does not compile is
    /// not, and compiling something against it is what turns that into an
    /// answer.
    #[serde(default)]
    pub bench: Option<String>,
    /// The VHDL standard, as `nvc` spells it. Absent lets the instance use
    /// its own default.
    #[serde(default)]
    pub standard: Option<String>,
}
