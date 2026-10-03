//! App metrics: an app publishes a whole snapshot in the Prometheus text format (any client
//! library renders it: `prometheus-client`, Python `prometheus_client`, Go `client_golang`),
//! Shell validates it here and serves the merge of all apps on `/metrics`.
//!
//! Both the classic text format (0.0.4) and OpenMetrics are accepted; the output is 0.0.4.
//! Timestamps and `_created` samples are dropped: Prometheus stamps samples at scrape.

use std::collections::HashMap;
use std::fmt::Write as _;

/// Limits of one app's snapshot.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Samples (series) kept; the rest is dropped and counted.
    pub max_series: usize,
    pub max_bytes: usize,
}

/// Labels of a series, without values (`le`, `quantile` included).
pub const MAX_LABELS: usize = 10;
pub const MAX_NAME: usize = 128;
pub const MAX_LABEL_VALUE: usize = 128;
pub const MAX_HELP: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Counter,
    Gauge,
    Histogram,
    Summary,
    Untyped,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
            Kind::Summary => "summary",
            Kind::Untyped => "untyped",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    /// The sample's own name: `x_total`, `x_bucket`, `x_sum`, …
    pub name: String,
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

#[derive(Debug, Clone)]
pub struct Family {
    pub name: String,
    pub kind: Kind,
    pub help: Option<String>,
    pub samples: Vec<Sample>,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub families: Vec<Family>,
    /// Samples kept.
    pub series: usize,
    /// Samples over `max_series`, dropped.
    pub dropped: usize,
}

