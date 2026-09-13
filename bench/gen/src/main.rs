//! W1 benchmark data generator.
//!
//! Emits TabSeparated rows on stdout for piping straight into clickhouse-client.
//!
//! Two fidelity decisions that make the numbers meaningful:
//!
//! 1. **Rows are emitted in TIME order, not sort-key order.** The ClickHouse sort key is
//!    `(tenant_id, resource_id, observed_at)` but real telemetry arrives ordered by time.
//!    Generating in sort-key order would produce unrealistically cheap merges and a
//!    flattering compression ratio. Time-ordered generation is the real workload.
//!
//! 2. **Needle tokens are placed at EXACT known frequencies**, not probabilistically.
//!    A text-search benchmark is meaningless unless you know how many rows should match.
//!    Every needle's expected count is `rows / period`, verifiable with a COUNT(*).
//!
//! Usage:
//!   uops-bench-gen logs    --rows 100000000 --resources 5000 --tenants 3 --days 7
//!   uops-bench-gen metrics --rows 100000000 --resources 5000 --metrics 20 --days 7

use std::io::{self, BufWriter, Write};

// ---------------------------------------------------------------------------
// Needle tokens: exact frequencies so search results are interpretable.
// ---------------------------------------------------------------------------

/// (token, period) — the token appears on every Nth row.
/// Expected matches at 100M rows: 1, 100, 10k, 1M.
/// Expected matches at 1B rows:  10, 1000, 100k, 10M.
const NEEDLES: &[(&str, u64)] = &[
    ("zzqx_ultrarare", 100_000_000),
    ("zzqx_rare", 1_000_000),
    ("zzqx_mid", 10_000),
    ("zzqx_common", 100),
];

// ---------------------------------------------------------------------------
// Realistic body templates. {N} is substituted with varying values so bodies are
// not trivially dictionary-compressible — a pitfall that would inflate the
// compression ratio and make the benchmark lie.
// ---------------------------------------------------------------------------

const LOG_TEMPLATES: &[&str] = &[
    "%LINK-3-UPDOWN: Interface GigabitEthernet0/{0}, changed state to down",
    "%LINK-3-UPDOWN: Interface GigabitEthernet0/{0}, changed state to up",
    "%LINEPROTO-5-UPDOWN: Line protocol on Interface Gi0/{0}, changed state to down",
    "%SEC-6-IPACCESSLOGP: list 101 denied tcp 10.{0}.{1}.{2}(4{3}) -> 192.168.{1}.{2}(443), 1 packet",
    "%SYS-5-CONFIG_I: Configured from console by admin on vty{0} (10.{1}.{2}.{3})",
    "%BGP-5-ADJCHANGE: neighbor 10.{0}.{1}.{2} Down BGP Notification sent",
    "%BGP-5-ADJCHANGE: neighbor 10.{0}.{1}.{2} Up",
    "%OSPF-5-ADJCHG: Process 1, Nbr 10.{0}.{1}.{2} on Vlan{3} from LOADING to FULL",
    "%DHCPD-4-PING_CONFLICT: DHCP address conflict for 192.168.{1}.{2}",
    "sshd[{3}]: Failed password for invalid user admin from 203.0.{1}.{2} port 5{3} ssh2",
    "sshd[{3}]: Accepted publickey for deploy from 10.{0}.{1}.{2} port 4{3} ssh2",
    "kernel: TCP: request_sock_TCP: Possible SYN flooding on port {3}. Sending cookies",
    "systemd[1]: session-{3}.scope: Succeeded",
    "postgres[{3}]: connection timeout expired for database appdb user svc_api",
    "nginx: 10.{0}.{1}.{2} - - \"GET /api/v1/orders/{3} HTTP/1.1\" 500 1{3} 0.{0}12",
    "nginx: 10.{0}.{1}.{2} - - \"POST /api/v1/login HTTP/1.1\" 200 4{3} 0.0{0}4",
    "firewalld: DENY IN=eth0 OUT= SRC=203.0.{1}.{2} DST=10.{0}.{1}.{2} PROTO=TCP DPT=3389",
    "dockerd: container {3}f{0}a exited with code 137 out of memory",
    "kubelet: Readiness probe failed for pod api-{3}: connection timeout after 3s",
    "snmpd: Connection from UDP: [10.{0}.{1}.{2}]:16{3}->[10.0.0.1]:161 community mismatch",
];

