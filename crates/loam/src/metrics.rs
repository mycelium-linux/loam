//! `soil:metrics` — a registry the host owns and the Spore only writes to.
//!
//! A metric is a fact, not an action: it says what the work did, and it must
//! never be able to fail the work. That is why the budgets here are enforced at
//! write time (a bad name is refused loudly) and why the HTTP surface belongs to
//! the host: a Spore cannot choose a port, a path, or who scrapes it.
//!
//! Counters add, gauges replace. Labels are sorted, so the same series written
//! with its labels in a different order is one series — the renderer and the
//! duplicate check both depend on that.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;

/// Longest series or label name the registry accepts.
const MAX_NAME: usize = 100;
/// Longest label value.
const MAX_LABEL_VALUE: usize = 200;
/// Most labels on one series.
const MAX_LABELS: usize = 16;
/// Most series one registry holds before writes fail closed.
const MAX_SERIES: usize = 10_000;

/// What a sample does to its series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Monotonic: `emit` adds to the series.
    Counter,
    /// `emit` replaces the series.
    Gauge,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
        }
    }
}

/// One series plus its current value.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub name: String,
    pub kind: Kind,
    pub value: f64,
    pub help: Option<String>,
    pub labels: BTreeMap<String, String>,
}

/// Why a write was refused. Every variant is the caller's bug, not a runtime
/// condition: a Spore that emits outside the budget is ignored, and the host
/// keeps serving the series it already has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricsError {
    Invalid,
    OverBudget,
}

type Series = (String, BTreeMap<String, String>);

#[derive(Default)]
struct Registry {
    series: BTreeMap<Series, (Kind, f64, Option<String>)>,
}

/// The per-Spore metrics registry. One instance per hosted Spore, shared by
/// every request store and the renderer, so `/metrics` shows exactly the series
/// that guest wrote; `emit` is the only writer.
#[derive(Default)]
pub struct Metrics {
    registry: Mutex<Registry>,
}

impl Metrics {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record samples. A counter adds, a gauge replaces.
    ///
    /// # Errors
    /// [`MetricsError::Invalid`] for a name, label or value outside the budget;
    /// [`MetricsError::OverBudget`] when the registry is full.
    pub fn emit(&self, samples: &[Sample]) -> Result<(), MetricsError> {
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        for sample in samples {
            validate(sample)?;
            let key = (sample.name.clone(), sample.labels.clone());
            let existing_kind = registry.series.get(&key).map(|(kind, ..)| *kind);
            if existing_kind.is_some_and(|kind| kind != sample.kind) {
                // Same series, two kinds: the renderer cannot emit both a
                // counter and a gauge under one name.
                return Err(MetricsError::Invalid);
            }
            if existing_kind.is_none() && registry.series.len() >= MAX_SERIES {
                return Err(MetricsError::OverBudget);
            }
            let entry =
                registry
                    .series
                    .entry(key)
                    .or_insert((sample.kind, 0.0, sample.help.clone()));
            match sample.kind {
                Kind::Counter => entry.1 += sample.value,
                Kind::Gauge => entry.1 = sample.value,
            }
            if entry.2.is_none() {
                entry.2 = sample.help.clone();
            }
        }
        Ok(())
    }

    /// Every series currently held, sorted by name then labels.
    #[must_use]
    pub fn snapshot(&self) -> Vec<Sample> {
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        registry
            .series
            .iter()
            .map(|((name, labels), (kind, value, help))| Sample {
                name: name.clone(),
                kind: *kind,
                value: *value,
                help: help.clone(),
                labels: labels.clone(),
            })
            .collect()
    }

    /// Prometheus text exposition of the current registry.
    ///
    /// `HELP` and `TYPE` are written once per name, before its series, which is
    /// what the format requires and what makes the output diffable.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut last: Option<(&str, Kind)> = None;
        let snapshot = self.snapshot();
        for sample in &snapshot {
            if last != Some((sample.name.as_str(), sample.kind)) {
                if let Some(help) = &sample.help {
                    let _ = writeln!(out, "# HELP {} {}", sample.name, escape_help(help));
                }
                let _ = writeln!(out, "# TYPE {} {}", sample.name, sample.kind.as_str());
                last = Some((sample.name.as_str(), sample.kind));
            }
            out.push_str(&sample.name);
            if !sample.labels.is_empty() {
                out.push('{');
                for (i, (key, value)) in sample.labels.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    let _ = write!(out, "{key}=\"{}\"", escape_label(value));
                }
                out.push('}');
            }
            let _ = writeln!(out, " {}", sample.value);
        }
        out
    }
}