/// Declared metadata of a family (`# TYPE`, `# HELP`), by the declared name.
struct Meta {
    kind: Option<DeclaredKind>,
    help: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeclaredKind {
    Counter,
    Gauge,
    Histogram,
    Summary,
    Untyped,
    /// OpenMetrics `info`: samples `<name>_info`, served as a gauge.
    Info,
    /// OpenMetrics `stateset`: served as a gauge.
    StateSet,
}

/// Parses and validates an app's snapshot. A syntax or naming error rejects the whole
/// snapshot (the previous one stays); samples over the series limit are dropped and counted.
pub fn parse(text: &str, limits: &Limits) -> Result<Snapshot, String> {
    if text.len() > limits.max_bytes {
        return Err(format!("snapshot is {} bytes, the limit is {}", text.len(), limits.max_bytes));
    }
    let lines: Vec<&str> = text.lines().collect();

    // Metadata first: in OpenMetrics `# TYPE` precedes the samples, in 0.0.4 it should.
    let mut meta: HashMap<String, Meta> = HashMap::new();
    for (n, line) in lines.iter().enumerate() {
        let err = |m: String| format!("line {}: {m}", n + 1);
        let Some(rest) = line.strip_prefix('#') else { continue };
        let mut parts = rest.trim_start().splitn(3, ' ');
        let (Some(keyword), Some(name)) = (parts.next(), parts.next()) else { continue };
        let value = parts.next().unwrap_or("");
        match keyword {
            "TYPE" => {
                check_name(name).map_err(err)?;
                let kind = match value.trim() {
                    "counter" => DeclaredKind::Counter,
                    "gauge" => DeclaredKind::Gauge,
                    "histogram" => DeclaredKind::Histogram,
                    "summary" => DeclaredKind::Summary,
                    "untyped" | "unknown" => DeclaredKind::Untyped,
                    "info" => DeclaredKind::Info,
                    "stateset" => DeclaredKind::StateSet,
                    other => return Err(err(format!("type {other:?} is not supported"))),
                };
                let m = meta.entry(name.to_string()).or_insert(Meta { kind: None, help: None });
                if m.kind.replace(kind).is_some() {
                    return Err(err(format!("{name}: TYPE declared twice")));
                }
            }
            "HELP" => {
                check_name(name).map_err(err)?;
                let help = unescape_help(value);
                if help.len() > MAX_HELP {
                    return Err(err(format!("{name}: HELP is longer than {MAX_HELP} bytes")));
                }
                meta.entry(name.to_string()).or_insert(Meta { kind: None, help: None }).help = Some(help);
            }
            _ => {}
        }
    }

    let mut families: Vec<Family> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut snapshot = Snapshot::default();
    for (n, line) in lines.iter().enumerate() {
        let err = |m: String| format!("line {}: {m}", n + 1);
        let line = line.trim();
        if line == "# EOF" {
            break;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let sample = parse_sample(line).map_err(err)?;
        let Some((family, kind, help)) = resolve(&sample.name, &meta) else {
            continue; // `_created`: Prometheus stamps samples itself
        };
        let slot = *index.entry(family.clone()).or_insert_with(|| {
            families.push(Family { name: family.clone(), kind, help: help.clone(), samples: Vec::new() });
            families.len() - 1
        });
        if snapshot.series >= limits.max_series {
            snapshot.dropped += 1;
            continue;
        }
        snapshot.series += 1;
        families[slot].samples.push(sample);
    }
    families.retain(|f| !f.samples.is_empty());
    snapshot.families = families;
    Ok(snapshot)
}

/// The family a sample belongs to: its served name, kind and help. `None` for samples
/// that are dropped on purpose (`_created`).
fn resolve(sample: &str, meta: &HashMap<String, Meta>) -> Option<(String, Kind, Option<String>)> {
    let declared = |name: &str| meta.get(name).and_then(|m| m.kind.map(|k| (k, m.help.clone())));
    if let Some((kind, help)) = declared(sample) {
        let kind = match kind {
            DeclaredKind::Counter => Kind::Counter,
            DeclaredKind::Gauge | DeclaredKind::Info | DeclaredKind::StateSet => Kind::Gauge,
            DeclaredKind::Histogram => Kind::Histogram,
            DeclaredKind::Summary => Kind::Summary,
            DeclaredKind::Untyped => Kind::Untyped,
        };
        return Some((sample.to_string(), kind, help));
    }
    for suffix in ["_total", "_bucket", "_sum", "_count", "_created", "_info"] {
        let Some(base) = sample.strip_suffix(suffix) else { continue };
        let Some((kind, help)) = declared(base) else { continue };
        return match (kind, suffix) {
            (_, "_created") => None,
            // OpenMetrics declares a counter without `_total`; 0.0.4 names the family with it.
            (DeclaredKind::Counter, "_total") => Some((sample.to_string(), Kind::Counter, help)),
            (DeclaredKind::Info, "_info") => Some((sample.to_string(), Kind::Gauge, help)),
            (DeclaredKind::Histogram, "_bucket" | "_sum" | "_count") => Some((base.to_string(), Kind::Histogram, help)),
            (DeclaredKind::Summary, "_sum" | "_count") => Some((base.to_string(), Kind::Summary, help)),
            _ => Some((sample.to_string(), Kind::Untyped, None)),
        };
    }
    let help = meta.get(sample).and_then(|m| m.help.clone());
    Some((sample.to_string(), Kind::Untyped, help))
}

fn parse_sample(line: &str) -> Result<Sample, String> {
    let name_end = line.find(|c: char| c == '{' || c.is_whitespace()).unwrap_or(line.len());
    let name = &line[..name_end];
    check_name(name)?;
    let mut rest = &line[name_end..];
    let mut labels: Vec<(String, String)> = Vec::new();
    if let Some(after) = rest.strip_prefix('{') {
        let (parsed, remaining) = parse_labels(after)?;
        labels = parsed;
        rest = remaining;
    }
    let mut fields = rest.split_whitespace();
    let value = fields.next().ok_or("no value")?;
    let value = parse_value(value).ok_or_else(|| format!("invalid value {value:?}"))?;
    // An optional timestamp (and an OpenMetrics exemplar) follow; Prometheus stamps samples itself.
    if labels.len() > MAX_LABELS {
        return Err(format!("{name}: more than {MAX_LABELS} labels"));
    }
    Ok(Sample { name: name.to_string(), labels, value })
}

/// `name="value",…}` → labels and the rest of the line after `}`.
fn parse_labels(mut s: &str) -> Result<(Vec<(String, String)>, &str), String> {
    let mut labels: Vec<(String, String)> = Vec::new();
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix('}') {
            return Ok((labels, rest));
        }
        let eq = s.find('=').ok_or("unterminated labels")?;
        let key = s[..eq].trim();
        check_label_name(key)?;
        if labels.iter().any(|(k, _)| k == key) {
            return Err(format!("label {key} is repeated"));
        }
        s = s[eq + 1..].trim_start();
        s = s.strip_prefix('"').ok_or_else(|| format!("label {key}: value must be quoted"))?;
        let mut value = String::new();
        let mut chars = s.char_indices();
        let end = loop {
            match chars.next() {
                Some((i, '"')) => break i,
                Some((_, '\\')) => match chars.next() {
                    Some((_, 'n')) => value.push('\n'),
                    Some((_, c)) => value.push(c),
                    None => return Err("unterminated label value".into()),
                },
                Some((_, c)) => value.push(c),
                None => return Err("unterminated label value".into()),
            }
        };
        if value.len() > MAX_LABEL_VALUE {
            return Err(format!("label {key}: value is longer than {MAX_LABEL_VALUE} bytes"));
        }
        labels.push((key.to_string(), value));
        s = s[end + 1..].trim_start();
        s = s.strip_prefix(',').unwrap_or(s);
    }
}

fn parse_value(s: &str) -> Option<f64> {
    match s {
        "+Inf" | "Inf" => Some(f64::INFINITY),
        "-Inf" => Some(f64::NEG_INFINITY),
        "NaN" => Some(f64::NAN),
        _ if s.bytes().all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)) => s.parse().ok(),
        _ => None,
    }
}