const METRIC_NAMES: &[(&str, &str)] = &[
    ("system.cpu.utilization", "%"),
    ("system.memory.utilization", "%"),
    ("system.filesystem.utilization", "%"),
    ("system.load.1m", "1"),
    ("network.io.receive", "By"),
    ("network.io.transmit", "By"),
    ("network.packets.receive", "1"),
    ("network.packets.transmit", "1"),
    ("network.errors.receive", "1"),
    ("network.errors.transmit", "1"),
    ("network.interface.status", "1"),
    ("system.uptime", "s"),
    ("device.temperature", "Cel"),
    ("device.fan.speed", "1"),
    ("device.power.draw", "W"),
    ("snmp.poll.duration", "ms"),
    ("icmp.rtt", "ms"),
    ("icmp.packet_loss", "%"),
    ("tcp.connect.duration", "ms"),
    ("process.count", "1"),
];

// UUID namespaces. Arbitrary but fixed, so generated IDs are stable across runs and
// query files can hardcode a known resource_id.
const TENANT_NS: u64 = 0x1111_1111_1111_1111;
const RESOURCE_NS: u64 = 0x2222_2222_2222_2222;
const SITE_NS: u64 = 0x3333_3333_3333_3333;

const VENDORS: &[&str] = &["cisco", "mikrotik", "juniper", "fortinet", "linux", "windows"];
const SOURCE_KINDS: &[&str] = &["syslog", "snmp", "otlp", "trap"];
const SEVERITIES: &[&str] = &[
    "info", "info", "info", "info", "info", "info", "info", // ~58% info: real syslog is info-heavy
    "notice", "notice", "debug", "warn", "warn", "error", "critical",
];

// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    #[inline]
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    #[inline]
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Deterministic UUID from a namespace + index. Not a real UUIDv4; stable and unique,
/// which is all the benchmark needs, and it lets query files hardcode known IDs.
fn uuid_at(ns: u64, idx: u64, out: &mut String) {
    let hi = (ns.wrapping_mul(0xD6E8_FEB8_6659_FD93)) ^ idx.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let lo = idx
        .wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        .wrapping_add(ns.rotate_left(17));
    let b = [hi.to_be_bytes(), lo.to_be_bytes()].concat();
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, byte) in b.iter().enumerate() {
        if i == 4 || i == 6 || i == 8 || i == 10 {
            out.push('-');
        }
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
}

/// days → (y, m, d). Howard Hinnant's civil_from_days.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// epoch millis → "YYYY-MM-DD HH:MM:SS.mmm"
fn fmt_ts(ms: i64, out: &mut String) {
    let (secs, milli) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, mo, d) = civil_from_days(days);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    push_pad(out, y as u64, 4);
    out.push('-');
    push_pad(out, mo as u64, 2);
    out.push('-');
    push_pad(out, d as u64, 2);
    out.push(' ');
    push_pad(out, h as u64, 2);
    out.push(':');
    push_pad(out, mi as u64, 2);
    out.push(':');
    push_pad(out, s as u64, 2);
    out.push('.');
    push_pad(out, milli as u64, 3);
}