fn validate(sample: &Sample) -> Result<(), MetricsError> {
    if !valid_name(&sample.name) || sample.name.len() > MAX_NAME {
        return Err(MetricsError::Invalid);
    }
    if !sample.value.is_finite() {
        return Err(MetricsError::Invalid);
    }
    if sample.kind == Kind::Counter && sample.value < 0.0 {
        return Err(MetricsError::Invalid);
    }
    if sample.labels.len() > MAX_LABELS {
        return Err(MetricsError::OverBudget);
    }
    for (key, value) in &sample.labels {
        if !valid_name(key) || key.len() > MAX_NAME {
            return Err(MetricsError::Invalid);
        }
        if value.len() > MAX_LABEL_VALUE || value.contains(['\n', '"']) {
            return Err(MetricsError::Invalid);
        }
    }
    if sample
        .help
        .as_ref()
        .is_some_and(|help| help.contains(['\n', '\\']))
    {
        return Err(MetricsError::Invalid);
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn escape_label(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn escape_help(value: &str) -> String {
    value.replace('\\', "\\\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str, kind: Kind, value: f64) -> Sample {
        Sample {
            name: name.to_owned(),
            kind,
            value,
            help: None,
            labels: BTreeMap::new(),
        }
    }

    fn labelled(name: &str, kind: Kind, value: f64, labels: &[(&str, &str)]) -> Sample {
        Sample {
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..sample(name, kind, value)
        }
    }

    #[test]
    fn a_counter_adds_and_a_gauge_replaces() {
        let metrics = Metrics::new();
        metrics
            .emit(&[sample("soil_builds_total", Kind::Counter, 2.0)])
            .unwrap();
        metrics
            .emit(&[sample("soil_builds_total", Kind::Counter, 3.0)])
            .unwrap();
        metrics
            .emit(&[sample("soil_queue_depth", Kind::Gauge, 9.0)])
            .unwrap();
        metrics
            .emit(&[sample("soil_queue_depth", Kind::Gauge, 1.0)])
            .unwrap();
        let values: BTreeMap<_, _> = metrics
            .snapshot()
            .into_iter()
            .map(|s| (s.name, s.value))
            .collect();
        assert_eq!(values["soil_builds_total"], 5.0);
        assert_eq!(values["soil_queue_depth"], 1.0);
    }

    #[test]
    fn label_order_is_the_same_series() {
        let metrics = Metrics::new();
        metrics
            .emit(&[labelled(
                "soil_agents",
                Kind::Gauge,
                1.0,
                &[("host", "batty"), ("phase", "working")],
            )])
            .unwrap();
        metrics
            .emit(&[labelled(
                "soil_agents",
                Kind::Gauge,
                2.0,
                &[("phase", "working"), ("host", "batty")],
            )])
            .unwrap();
        assert_eq!(metrics.snapshot().len(), 1);
    }

    #[test]
    fn a_second_kind_for_one_series_is_refused() {
        let metrics = Metrics::new();
        metrics
            .emit(&[sample("soil_thing", Kind::Gauge, 1.0)])
            .unwrap();
        assert_eq!(
            metrics.emit(&[sample("soil_thing", Kind::Counter, 1.0)]),
            Err(MetricsError::Invalid)
        );
    }

    #[test]
    fn bad_names_and_values_are_refused() {
        let metrics = Metrics::new();
        for bad in [
            sample("1starts_with_digit", Kind::Gauge, 1.0),
            sample("has-dash", Kind::Gauge, 1.0),
            sample("has space", Kind::Gauge, 1.0),
            sample("", Kind::Gauge, 1.0),
            sample("nan", Kind::Gauge, f64::NAN),
            sample("negative_counter", Kind::Counter, -1.0),
        ] {
            assert_eq!(metrics.emit(&[bad]), Err(MetricsError::Invalid));
        }
    }

    #[test]
    fn a_label_value_with_a_quote_or_newline_is_refused() {
        let metrics = Metrics::new();
        for bad in [&["x\"y"][..], &["line\nbreak"][..]] {
            let mut sample = labelled("soil_agents", Kind::Gauge, 1.0, &[("host", "batty")]);
            sample.labels.insert("note".to_owned(), bad[0].to_owned());
            assert_eq!(metrics.emit(&[sample]), Err(MetricsError::Invalid));
        }
    }

    #[test]
    fn rendering_is_prometheus_text_with_one_help_and_type_per_name() {
        let metrics = Metrics::new();
        let mut s = labelled(
            "soil_agents",
            Kind::Gauge,
            3.0,
            &[("phase", "working"), ("host", "batty")],
        );
        s.help = Some("Agents by phase".to_owned());
        metrics.emit(&[s]).unwrap();
        let text = metrics.render();
        assert!(
            text.contains("# HELP soil_agents Agents by phase\n"),
            "{text}"
        );
        assert!(text.contains("# TYPE soil_agents gauge\n"), "{text}");
        assert!(
            text.contains("soil_agents{host=\"batty\",phase=\"working\"} 3\n"),
            "{text}"
        );
        assert_eq!(text.matches("# TYPE soil_agents").count(), 1);
    }
}
