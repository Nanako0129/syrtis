//! Source-generation-aware parity diagnostics for the hourly and Agents
//! report filters.
//!
//! The two reports are deliberately run through one synchronous orchestration
//! seam. Production uses the real local-source token and report functions;
//! tests inject closures so a fixture can mutate a source at an exact stage
//! without sleeping or relying on a racing background writer.

use serde::Serialize;
use serde_json::Value;

use crate::{agents_report, hourly_report, usage_graph, LocalSourceContext};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ProbeStatus {
    #[serde(rename = "match")]
    Match,
    #[serde(rename = "mismatch")]
    Mismatch,
    #[serde(rename = "sourceChanged")]
    SourceChanged,
    #[serde(rename = "tokenUnavailable")]
    TokenUnavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReportAggregate {
    pub entry_count: i64,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
    pub total_tokens: i64,
    pub message_count: i64,
    pub total_cost: f64,
}

impl ReportAggregate {
    fn delta(unfiltered: &Self, full: &Self) -> Self {
        Self {
            entry_count: full.entry_count.saturating_sub(unfiltered.entry_count),
            input: full.input.saturating_sub(unfiltered.input),
            output: full.output.saturating_sub(unfiltered.output),
            cache_read: full.cache_read.saturating_sub(unfiltered.cache_read),
            cache_write: full.cache_write.saturating_sub(unfiltered.cache_write),
            reasoning: full.reasoning.saturating_sub(unfiltered.reasoning),
            total_tokens: full.total_tokens.saturating_sub(unfiltered.total_tokens),
            message_count: full.message_count.saturating_sub(unfiltered.message_count),
            total_cost: full.total_cost - unfiltered.total_cost,
        }
    }

    fn matches(&self, other: &Self) -> bool {
        // Pricing refreshes independently from the local-source generation
        // token. Keep cost in the diagnostic delta, but do not turn a price-only
        // refresh between scans into a filter mismatch.
        self.entry_count == other.entry_count
            && self.input == other.input
            && self.output == other.output
            && self.cache_read == other.cache_read
            && self.cache_write == other.cache_write
            && self.reasoning == other.reasoning
            && self.total_tokens == other.total_tokens
            && self.message_count == other.message_count
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReportParity {
    pub status: ProbeStatus,
    // A report can be intentionally skipped after a known source change or a
    // failed token probe. Keep the keys present as JSON nulls so the envelope
    // remains schema-stable without inventing zero-valued evidence.
    pub unfiltered: Option<ReportAggregate>,
    pub full: Option<ReportAggregate>,
    pub delta: Option<ReportAggregate>,
}

impl ReportParity {
    fn skipped(status: ProbeStatus) -> Self {
        Self {
            status,
            unfiltered: None,
            full: None,
            delta: None,
        }
    }

    fn from_reports(
        status: ProbeStatus,
        unfiltered: ReportAggregate,
        full: ReportAggregate,
    ) -> Self {
        let delta = ReportAggregate::delta(&unfiltered, &full);
        Self {
            status,
            unfiltered: Some(unfiltered),
            full: Some(full),
            delta: Some(delta),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FilterParityPayload {
    pub hourly: ReportParity,
    pub agents: ReportParity,
    pub present_client_count: i64,
}

type TokenFn<'a> = dyn FnMut(&LocalSourceContext) -> Result<u64, String> + 'a;
type GraphFn<'a> = dyn FnMut(&LocalSourceContext) -> Result<Value, String> + 'a;
type ReportFn<'a> = dyn FnMut(&LocalSourceContext, Option<&[String]>) -> Result<Value, String> + 'a;

/// Run the production parity probe using one context and a fresh graph.
pub(crate) fn run(context: &LocalSourceContext) -> Result<FilterParityPayload, String> {
    let mut token = |context: &LocalSourceContext| {
        tokscale_core::local_source_change_token(&context.parse_options(None, None))
    };
    let mut graph = |context: &LocalSourceContext| usage_graph::run(context, "");
    let mut hourly = |context: &LocalSourceContext, clients: Option<&[String]>| {
        hourly_report::run(context, "", clients.map(|values| values.to_vec()))
    };
    let mut agents = |context: &LocalSourceContext, clients: Option<&[String]>| {
        agents_report::run(context, "", clients.map(|values| values.to_vec()))
    };

    run_with(context, &mut token, &mut graph, &mut hourly, &mut agents)
}

/// Testable orchestration seam. The callbacks are synchronous by design: a
/// test can mutate a recognized temporary source immediately before returning
/// from one stage, making each token boundary deterministic.
pub(crate) fn run_with(
    context: &LocalSourceContext,
    token: &mut TokenFn<'_>,
    graph: &mut GraphFn<'_>,
    hourly: &mut ReportFn<'_>,
    agents: &mut ReportFn<'_>,
) -> Result<FilterParityPayload, String> {
    let token0 = token(context).ok();
    // Never use graph_cached here: the present-client list must come from the
    // same fresh graph generation that the first token brackets.
    let graph = graph(context)?;
    let clients = graph_clients(&graph)?;
    let present_client_count = i64::try_from(clients.len())
        .map_err(|_| "present client count exceeds supported range".to_string())?;
    let token1 = token(context).ok();

    if let Some(status) = boundary_status(&[token0, token1]) {
        return Ok(FilterParityPayload {
            hourly: ReportParity::skipped(status),
            agents: ReportParity::skipped(status),
            present_client_count,
        });
    }

    let hourly_unfiltered = parse_hourly(hourly(context, None)?)?;
    let token2 = token(context).ok();
    if let Some(status) = boundary_status(&[token0, token1, token2]) {
        return Ok(FilterParityPayload {
            hourly: ReportParity {
                status,
                unfiltered: Some(hourly_unfiltered),
                full: None,
                delta: None,
            },
            agents: ReportParity::skipped(status),
            present_client_count,
        });
    }

    let hourly_full = parse_hourly(hourly(context, Some(&clients))?)?;
    let token3 = token(context).ok();
    if let Some(status) = boundary_status(&[token0, token1, token2, token3]) {
        let delta = ReportAggregate::delta(&hourly_unfiltered, &hourly_full);
        return Ok(FilterParityPayload {
            hourly: ReportParity {
                status,
                unfiltered: Some(hourly_unfiltered),
                full: Some(hourly_full),
                delta: Some(delta),
            },
            agents: ReportParity::skipped(status),
            present_client_count,
        });
    }

    let hourly_status = if hourly_unfiltered.matches(&hourly_full) {
        ProbeStatus::Match
    } else {
        ProbeStatus::Mismatch
    };

    let agents_unfiltered = parse_agents(agents(context, None)?)?;
    let token4 = token(context).ok();
    if let Some(status) = boundary_status(&[token0, token1, token2, token3, token4]) {
        return Ok(FilterParityPayload {
            hourly: ReportParity::from_reports(hourly_status, hourly_unfiltered, hourly_full),
            agents: ReportParity {
                status,
                unfiltered: Some(agents_unfiltered),
                full: None,
                delta: None,
            },
            present_client_count,
        });
    }

    let agents_full = parse_agents(agents(context, Some(&clients))?)?;
    let token5 = token(context).ok();
    let agents_status = boundary_status(&[token0, token1, token2, token3, token4, token5])
        .unwrap_or_else(|| {
            if agents_unfiltered.matches(&agents_full) {
                ProbeStatus::Match
            } else {
                ProbeStatus::Mismatch
            }
        });

    Ok(FilterParityPayload {
        hourly: ReportParity::from_reports(hourly_status, hourly_unfiltered, hourly_full),
        agents: ReportParity::from_reports(agents_status, agents_unfiltered, agents_full),
        present_client_count,
    })
}

fn boundary_status(tokens: &[Option<u64>]) -> Option<ProbeStatus> {
    // A known adjacent difference is stronger evidence than a later missing
    // token. This keeps a real source mutation classified as sourceChanged
    // even if a subsequent probe also fails.
    if tokens
        .windows(2)
        .any(|window| matches!((window[0], window[1]), (Some(left), Some(right)) if left != right))
    {
        Some(ProbeStatus::SourceChanged)
    } else if tokens.iter().any(Option::is_none) {
        Some(ProbeStatus::TokenUnavailable)
    } else {
        None
    }
}

fn graph_clients(graph: &Value) -> Result<Vec<String>, String> {
    let clients = graph
        .get("summary")
        .and_then(|summary| summary.get("clients"))
        .and_then(Value::as_array)
        .ok_or_else(|| "graph summary clients are missing".to_string())?;
    clients
        .iter()
        .map(|client| {
            client
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "graph summary contains an invalid client".to_string())
        })
        .collect()
}

fn required_i64(value: &Value, field: &str) -> Result<i64, String> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("report field {field} is missing or invalid"))
}

fn required_cost(value: &Value) -> Result<f64, String> {
    let cost = value
        .get("totalCost")
        .and_then(Value::as_f64)
        .ok_or_else(|| "report total cost is missing or invalid".to_string())?;
    if cost.is_finite() {
        Ok(cost)
    } else {
        Err("report total cost is not finite".to_string())
    }
}

fn aggregate_entries(value: &Value, message_field: &str) -> Result<(ReportAggregate, i64), String> {
    let entries = value
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| "report entries are missing or invalid".to_string())?;
    let entry_count = i64::try_from(entries.len())
        .map_err(|_| "report entry count exceeds supported range".to_string())?;
    let mut aggregate = ReportAggregate {
        entry_count,
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        reasoning: 0,
        total_tokens: 0,
        message_count: 0,
        total_cost: required_cost(value)?,
    };

    for entry in entries {
        aggregate.input = aggregate
            .input
            .saturating_add(required_i64(entry, "input")?);
        aggregate.output = aggregate
            .output
            .saturating_add(required_i64(entry, "output")?);
        aggregate.cache_read = aggregate
            .cache_read
            .saturating_add(required_i64(entry, "cacheRead")?);
        aggregate.cache_write = aggregate
            .cache_write
            .saturating_add(required_i64(entry, "cacheWrite")?);
        aggregate.reasoning = aggregate
            .reasoning
            .saturating_add(required_i64(entry, "reasoning")?);
        aggregate.message_count = aggregate
            .message_count
            .saturating_add(required_i64(entry, message_field)?);
    }
    aggregate.total_tokens = aggregate
        .input
        .saturating_add(aggregate.output)
        .saturating_add(aggregate.cache_read)
        .saturating_add(aggregate.cache_write)
        .saturating_add(aggregate.reasoning);

    Ok((aggregate, entry_count))
}

fn parse_hourly(value: Value) -> Result<ReportAggregate, String> {
    Ok(aggregate_entries(&value, "messageCount")?.0)
}

fn parse_agents(value: Value) -> Result<ReportAggregate, String> {
    let (mut aggregate, _) = aggregate_entries(&value, "messages")?;
    // The report's top-level message total is the authoritative aggregate used
    // by the existing Swift DTO. Requiring it also catches a mapper that drops
    // an entry's message count while still serializing valid rows.
    aggregate.message_count = required_i64(&value, "totalMessages")?;
    Ok(aggregate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::PathBuf;
    use std::rc::Rc;

    fn context() -> LocalSourceContext {
        LocalSourceContext {
            home_dir: Some(PathBuf::from("fixture-home")),
            use_env_roots: true,
        }
    }

    fn graph() -> Value {
        serde_json::json!({"summary": {"clients": ["claude", "synthetic"]}})
    }

    fn hourly(input: i64, messages: i64, cost: f64) -> Value {
        serde_json::json!({
            "entries": [{
                "input": input, "output": 20, "cacheRead": 3,
                "cacheWrite": 4, "reasoning": 5, "messageCount": messages
            }],
            "totalCost": cost
        })
    }

    fn agents(input: i64, messages: i64, cost: f64) -> Value {
        serde_json::json!({
            "entries": [{
                "input": input, "output": 20, "cacheRead": 3,
                "cacheWrite": 4, "reasoning": 5, "messages": messages
            }],
            "totalCost": cost, "totalMessages": messages
        })
    }

    fn run_fixture(
        tokens: Vec<Result<u64, String>>,
        hourly_values: Vec<Result<Value, String>>,
        agent_values: Vec<Result<Value, String>>,
    ) -> FilterParityPayload {
        let mut tokens = tokens.into_iter();
        let mut hourly_values = hourly_values.into_iter();
        let mut agent_values = agent_values.into_iter();
        let mut token = |_context: &LocalSourceContext| tokens.next().unwrap();
        let mut graph_fn = |_context: &LocalSourceContext| Ok(graph());
        let mut hourly_fn = |_context: &LocalSourceContext, _clients: Option<&[String]>| {
            hourly_values.next().unwrap()
        };
        let mut agents_fn = |_context: &LocalSourceContext, _clients: Option<&[String]>| {
            agent_values.next().unwrap()
        };
        run_with(
            &context(),
            &mut token,
            &mut graph_fn,
            &mut hourly_fn,
            &mut agents_fn,
        )
        .unwrap()
    }

    #[test]
    fn stable_equal_reports_match_and_include_delta() {
        let result = run_fixture(
            vec![Ok(1); 6],
            vec![Ok(hourly(10, 2, 0.25)), Ok(hourly(10, 2, 0.25))],
            vec![Ok(agents(10, 2, 0.25)), Ok(agents(10, 2, 0.25))],
        );
        assert_eq!(result.hourly.status, ProbeStatus::Match);
        assert_eq!(result.agents.status, ProbeStatus::Match);
        assert_eq!(result.hourly.delta.unwrap().total_tokens, 0);
        assert_eq!(result.agents.delta.unwrap().message_count, 0);
    }

    #[test]
    fn stable_different_reports_are_mismatch() {
        let result = run_fixture(
            vec![Ok(1); 6],
            vec![Ok(hourly(10, 2, 0.25)), Ok(hourly(11, 2, 0.25))],
            vec![Ok(agents(10, 2, 0.25)), Ok(agents(10, 2, 0.25))],
        );
        assert_eq!(result.hourly.status, ProbeStatus::Mismatch);
        assert_eq!(result.agents.status, ProbeStatus::Match);
        assert_eq!(result.hourly.delta.unwrap().input, 1);
    }

    #[test]
    fn stable_source_ignores_independent_pricing_refresh() {
        let result = run_fixture(
            vec![Ok(1); 6],
            vec![Ok(hourly(10, 2, 0.25)), Ok(hourly(10, 2, 0.75))],
            vec![Ok(agents(10, 2, 0.25)), Ok(agents(10, 2, 1.25))],
        );
        assert_eq!(result.hourly.status, ProbeStatus::Match);
        assert_eq!(result.agents.status, ProbeStatus::Match);
        assert_eq!(result.hourly.delta.unwrap().total_cost, 0.5);
        assert_eq!(result.agents.delta.unwrap().total_cost, 1.0);
    }

    #[test]
    fn every_relevant_boundary_is_source_changed_and_later_scans_short_circuit() {
        for changed_at in 1..=5 {
            let mut tokens: Vec<Result<u64, String>> = vec![Ok(1); 6];
            // A single adjacent boundary differs while the remaining values
            // stay equal, making the expected source generation unambiguous.
            tokens[changed_at] = Ok(100 + changed_at as u64);
            let calls = Rc::new(RefCell::new(Vec::new()));
            let token_calls = Rc::clone(&calls);
            let token_index = Rc::new(RefCell::new(0usize));
            let token_counter = Rc::clone(&token_index);
            let mut token = {
                let mut values = tokens.into_iter();
                move |_context: &LocalSourceContext| {
                    let index = *token_counter.borrow();
                    *token_counter.borrow_mut() += 1;
                    token_calls.borrow_mut().push(format!("token{index}"));
                    values.next().unwrap()
                }
            };
            let graph_calls = Rc::clone(&calls);
            let mut graph_fn = |_context: &LocalSourceContext| {
                graph_calls.borrow_mut().push("graph".to_string());
                Ok(graph())
            };
            let hourly_calls = Rc::clone(&calls);
            let mut hourly_fn = |_context: &LocalSourceContext, clients: Option<&[String]>| {
                hourly_calls.borrow_mut().push(if clients.is_some() {
                    "hourly-full".to_string()
                } else {
                    "hourly-nil".to_string()
                });
                Ok(hourly(10, 2, 0.25))
            };
            let agents_calls = Rc::clone(&calls);
            let mut agents_fn = |_context: &LocalSourceContext, clients: Option<&[String]>| {
                agents_calls.borrow_mut().push(if clients.is_some() {
                    "agents-full".to_string()
                } else {
                    "agents-nil".to_string()
                });
                Ok(agents(10, 2, 0.25))
            };
            let result = run_with(
                &context(),
                &mut token,
                &mut graph_fn,
                &mut hourly_fn,
                &mut agents_fn,
            )
            .unwrap();
            assert_eq!(
                result.hourly.status,
                if changed_at <= 3 {
                    ProbeStatus::SourceChanged
                } else {
                    ProbeStatus::Match
                },
                "boundary {changed_at}"
            );
            assert_eq!(
                result.agents.status,
                ProbeStatus::SourceChanged,
                "boundary {changed_at}"
            );
            // Every stage is bracketed by one token read, so a change seen at
            // boundary `changed_at` ends the log at `token{changed_at}`.
            let stages = [
                "graph",
                "hourly-nil",
                "hourly-full",
                "agents-nil",
                "agents-full",
            ];
            let mut expected = vec!["token0".to_string()];
            for (index, stage) in stages.iter().take(changed_at).enumerate() {
                expected.push(stage.to_string());
                expected.push(format!("token{}", index + 1));
            }
            assert_eq!(
                *calls.borrow(),
                expected,
                "stage call log for boundary {changed_at}"
            );
        }
    }

    #[test]
    fn known_change_wins_over_later_missing_token() {
        assert_eq!(
            boundary_status(&[Some(1), Some(2), None]),
            Some(ProbeStatus::SourceChanged)
        );
        assert_eq!(
            boundary_status(&[Some(1), None, Some(2)]),
            Some(ProbeStatus::TokenUnavailable)
        );
    }

    #[test]
    fn token_failure_is_token_unavailable_not_outer_failure() {
        let result = run_fixture(vec![Err("probe failed".to_string()); 6], vec![], vec![]);
        assert_eq!(result.hourly.status, ProbeStatus::TokenUnavailable);
        assert_eq!(result.agents.status, ProbeStatus::TokenUnavailable);
    }

    #[test]
    fn graph_failure_remains_an_outer_error() {
        let mut token = |_context: &LocalSourceContext| Ok(1);
        let mut graph_fn = |_context: &LocalSourceContext| Err("graph failed".to_string());
        let mut hourly_fn =
            |_context: &LocalSourceContext, _clients: Option<&[String]>| Ok(hourly(1, 1, 0.0));
        let mut agents_fn =
            |_context: &LocalSourceContext, _clients: Option<&[String]>| Ok(agents(1, 1, 0.0));
        let result = run_with(
            &context(),
            &mut token,
            &mut graph_fn,
            &mut hourly_fn,
            &mut agents_fn,
        );
        assert!(result.is_err());
    }

    #[test]
    fn controlled_append_between_stages_is_source_changed_and_skips_later_scans() {
        let root = std::env::temp_dir().join(format!(
            "tokenbar-filter-parity-stage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = root.join(".codex/sessions/session.jsonl");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"seed\n").unwrap();
        let options = tokscale_core::LocalParseOptions {
            home_dir: Some(root.to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(vec!["codex".to_string()]),
            ..Default::default()
        };
        let mut token =
            |_context: &LocalSourceContext| tokscale_core::local_source_change_token(&options);
        let calls = Rc::new(RefCell::new(Vec::new()));
        let graph_calls = Rc::clone(&calls);
        let mut graph_fn = |_context: &LocalSourceContext| {
            graph_calls.borrow_mut().push("graph".to_string());
            Ok(graph())
        };
        let hourly_calls = Rc::clone(&calls);
        let mut hourly_fn = |_context: &LocalSourceContext, clients: Option<&[String]>| {
            hourly_calls.borrow_mut().push(if clients.is_some() {
                "hourly-full".to_string()
            } else {
                "hourly-nil".to_string()
            });
            if clients.is_none() {
                let mut file = OpenOptions::new().append(true).open(&source).unwrap();
                file.write_all(b"size-changing append\n").unwrap();
                file.flush().unwrap();
            }
            Ok(hourly(10, 2, 0.25))
        };
        let agents_calls = Rc::clone(&calls);
        let mut agents_fn = |_context: &LocalSourceContext, _clients: Option<&[String]>| {
            agents_calls.borrow_mut().push("agents".to_string());
            Ok(agents(10, 2, 0.25))
        };

        let result = run_with(
            &LocalSourceContext {
                home_dir: Some(root.clone()),
                use_env_roots: true,
            },
            &mut token,
            &mut graph_fn,
            &mut hourly_fn,
            &mut agents_fn,
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(root);

        assert_eq!(result.hourly.status, ProbeStatus::SourceChanged);
        assert_eq!(result.agents.status, ProbeStatus::SourceChanged);
        assert_eq!(
            calls
                .borrow()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["graph", "hourly-nil"]
        );
    }
}