fn push_pad(out: &mut String, v: u64, width: usize) {
    let mut buf = [0u8; 20];
    let mut n = 0;
    let mut v = v;
    if v == 0 {
        buf[0] = b'0';
        n = 1;
    }
    while v > 0 {
        buf[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    for _ in n..width {
        out.push('0');
    }
    for i in (0..n).rev() {
        out.push(buf[i] as char);
    }
}

fn push_u64(out: &mut String, v: u64) {
    push_pad(out, v, 0);
}

/// Fixed 4-decimal float without allocating (avoids `format!` per row).
fn push_fixed4(out: &mut String, v: f64) {
    let neg = v < 0.0;
    if neg {
        out.push('-');
    }
    let scaled = (v.abs() * 10_000.0).round() as u64;
    push_u64(out, scaled / 10_000);
    out.push('.');
    push_pad(out, scaled % 10_000, 4);
}

/// TSV field escaping: tab, newline, carriage return, backslash.
fn push_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
}

/// ClickHouse TSV representation of Map(String, String): {'k':'v','k2':'v2'}
/// Inner strings are single-quoted with backslash escapes.
///
/// Written as push-style helpers rather than taking a slice of pairs so that map
/// values can be built in place. Formatting each value into a temporary String per
/// row costs ~3 allocations/row, which at 1B rows is the difference between minutes
/// and hours of generation time.
fn map_open(out: &mut String) {
    out.push('{');
}
fn map_close(out: &mut String) {
    out.push('}');
}
/// Begin an entry: emits `,` separator if needed, then `'key':'`.
/// Caller pushes the value, then calls `map_val_end`.
fn map_key(out: &mut String, first: &mut bool, k: &str) {
    if !*first {
        out.push(',');
    }
    *first = false;
    out.push('\'');
    push_map_str(out, k);
    out.push_str("':'");
}
fn map_val_end(out: &mut String) {
    out.push('\'');
}
/// Convenience for a literal string value needing no escaping.
fn map_entry(out: &mut String, first: &mut bool, k: &str, v: &str) {
    map_key(out, first, k);
    push_map_str(out, v);
    map_val_end(out);
}

fn push_map_str(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

/// Expand a template, substituting {0}..{3} with per-row varying small integers.
fn expand_template(out: &mut String, tpl: &str, v: [u64; 4]) {
    let b = tpl.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'{' && i + 2 < b.len() && b[i + 2] == b'}' && b[i + 1].is_ascii_digit() {
            let idx = (b[i + 1] - b'0') as usize;
            push_u64(out, v[idx.min(3)]);
            i += 3;
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
}

// ---------------------------------------------------------------------------

struct Args {
    signal: String,
    rows: u64,
    resources: u64,
    tenants: u64,
    metrics: usize,
    days: u64,
    seed: u64,
    start_ms: i64,
}

fn parse_args() -> Args {
    let mut a = Args {
        signal: String::new(),
        rows: 1_000_000,
        resources: 5_000,
        tenants: 3,
        metrics: METRIC_NAMES.len(),
        days: 7,
        seed: 42,
        // 2026-09-01 00:00:00 UTC
        start_ms: 1_787_270_400_000,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        eprintln!("usage: uops-bench-gen <logs|metrics> [--rows N] [--resources N] [--tenants N] [--metrics N] [--days N] [--seed N]");
        std::process::exit(2);
    }
    a.signal = argv[0].clone();
    let mut i = 1;
    while i + 1 < argv.len() + 1 && i < argv.len() {
        let key = argv[i].as_str();
        let val = argv.get(i + 1).map(|s| s.as_str()).unwrap_or("");
        let n: u64 = val.parse().unwrap_or_else(|_| {
            eprintln!("bad value for {key}: {val}");
            std::process::exit(2)
        });
        match key {
            "--rows" => a.rows = n,
            "--resources" => a.resources = n.max(1),
            "--tenants" => a.tenants = n.max(1),
            "--metrics" => a.metrics = (n as usize).clamp(1, METRIC_NAMES.len()),
            "--days" => a.days = n.max(1),
            "--seed" => a.seed = n,
            other => {
                eprintln!("unknown flag {other}");
                std::process::exit(2)
            }
        }
        i += 2;
    }
    a
}

fn main() {
    let a = parse_args();
    let stdout = io::stdout();
    let mut w = BufWriter::with_capacity(8 << 20, stdout.lock());
    match a.signal.as_str() {
        "logs" => gen_logs(&a, &mut w),
        "metrics" => gen_metrics(&a, &mut w),
        other => {
            eprintln!("unknown signal '{other}' (expected 'logs' or 'metrics')");
            std::process::exit(2);
        }
    }
    w.flush().expect("flush");
}

fn gen_logs<W: Write>(a: &Args, w: &mut W) {
    let mut rng = Rng::new(a.seed);
    let mut line = String::with_capacity(1024);
    let mut body = String::with_capacity(256);
    let window_ms = (a.days * 86_400_000) as i64;
    // Time advances monotonically across the run (real arrival order), with jitter
    // so rows within a millisecond bucket are not artificially ordered.
    let step = (window_ms as f64 / a.rows as f64).max(0.001);

    for i in 0..a.rows {
        line.clear();
        let t = a.tenants;
        let tenant = i % t;
        let res = rng.below(a.resources);
        let site = res % 12;
        let observed = a.start_ms + (i as f64 * step) as i64 + rng.below(250) as i64;
        // ingest lag: mostly sub-second, occasionally seconds (realistic, and it is
        // why the envelope carries both observed_at and ingested_at).
        let lag = if rng.below(100) == 0 { rng.below(9_000) + 1_000 } else { rng.below(900) };

        uuid_at(TENANT_NS, tenant, &mut line);
        line.push('\t');
        uuid_at(RESOURCE_NS, res, &mut line);
        line.push('\t');
        uuid_at(SITE_NS, site, &mut line);
        line.push('\t');
        fmt_ts(observed, &mut line);
        line.push('\t');
        fmt_ts(observed + lag as i64, &mut line);
        line.push('\t');
        line.push_str(SOURCE_KINDS[(res % SOURCE_KINDS.len() as u64) as usize]);
        line.push('\t');
        let vendor = VENDORS[(res % VENDORS.len() as u64) as usize];
        line.push_str(vendor);
        line.push('\t');
        line.push_str(SEVERITIES[(rng.below(SEVERITIES.len() as u64)) as usize]);
        line.push('\t');
        push_u64(&mut line, rng.below(24));
        line.push('\t');

        // body — built into a reused buffer, then escaped into the line
        let tpl = LOG_TEMPLATES[(rng.below(LOG_TEMPLATES.len() as u64)) as usize];
        let vals = [
            rng.below(250) + 1,
            rng.below(250) + 1,
            rng.below(250) + 1,
            rng.below(9000) + 1000,
        ];
        body.clear();
        expand_template(&mut body, tpl, vals);
        for (needle, period) in NEEDLES {
            if i % period == 0 {
                body.push(' ');
                body.push_str(needle);
            }
        }
        push_escaped(&mut line, &body);
        line.push('\t');

        // attributes
        let mut first = true;
        map_open(&mut line);
        map_key(&mut line, &mut first, "host.name");
        line.push_str("dev-");
        push_pad(&mut line, res, 5);
        map_val_end(&mut line);
        map_entry(
            &mut line,
            &mut first,
            "network.protocol.name",
            if res % 3 == 0 { "tcp" } else { "udp" },
        );
        map_entry(
            &mut line,
            &mut first,
            "service.name",
            if res % 5 == 0 { "edge" } else { "core" },
        );
        map_close(&mut line);

        line.push('\t'); // trace_id (empty)
        line.push('\t'); // span_id  (empty)
        line.push('\n');

        w.write_all(line.as_bytes()).expect("write");
    }
}

fn gen_metrics<W: Write>(a: &Args, w: &mut W) {
    let mut rng = Rng::new(a.seed ^ 0x4D45_5452_4943_5300);
    let mut line = String::with_capacity(512);
    let window_ms = (a.days * 86_400_000) as i64;
    let step = (window_ms as f64 / a.rows as f64).max(0.001);

    for i in 0..a.rows {
        line.clear();
        let tenant = i % a.tenants;
        let res = rng.below(a.resources);
        let site = res % 12;
        let m = (i as usize) % a.metrics;
        let (name, unit) = METRIC_NAMES[m];
        let observed = a.start_ms + (i as f64 * step) as i64;

        uuid_at(TENANT_NS, tenant, &mut line);
        line.push('\t');
        uuid_at(RESOURCE_NS, res, &mut line);
        line.push('\t');
        uuid_at(SITE_NS, site, &mut line);
        line.push('\t');
        line.push_str(name);
        line.push('\t');
        fmt_ts(observed, &mut line);
        line.push('\t');
        fmt_ts(observed + rng.below(500) as i64, &mut line);
        line.push('\t');
        // A slow per-resource walk rather than uniform noise, so rollups and trend
        // queries operate on data with real structure. Uniform random values compress
        // differently and would misreport the compression ratio.
        let v = ((res.wrapping_mul(7919) % 1000) as f64 / 10.0)
            + ((i % 1440) as f64 / 24.0)
            + (rng.below(100) as f64 / 50.0);
        push_fixed4(&mut line, v);
        line.push('\t');
        line.push_str(unit);
        line.push('\t');
        // interface label drives cardinality: 8 interfaces per device
        let mut first = true;
        map_open(&mut line);
        map_key(&mut line, &mut first, "interface");
        line.push_str("Gi0/");
        push_u64(&mut line, res % 8);
        map_val_end(&mut line);
        map_key(&mut line, &mut first, "host.name");
        line.push_str("dev-");
        push_pad(&mut line, res, 5);
        map_val_end(&mut line);
        map_close(&mut line);

        line.push('\n');
        w.write_all(line.as_bytes()).expect("write");
    }
}
