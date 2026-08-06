// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Manages the checked-in OpenAPI documents for the vw service APIs.
//!
//! Run it through the workspace alias:
//!
//! ```text
//! cargo xtask openapi list        # what documents are managed
//! cargo xtask openapi generate    # write them from the API traits
//! cargo xtask openapi check       # fail if what is on disk is out of date
//! ```

use std::process::ExitCode;

use clap::Parser;
use dropshot_api_manager::App;

fn main() -> anyhow::Result<ExitCode> {
    let app = App::parse();
    Ok(app.exec(
        &vw_openapi_manager::environment()?,
        &vw_openapi_manager::all_apis()?,
    ))
}
