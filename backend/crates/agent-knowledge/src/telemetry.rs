//! Port of telemetry.py: the per-app Prometheus registry, rendered byte-for-byte like prometheus_client 0.26's
//! `generate_latest` (text format 0.0.4): HELP/TYPE per family, label pairs sorted by name (so `le` comes first in
//! buckets), values through `floatToGoString`, and each family's `_created` samples in their own gauge family, which
//! is only written when the family has children.

use std::sync::Mutex;

use hub_core::common::now;
use hub_core::py::float;

const BUCKETS: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// prometheus_client `floatToGoString`.
pub fn go_float(d: f64) -> String {
    if d == f64::INFINITY {
        return "+Inf".into();
    }
    if d == f64::NEG_INFINITY {
        return "-Inf".into();
    }
    if d.is_nan() {
        return "NaN".into();
    }
    let s = float::repr(d);
    if let Some(dot) = s.find('.')
        && d > 0.0
        && dot > 6
    {
        let raw = format!("{}.{}{}", &s[..1], &s[1..dot], &s[dot + 1..]);
        let mantissa = raw.trim_end_matches(['0', '.']);
        return format!("{mantissa}e+{:02}", dot - 1);
    }
    s
}

fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\n', "\\n").replace('"', "\\\"")
}

fn labels(pairs: &[(&str, &str)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<&(&str, &str)> = pairs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let body: Vec<String> = sorted
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape_label(v)))
        .collect();
    format!("{{{}}}", body.join(","))
}

struct Child {
    values: Vec<String>,
    count: f64,
    created: f64,
    buckets: Vec<f64>,
    sum: f64,
}

/// A labelled counter (`name` ends in `_total`) or histogram; children in first-use order.
pub struct Metric {
    name: &'static str,
    help: &'static str,
    labelnames: &'static [&'static str],
    histogram: bool,
    children: Mutex<Vec<Child>>,
}

impl Metric {
    fn new(name: &'static str, help: &'static str, labelnames: &'static [&'static str], histogram: bool) -> Self {
        Self {
            name,
            help,
            labelnames,
            histogram,
            children: Mutex::new(Vec::new()),
        }
    }

    fn with(&self, values: &[&str], f: impl FnOnce(&mut Child)) {
        let Ok(mut ch) = self.children.lock() else { return };
        let i = match ch
            .iter()
            .position(|c| c.values.iter().map(String::as_str).eq(values.iter().copied()))
        {
            Some(i) => i,
            None => {
                ch.push(Child {
                    values: values.iter().map(|s| (*s).to_string()).collect(),
                    count: 0.0,
                    created: now(),
                    buckets: vec![0.0; BUCKETS.len() + 1],
                    sum: 0.0,
                });
                ch.len() - 1
            }
        };
        f(&mut ch[i]);
    }

    /// `counter.labels(*values).inc(n)`.
    pub fn inc_by(&self, values: &[&str], n: f64) {
        self.with(values, |c| c.count += n);
    }

    /// `counter.labels(*values).inc()`.
    pub fn inc(&self, values: &[&str]) {
        self.inc_by(values, 1.0);
    }

    /// `histogram.labels(*values).observe(v)`.
    pub fn observe(&self, values: &[&str], v: f64) {
        self.with(values, |c| {
            c.sum += v;
            for (i, b) in BUCKETS.iter().enumerate() {
                if v <= *b {
                    c.buckets[i] += 1.0;
                }
            }
            c.buckets[BUCKETS.len()] += 1.0;
            c.count += 1.0;
        });
    }

    /// The value of one counter child (tests).
    pub fn value(&self, values: &[&str]) -> f64 {
        self.children.lock().map_or(0.0, |ch| {
            ch.iter()
                .find(|c| c.values.iter().map(String::as_str).eq(values.iter().copied()))
                .map_or(0.0, |c| c.count)
        })
    }

    fn render(&self, out: &mut String) {
        let Ok(ch) = self.children.lock() else { return };
        let base = if self.histogram {
            self.name
        } else {
            self.name.trim_end_matches("_total")
        };
        let mtype = if self.histogram { "histogram" } else { "counter" };
        out.push_str(&format!(
            "# HELP {} {}\n# TYPE {} {mtype}\n",
            self.name, self.help, self.name
        ));
        for c in ch.iter() {
            let pairs: Vec<(&str, &str)> = self
                .labelnames
                .iter()
                .copied()
                .zip(c.values.iter().map(String::as_str))
                .collect();
            if self.histogram {
                for (i, b) in BUCKETS.iter().enumerate() {
                    let le = go_float(*b);
                    let mut p = pairs.clone();
                    p.push(("le", &le));
                    out.push_str(&format!(
                        "{}_bucket{} {}\n",
                        self.name,
                        labels(&p),
                        go_float(c.buckets[i])
                    ));
                }
                let mut p = pairs.clone();
                p.push(("le", "+Inf"));
                out.push_str(&format!(
                    "{}_bucket{} {}\n",
                    self.name,
                    labels(&p),
                    go_float(c.buckets[BUCKETS.len()])
                ));
                out.push_str(&format!(
                    "{}_count{} {}\n",
                    self.name,
                    labels(&pairs),
                    go_float(c.count)
                ));
                out.push_str(&format!("{}_sum{} {}\n", self.name, labels(&pairs), go_float(c.sum)));
            } else {
                out.push_str(&format!("{}{} {}\n", self.name, labels(&pairs), go_float(c.count)));
            }
        }
        if ch.is_empty() {
            return;
        }
        out.push_str(&format!(
            "# HELP {base}_created {}\n# TYPE {base}_created gauge\n",
            self.help
        ));
        for c in ch.iter() {
            let pairs: Vec<(&str, &str)> = self
                .labelnames
                .iter()
                .copied()
                .zip(c.values.iter().map(String::as_str))
                .collect();
            out.push_str(&format!("{base}_created{} {}\n", labels(&pairs), go_float(c.created)));
        }
    }
}