/// Metric name: `[a-zA-Z_][a-zA-Z0-9_]*` (colons are for recording rules);
/// `wshell_` belongs to Shell's own metrics.
fn check_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.len() <= MAX_NAME;
    if !ok {
        return Err(format!("invalid metric name {name:?}"));
    }
    if name.starts_with("wshell_") {
        return Err(format!("{name}: the wshell_ prefix is reserved for Shell's metrics"));
    }
    Ok(())
}

/// Label name: `[a-zA-Z_][a-zA-Z0-9_]*`, not `__*`, not `app` (Shell sets it).
fn check_label_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.len() <= MAX_NAME
        && !name.starts_with("__");
    if !ok {
        return Err(format!("invalid label name {name:?}"));
    }
    if name == "app" {
        return Err("label app is set by Shell".into());
    }
    Ok(())
}

fn unescape_help(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (c, c == '\\') {
            (_, true) => match chars.next() {
                Some('n') => out.push('\n'),
                Some(c) => out.push(c),
                None => out.push('\\'),
            },
            (c, false) => out.push(c),
        }
    }
    out
}

// --- Output (text format 0.0.4) ---

pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Writes families in the text format; `app` labels are added by the caller.
#[derive(Default)]
pub struct Writer {
    pub out: String,
}

impl Writer {
    pub fn family(&mut self, name: &str, kind: Kind, help: &str) {
        if !help.is_empty() {
            let help = help.replace('\\', "\\\\").replace('\n', "\\n");
            let _ = writeln!(self.out, "# HELP {name} {help}");
        }
        let _ = writeln!(self.out, "# TYPE {name} {}", kind.as_str());
    }

    pub fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: f64) {
        self.out.push_str(name);
        if !labels.is_empty() {
            self.out.push('{');
            for (i, (k, v)) in labels.iter().enumerate() {
                if i > 0 {
                    self.out.push(',');
                }
                let v = v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
                let _ = write!(self.out, "{k}=\"{v}\"");
            }
            self.out.push('}');
        }
        let _ = writeln!(self.out, " {}", format_value(value));
    }
}

