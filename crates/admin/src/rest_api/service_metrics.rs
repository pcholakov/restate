// Copyright (c) 2023 - 2025 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use axum::extract::State;

use crate::state::AdminServiceState;

pub async fn render_service_metrics<V>(State(state): State<AdminServiceState<V>>) -> String {
    if let Some(prometheus) = state.prometheus_handle.as_ref() {
        let metrics_unfiltered = prometheus.render();
        filter_metrics(metrics_unfiltered, &["restate_services_"])
    } else {
        "".to_owned()
    }
}

enum MetricGroup {
    Matching(Vec<String>),
    MaybeMatching(Vec<String>),
    NotMatching,
}

impl MetricGroup {
    fn on_next_line(self, line: &str, is_match: Option<bool>) -> Self {
        match (self, is_match) {
            (Self::Matching(mut items), _) => {
                items.push(line.to_owned());
                Self::Matching(items)
            }
            (Self::MaybeMatching(mut items), is_match) => match is_match {
                Some(true) => {
                    items.push(line.to_owned());
                    Self::Matching(items)
                }
                Some(false) => Self::NotMatching,
                None => {
                    items.push(line.to_owned());
                    Self::MaybeMatching(items)
                }
            },
            (res @ Self::NotMatching, _) => res,
        }
    }

    fn on_group_end(self, buffer: &mut String) -> Self {
        match self {
            Self::Matching(items) => {
                if !buffer.is_empty() {
                    buffer.push('\n');
                }
                for line in items {
                    buffer.push_str(&line);
                    buffer.push('\n');
                }
            }
            Self::MaybeMatching(_) | Self::NotMatching => {}
        };
        Self::MaybeMatching(vec![])
    }
}

fn filter_metrics(input: String, retain_prefixes: &[&str]) -> String {
    let mut result = String::new();
    let mut state = MetricGroup::NotMatching;

    for line in input.lines() {
        if line.starts_with("# TYPE ") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            let in_matching_group = if parts.len() >= 3 {
                let current_metric = parts[2].to_string();
                Some(
                    retain_prefixes
                        .iter()
                        .any(|&prefix| current_metric.starts_with(prefix)),
                )
            } else {
                None
            };
            state = state.on_next_line(line, in_matching_group)
        } else if line.trim().is_empty() {
            state = state.on_group_end(&mut result);
        } else {
            state = state.on_next_line(line, None);
        }
    }

    result
}
