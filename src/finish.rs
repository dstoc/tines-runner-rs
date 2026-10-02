//! Finish reports derived from the structured Codex stream.

use std::collections::BTreeSet;

use serde_json::{Number, Value};

use crate::codex_stream::CodexEvent;
use crate::protocol::{
    CodexMeasurementStatus, CodexPricingEvidenceV1, CodexRawUsageV1, FinishRunRequest,
    FinishStatus, RunUsage,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_PROVIDER_SESSION_ID_LEN: usize = 255;
const MAX_MODEL_LEN: usize = 255;
const MAX_VERSION_LEN: usize = 100;

/// Collects session and cumulative usage data for one Codex invocation.
///
/// The collector keeps the latest valid terminal usage snapshot. A malformed
/// later snapshot marks pricing evidence invalid but does not erase usage that
/// the stream already supplied.
#[derive(Debug)]
pub struct CodexRunReport {
    model: Option<String>,
    daemon_version: Option<String>,
    thread_ids: BTreeSet<String>,
    raw_usage: Option<CodexRawUsageV1>,
    previous_usage: Option<CodexRawUsageV1>,
    terminal_snapshots: u64,
    invalid_usage: bool,
    nonmonotonic_usage: bool,
}

impl CodexRunReport {
    /// Begin a cold Codex invocation report.
    pub fn new(model: Option<&str>) -> Self {
        Self {
            model: model
                .map(str::to_owned)
                .filter(|value| valid_short(value, MAX_MODEL_LEN)),
            daemon_version: Some(crate::VERSION.to_owned())
                .filter(|value| valid_short(value, MAX_VERSION_LEN)),
            thread_ids: BTreeSet::new(),
            raw_usage: None,
            previous_usage: None,
            terminal_snapshots: 0,
            invalid_usage: false,
            nonmonotonic_usage: false,
        }
    }

    /// Keep a provider thread ID or terminal usage snapshot from an event.
    pub fn observe(&mut self, event: &CodexEvent) {
        if let Some(thread_id) = event
            .thread_id()
            .filter(|value| valid_short(value, MAX_PROVIDER_SESSION_ID_LEN))
        {
            self.thread_ids.insert(thread_id.to_owned());
        }

        if event.event_type() != Some("turn.completed") {
            return;
        }
        self.terminal_snapshots = self.terminal_snapshots.saturating_add(1);
        let Some(raw) = event.raw_usage() else {
            return;
        };
        let Some(object) = raw.as_object() else {
            self.invalid_usage = true;
            return;
        };

        let mut invalid = false;
        let snapshot = CodexRawUsageV1 {
            input_tokens: read_metric(object.get("input_tokens"), &mut invalid),
            cached_input_tokens: read_metric(object.get("cached_input_tokens"), &mut invalid),
            cache_write_input_tokens: read_metric(
                object.get("cache_write_input_tokens"),
                &mut invalid,
            ),
            output_tokens: read_metric(object.get("output_tokens"), &mut invalid),
        };
        if let (Some(total), Some(read), Some(write)) = (
            snapshot.input_tokens,
            snapshot.cached_input_tokens,
            snapshot.cache_write_input_tokens,
        ) && read.checked_add(write).is_none_or(|cached| cached > total)
        {
            invalid = true;
        }
        self.invalid_usage |= invalid;

        if has_usage(&snapshot) {
            if self
                .previous_usage
                .as_ref()
                .is_some_and(|previous| usage_decreased(previous, &snapshot))
            {
                self.nonmonotonic_usage = true;
            }
            self.previous_usage = Some(snapshot.clone());
            self.raw_usage = Some(snapshot);
        }
    }

    /// Build the request sent to `POST /runs/:id/finish` after the process ends.
    pub fn into_finish_request(
        self,
        status: FinishStatus,
        error: Option<String>,
    ) -> FinishRunRequest {
        let measurement_status = self.measurement_status(status);
        let usage = self.raw_usage.as_ref().and_then(normalize_usage);
        let provider_session_id = (self.thread_ids.len() == 1)
            .then(|| self.thread_ids.into_iter().next())
            .flatten();
        let pricing_evidence = CodexPricingEvidenceV1 {
            version: 1,
            harness: "codex".to_owned(),
            model: self.model,
            identity_source: "launch_argument".to_owned(),
            usage_scope: "thread_total".to_owned(),
            session_mode: "cold".to_owned(),
            normalization: "codex-jsonl-v1".to_owned(),
            raw_usage: self.raw_usage,
            model_rerouted: false,
            measurement_status,
            terminal_snapshots: self.terminal_snapshots,
            daemon_version: self.daemon_version,
        };

        FinishRunRequest {
            status,
            error: error.map(|value| truncate_utf16(&value, 10_000)),
            provider_session_id,
            usage,
            pricing_evidence: Some(pricing_evidence),
        }
    }

    fn measurement_status(&self, status: FinishStatus) -> CodexMeasurementStatus {
        if self.thread_ids.len() > 1 {
            CodexMeasurementStatus::MultipleThreads
        } else if self.nonmonotonic_usage {
            CodexMeasurementStatus::Nonmonotonic
        } else if self.invalid_usage {
            CodexMeasurementStatus::Invalid
        } else if status == FinishStatus::Failed
            && (self.raw_usage.is_some() || self.terminal_snapshots > 0)
        {
            CodexMeasurementStatus::IncompleteAttempt
        } else if self
            .raw_usage
            .as_ref()
            .is_some_and(CodexRawUsageV1::is_complete)
        {
            CodexMeasurementStatus::Complete
        } else {
            CodexMeasurementStatus::Missing
        }
    }
}

impl CodexRawUsageV1 {
    fn is_complete(&self) -> bool {
        self.input_tokens.is_some()
            && self.cached_input_tokens.is_some()
            && self.cache_write_input_tokens.is_some()
            && self.output_tokens.is_some()
    }
}

fn read_metric(value: Option<&Value>, invalid: &mut bool) -> Option<u64> {
    let value = value?;
    let Some(number) = value.as_number().and_then(Number::as_u64) else {
        *invalid = true;
        return None;
    };
    if number > MAX_SAFE_INTEGER {
        *invalid = true;
        None
    } else {
        Some(number)
    }
}

fn has_usage(usage: &CodexRawUsageV1) -> bool {
    usage.input_tokens.is_some()
        || usage.cached_input_tokens.is_some()
        || usage.cache_write_input_tokens.is_some()
        || usage.output_tokens.is_some()
}

fn usage_decreased(previous: &CodexRawUsageV1, current: &CodexRawUsageV1) -> bool {
    [
        (previous.input_tokens, current.input_tokens),
        (previous.cached_input_tokens, current.cached_input_tokens),
        (
            previous.cache_write_input_tokens,
            current.cache_write_input_tokens,
        ),
        (previous.output_tokens, current.output_tokens),
    ]
    .into_iter()
    .any(|(before, after)| matches!((before, after), (Some(before), Some(after)) if after < before))
}

fn normalize_usage(raw: &CodexRawUsageV1) -> Option<RunUsage> {
    let input_tokens = match (
        raw.input_tokens,
        raw.cached_input_tokens,
        raw.cache_write_input_tokens,
    ) {
        (Some(total), Some(read), Some(write)) => total
            .checked_sub(read)
            .and_then(|value| value.checked_sub(write)),
        _ => None,
    };
    let usage = RunUsage {
        input_tokens,
        output_tokens: raw.output_tokens,
        cache_read_tokens: raw.cached_input_tokens,
        cache_write_tokens: raw.cache_write_input_tokens,
    };
    (usage.input_tokens.is_some()
        || usage.output_tokens.is_some()
        || usage.cache_read_tokens.is_some()
        || usage.cache_write_tokens.is_some())
    .then_some(usage)
}

fn valid_short(value: &str, max_len: usize) -> bool {
    !value.trim().is_empty()
        && value.encode_utf16().count() <= max_len
        && !value.chars().any(char::is_control)
}

fn truncate_utf16(value: &str, max_units: usize) -> String {
    let mut output = String::new();
    let mut units = 0;
    for character in value.chars() {
        let character_units = character.len_utf16();
        if units + character_units > max_units {
            break;
        }
        output.push(character);
        units += character_units;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::CodexRunReport;
    use crate::codex_stream::CodexStreamParser;
    use crate::protocol::{CodexMeasurementStatus, FinishStatus};

    fn report_for(stream: &str, status: FinishStatus) -> crate::protocol::FinishRunRequest {
        let mut parser = CodexStreamParser::default();
        let mut report = CodexRunReport::new(Some("gpt-5.1-codex"));
        for event in parser.push(stream).into_iter().chain(parser.finish()) {
            report.observe(&event);
        }
        report.into_finish_request(status, None)
    }

    #[test]
    fn successful_stream_maps_codex_usage_without_double_counting_cached_tokens() {
        let request = report_for(
            r#"{"type":"thread.started","thread_id":"thread-1"}
{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":20,"cache_write_input_tokens":5,"output_tokens":7}}"#,
            FinishStatus::Completed,
        );
        let usage = request.usage.expect("terminal usage");
        assert_eq!(usage.input_tokens, Some(75));
        assert_eq!(usage.cache_read_tokens, Some(20));
        assert_eq!(usage.cache_write_tokens, Some(5));
        assert_eq!(usage.output_tokens, Some(7));
        assert_eq!(request.provider_session_id.as_deref(), Some("thread-1"));
        assert_eq!(
            request.pricing_evidence.unwrap().measurement_status,
            CodexMeasurementStatus::Complete
        );
    }

    #[test]
    fn failed_or_partial_stream_keeps_observed_usage_without_filling_missing_dimensions() {
        let request = report_for(
            r#"{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":7}}"#,
            FinishStatus::Failed,
        );
        let usage = request.usage.expect("available output usage");
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, Some(7));
        assert_eq!(usage.cache_read_tokens, None);
        assert_eq!(usage.cache_write_tokens, None);
        assert_eq!(
            request.pricing_evidence.unwrap().measurement_status,
            CodexMeasurementStatus::IncompleteAttempt
        );
    }

    #[test]
    fn unavailable_usage_is_omitted_and_reported_as_missing() {
        let request = CodexRunReport::new(None).into_finish_request(FinishStatus::Completed, None);
        assert!(request.usage.is_none());
        assert!(request.provider_session_id.is_none());
        let evidence = request.pricing_evidence.unwrap();
        assert!(evidence.model.is_none());
        assert!(evidence.raw_usage.is_none());
        assert_eq!(evidence.measurement_status, CodexMeasurementStatus::Missing);
    }
}
