// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! A small, human-friendly summary of an IP-XACT component — used by
//! `vw ip generate` to print what it's processing before emitting code.

use ipxact::Component;

#[derive(Clone, Debug)]
pub struct Summary {
    pub vlnv: String,
    pub description: Option<String>,
    pub parameter_count: usize,
    pub user_parameter_count: usize,
    pub model_parameter_count: usize,
    pub port_count: usize,
    pub choice_count: usize,
}

impl Summary {
    pub fn of(c: &Component) -> Self {
        let parameters: Vec<_> = c.component_parameters().collect();
        let user_parameter_count = parameters
            .iter()
            .filter(|p| p.value.is_user_configurable())
            .count();
        Self {
            vlnv: c.vlnv(),
            description: c.description.clone(),
            parameter_count: parameters.len(),
            user_parameter_count,
            model_parameter_count: c.model_parameters().count(),
            port_count: c.ports().count(),
            choice_count: c.choices().count(),
        }
    }
}