fn format_value(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v == f64::INFINITY {
        "+Inf".into()
    } else if v == f64::NEG_INFINITY {
        "-Inf".into()
    } else {
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits { max_series: 100, max_bytes: 64 * 1024 };

    #[test]
    fn classic_text_format() {
        // What Python's prometheus_client renders (0.0.4).
        let text = r#"# HELP checks_total Checks done
# TYPE checks_total counter
checks_total{tool="ping",result="ok"} 3.0
checks_total{tool="ping",result="fail"} 1.0 1700000000000
# HELP checks_created Checks done
# TYPE checks_created gauge
checks_created{tool="ping",result="ok"} 1.7e9
# TYPE duration_seconds histogram
duration_seconds_bucket{le="0.5"} 1
duration_seconds_bucket{le="+Inf"} 2
duration_seconds_sum 1.5
duration_seconds_count 2
"#;
        let s = parse(text, &LIMITS).unwrap();
        let names: Vec<_> = s.families.iter().map(|f| (f.name.as_str(), f.kind)).collect();
        assert_eq!(
            names,
            [("checks_total", Kind::Counter), ("checks_created", Kind::Gauge), ("duration_seconds", Kind::Histogram)]
        );
        assert_eq!(s.families[0].samples[1].value, 1.0);
        assert_eq!(s.families[0].help.as_deref(), Some("Checks done"));
        assert_eq!(s.families[2].samples.len(), 4);
        assert_eq!(s.series, 7);
    }

    #[test]
    fn openmetrics_format() {
        // What Rust's prometheus-client renders: no _total in TYPE, _created, # EOF.
        let text = "# HELP checks Checks done.\n# TYPE checks counter\nchecks_total{result=\"ok\"} 2\n\
                    checks_created{result=\"ok\"} 1700000000.0\n# TYPE build info\nbuild_info{version=\"1\"} 1\n# EOF\n";
        let s = parse(text, &LIMITS).unwrap();
        assert_eq!(s.families[0].name, "checks_total");
        assert_eq!(s.families[0].kind, Kind::Counter);
        assert_eq!(s.families[0].samples.len(), 1);
        assert_eq!(s.families[1].name, "build_info");
        assert_eq!(s.families[1].kind, Kind::Gauge);
    }

    #[test]
    fn rejects_bad_input() {
        for (text, why) in [
            ("x{app=\"other\"} 1", "app"),
            ("wshell_fake 1", "reserved"),
            ("x{__name__=\"y\"} 1", "label name"),
            ("x one", "value"),
            ("x{a=\"1\",a=\"2\"} 1", "repeated"),
            ("x{a=1} 1", "quoted"),
            ("# TYPE x gaugehistogram\nx 1", "not supported"),
        ] {
            let e = parse(text, &LIMITS).unwrap_err();
            assert!(e.contains(why), "{text:?}: {e}");
        }
        assert!(parse(&"x 1\n".repeat(30_000), &LIMITS).unwrap_err().contains("limit"));
    }

    #[test]
    fn series_limit_drops_and_counts() {
        let text: String = (0..150).map(|i| format!("x{{i=\"{i}\"}} {i}\n")).collect();
        let s = parse(&text, &LIMITS).unwrap();
        assert_eq!((s.series, s.dropped), (100, 50));
    }

    #[test]
    fn escapes_round_trip() {
        let s = parse(r#"x{path="a\"b\\c\nd"} +Inf"#, &LIMITS).unwrap();
        let sample = &s.families[0].samples[0];
        assert_eq!(sample.labels[0].1, "a\"b\\c\nd");
        let mut w = Writer::default();
        w.sample("x", &[("app", "org.a"), ("path", &sample.labels[0].1)], sample.value);
        assert_eq!(w.out, "x{app=\"org.a\",path=\"a\\\"b\\\\c\\nd\"} +Inf\n");
    }
}
