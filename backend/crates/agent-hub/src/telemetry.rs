//! services/audit.py `Telemetry` + `prometheus_client.generate_latest`: the same metric names, types, labels, sample
//! order and text format (Grafana boards use them). Hand-written exposition because prometheus_client's output
//! (`_created` gauges, `floatToGoString`, `le` first) differs from the Rust crates'. Owner: CORE (LOCKED).

use std::sync::Mutex;

use hub_core::common::now;
use hub_core::py::float;

/// `CONTENT_TYPE_LATEST` of prometheus_client 0.26.
pub const CONTENT_TYPE: &str = "text/plain; version=1.0.0; charset=utf-8";
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
    let dot = s.find('.');
    if let Some(dot) = dot
        && d > 0.0
        && dot > 6
    {
        let raw = format!("{}.{}{}", &s[..1], &s[1..dot], &s[dot + 1..]);
        let mantissa = raw.trim_end_matches(['0', '.']);
        return format!("{mantissa}e+0{}", dot - 1);
    }
    s
}

fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\n', "\\n").replace('"', "\\\"")
}

fn labels(names: &[&str], values: &[String], le: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(le) = le {
        parts.push(format!("le=\"{le}\""));
    }
    for (n, v) in names.iter().zip(values) {
        parts.push(format!("{n}=\"{}\"", escape_label(v)));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", parts.join(","))
    }
}

struct Child {
    labels: Vec<String>,
    value: f64,
    created: f64,
    buckets: Vec<f64>,
    sum: f64,
}

/// A labelled counter or histogram (`labelnames` order as declared; children in first-use order).
pub struct Metric {
    name: &'static str,
    help: &'static str,
    labelnames: &'static [&'static str],
    histogram: bool,
    children: Mutex<Vec<Child>>,
    created: f64,
}

impl Metric {
    fn new(name: &'static str, help: &'static str, labelnames: &'static [&'static str], histogram: bool) -> Self {
        let m = Self {
            name,
            help,
            labelnames,
            histogram,
            children: Mutex::new(Vec::new()),
            created: now(),
        };
        if labelnames.is_empty() {
            m.child(&[]);
        }
        m
    }

    fn child(&self, values: &[&str]) {
        let Ok(mut ch) = self.children.lock() else { return };
        if !ch
            .iter()
            .any(|c| c.labels.iter().map(String::as_str).eq(values.iter().copied()))
        {
            ch.push(Child {
                labels: values.iter().map(|s| (*s).to_string()).collect(),
                value: 0.0,
                created: now(),
                buckets: vec![0.0; BUCKETS.len() + 1],
                sum: 0.0,
            });
        }
    }

    fn with<F: FnOnce(&mut Child)>(&self, values: &[&str], f: F) {
        self.child(values);
        if let Ok(mut ch) = self.children.lock()
            && let Some(c) = ch
                .iter_mut()
                .find(|c| c.labels.iter().map(String::as_str).eq(values.iter().copied()))
        {
            f(c);
        }
    }

    /// `counter.labels(*values).inc()`.
    pub fn inc(&self, values: &[&str]) {
        self.with(values, |c| c.value += 1.0);
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
        });
    }

    fn render(&self, out: &mut String) {
        let Ok(ch) = self.children.lock() else { return };
        let base = self.name.trim_end_matches("_total");
        if self.histogram {
            out.push_str(&format!(
                "# HELP {} {}\n# TYPE {} histogram\n",
                self.name, self.help, self.name
            ));
            for c in ch.iter() {
                for (i, b) in BUCKETS.iter().enumerate() {
                    let le = go_float(*b);
                    out.push_str(&format!(
                        "{}_bucket{} {}\n",
                        self.name,
                        labels(self.labelnames, &c.labels, Some(&le)),
                        go_float(c.buckets[i])
                    ));
                }
                out.push_str(&format!(
                    "{}_bucket{} {}\n",
                    self.name,
                    labels(self.labelnames, &c.labels, Some("+Inf")),
                    go_float(c.buckets[BUCKETS.len()])
                ));
                out.push_str(&format!(
                    "{}_count{} {}\n",
                    self.name,
                    labels(self.labelnames, &c.labels, None),
                    go_float(c.buckets[BUCKETS.len()])
                ));
                out.push_str(&format!(
                    "{}_sum{} {}\n",
                    self.name,
                    labels(self.labelnames, &c.labels, None),
                    go_float(c.sum)
                ));
            }
            out.push_str(&format!(
                "# HELP {}_created {}\n# TYPE {}_created gauge\n",
                self.name, self.help, self.name
            ));
            for c in ch.iter() {
                out.push_str(&format!(
                    "{}_created{} {}\n",
                    self.name,
                    labels(self.labelnames, &c.labels, None),
                    go_float(c.created)
                ));
            }
        } else {
            out.push_str(&format!(
                "# HELP {} {}\n# TYPE {} counter\n",
                self.name, self.help, self.name
            ));
            for c in ch.iter() {
                out.push_str(&format!(
                    "{}{} {}\n",
                    self.name,
                    labels(self.labelnames, &c.labels, None),
                    go_float(c.value)
                ));
            }
            out.push_str(&format!(
                "# HELP {base}_created {}\n# TYPE {base}_created gauge\n",
                self.help
            ));
            for c in ch.iter() {
                let created = if self.labelnames.is_empty() {
                    self.created
                } else {
                    c.created
                };
                out.push_str(&format!(
                    "{base}_created{} {}\n",
                    labels(self.labelnames, &c.labels, None),
                    go_float(created)
                ));
            }
        }
    }
}

/// The hub's own registry (never the global one).
pub struct Telemetry {
    pub requests: Metric,
    pub latency: Metric,
    pub applies: Metric,
    pub plans: Metric,
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
                "agent_hub_http_requests_total",
                "HTTP requests",
                &["method", "route", "status"],
                false,
            ),
            latency: Metric::new(
                "agent_hub_http_request_seconds",
                "HTTP latency",
                &["method", "route"],
                true,
            ),
            applies: Metric::new("agent_hub_applies_total", "Client applies", &["status"], false),
            plans: Metric::new("agent_hub_plans_total", "Plans computed", &[], false),
        }
    }

    /// `generate_latest(registry)`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for m in [&self.requests, &self.latency, &self.applies, &self.plans] {
            m.render(&mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposition_shape_matches_prometheus_client() {
        assert_eq!(go_float(1_793_140_269.270_06), "1.79314026927006e+09");
        assert_eq!(go_float(1.0), "1.0");
        assert_eq!(go_float(0.0123), "0.0123");
        let t = Telemetry::new();
        t.requests.inc(&["GET", "/api/v1/health", "200"]);
        t.latency.observe(&["GET", "/api/v1/health"], 0.0123);
        let s = t.render();
        assert!(s.starts_with(
            "# HELP agent_hub_http_requests_total HTTP requests\n# TYPE agent_hub_http_requests_total counter\n"
        ));
        assert!(
            s.contains("agent_hub_http_requests_total{method=\"GET\",route=\"/api/v1/health\",status=\"200\"} 1.0\n")
        );
        assert!(s.contains(
            "agent_hub_http_request_seconds_bucket{le=\"0.025\",method=\"GET\",route=\"/api/v1/health\"} 1.0\n"
        ));
        assert!(s.contains(
            "agent_hub_http_request_seconds_bucket{le=\"0.01\",method=\"GET\",route=\"/api/v1/health\"} 0.0\n"
        ));
        assert!(s.contains("agent_hub_plans_total 0.0\n# HELP agent_hub_plans_created Plans computed\n"));
    }
}