/// `Telemetry`: a registry per app (no global state, so tests can build many apps).
pub struct Telemetry {
    pub requests: Metric,
    pub latency: Metric,
    pub queries: Metric,
    pub chunks: Metric,
    pub jobs: Metric,
    pub denied: Metric,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::new()
    }
}

impl Telemetry {
    pub fn new() -> Self {
        Self {
            requests: Metric::new(
                "agent_knowledge_requests_total",
                "HTTP requests",
                &["method", "route", "status"],
                false,
            ),
            latency: Metric::new(
                "agent_knowledge_request_seconds",
                "HTTP latency",
                &["method", "route"],
                true,
            ),
            queries: Metric::new("agent_knowledge_queries_total", "vector queries", &["ns"], false),
            chunks: Metric::new(
                "agent_knowledge_chunks_ingested_total",
                "chunks ingested",
                &["ns"],
                false,
            ),
            jobs: Metric::new("agent_knowledge_jobs_total", "ingest jobs finished", &["state"], false),
            denied: Metric::new(
                "agent_knowledge_denied_total",
                "auth/scope/quota denials",
                &["reason"],
                false,
            ),
        }
    }

    /// `generate_latest(registry)`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for m in [
            &self.requests,
            &self.latency,
            &self.queries,
            &self.chunks,
            &self.jobs,
            &self.denied,
        ] {
            m.render(&mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_float_matches_prometheus_client() {
        assert_eq!(go_float(1.0), "1.0");
        assert_eq!(go_float(0.003), "0.003");
        assert_eq!(go_float(1_793_148_867.434_792_3), "1.7931488674347923e+09");
        assert_eq!(go_float(12_345_678_901.5), "1.23456789015e+10");
        assert_eq!(go_float(f64::INFINITY), "+Inf");
        assert_eq!(go_float(1e16), "1e+16");
    }

    #[test]
    fn layout_matches_generate_latest() {
        let t = Telemetry::new();
        t.requests.inc(&["GET", "/x", "200"]);
        t.latency.observe(&["GET", "/x"], 0.003);
        t.queries.inc(&["docs"]);
        let text = t.render();
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| !l.contains("_created ") || l.starts_with('#'))
            .collect();
        assert!(lines.contains(&"agent_knowledge_requests_total{method=\"GET\",route=\"/x\",status=\"200\"} 1.0"));
        assert!(
            lines.contains(&"agent_knowledge_request_seconds_bucket{le=\"0.005\",method=\"GET\",route=\"/x\"} 1.0")
        );
        assert!(lines.contains(&"agent_knowledge_request_seconds_sum{method=\"GET\",route=\"/x\"} 0.003"));
        assert!(text.contains(
            "# HELP agent_knowledge_requests_created HTTP requests\n# TYPE agent_knowledge_requests_created gauge\n"
        ));
        assert!(
            text.contains("# TYPE agent_knowledge_chunks_ingested_total counter\n# HELP agent_knowledge_jobs_total")
        );
    }
}
