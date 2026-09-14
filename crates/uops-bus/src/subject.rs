//! Subjects and subscription patterns.
//!
//! `telemetry.{tenant}.{signal}.{source_kind}` — hierarchical from the first message,
//! because a flat subject is impossible to widen later without rewriting every
//! publisher and subscriber at once.
//!
//! The matching rules here are **NATS subject semantics**, deliberately and exactly:
//! `*` matches one token, `>` matches the rest and may only appear last. `InProcessBus`
//! could have used something simpler — it is a `HashMap` and a channel — but then
//! moving to `JetStream` in v0.2 would change which messages each subscriber receives,
//! which is not a wiring change, it is a redesign of the pipeline. Implementing the
//! target system's semantics now is what makes the swap boring.

use std::fmt;

use uops_core::{SourceKind, TenantId};

use crate::error::{Error, Result};

/// The root token. Present on every subject so that a future NATS deployment can carry
/// non-telemetry traffic on the same server without collisions.
pub const ROOT: &str = "telemetry";

/// Which signal a message carries. Mirrors `uops_core::Signal::kind()`, as a type — the
/// subject is built from this rather than from a free string so a typo is a compile
/// error rather than a subscription that silently receives nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SignalKind {
    Metric,
    Log,
    Event,
    State,
    Trace,
    Flow,
}

impl SignalKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Metric => "metric",
            Self::Log => "log",
            Self::Event => "event",
            Self::State => "state",
            Self::Trace => "trace",
            Self::Flow => "flow",
        }
    }

    /// The discriminant `uops_core::Signal` reports, so a publisher can derive the
    /// subject from the envelope it is holding.
    #[must_use]
    pub fn from_signal(signal: &uops_core::Signal) -> Self {
        match signal {
            uops_core::Signal::Metric(_) => Self::Metric,
            uops_core::Signal::Log(_) => Self::Log,
            uops_core::Signal::Event(_) => Self::Event,
            uops_core::Signal::State(_) => Self::State,
            uops_core::Signal::Trace(_) => Self::Trace,
            uops_core::Signal::Flow(_) => Self::Flow,
        }
    }
}

/// A concrete subject. Never contains a wildcard.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Subject {
    tenant: TenantId,
    signal: SignalKind,
    source: SourceKind,
}

impl Subject {
    #[must_use]
    pub const fn new(tenant: TenantId, signal: SignalKind, source: SourceKind) -> Self {
        Self {
            tenant,
            signal,
            source,
        }
    }

    /// Derive the subject from an envelope. The normal path: a publisher should not be
    /// choosing a subject, it should be reading one off the thing it is publishing.
    #[must_use]
    pub fn of(envelope: &uops_core::TelemetryEnvelope) -> Self {
        Self::new(
            envelope.tenant_id,
            SignalKind::from_signal(&envelope.signal),
            envelope.source.kind,
        )
    }

    #[must_use]
    pub const fn tenant(&self) -> TenantId {
        self.tenant
    }

    #[must_use]
    pub const fn signal(&self) -> SignalKind {
        self.signal
    }

    #[must_use]
    pub const fn source(&self) -> SourceKind {
        self.source
    }

    fn tokens(&self) -> [String; 4] {
        [
            ROOT.to_owned(),
            self.tenant.to_string(),
            self.signal.as_str().to_owned(),
            self.source.as_str().to_owned(),
        ]
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{ROOT}.{}.{}.{}",
            self.tenant,
            self.signal.as_str(),
            self.source.as_str()
        )
    }
}

/// One token of a subscription pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Literal(String),
    /// `*` — exactly one token, any value.
    One,
    /// `>` — every remaining token. Only valid as the last token.
    Rest,
}

/// What a subscriber is listening for.
///
/// Built through named constructors rather than parsed from strings on the hot path,
/// so that "which tenants does this subscription cover" is answerable by reading the
/// call site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubjectPattern {
    tokens: Vec<Token>,
}

impl SubjectPattern {
    /// Everything for one tenant: `telemetry.<tenant>.>`.
    #[must_use]
    pub fn tenant(tenant: TenantId) -> Self {
        Self {
            tokens: vec![
                Token::Literal(ROOT.to_owned()),
                Token::Literal(tenant.to_string()),
                Token::Rest,
            ],
        }
    }

    /// One signal for one tenant, from any source:
    /// `telemetry.<tenant>.<signal>.*`.
    #[must_use]
    pub fn signal(tenant: TenantId, signal: SignalKind) -> Self {
        Self {
            tokens: vec![
                Token::Literal(ROOT.to_owned()),
                Token::Literal(tenant.to_string()),
                Token::Literal(signal.as_str().to_owned()),
                Token::One,
            ],
        }
    }

    /// Exactly one subject and nothing else.
    #[must_use]
    pub fn exact(subject: &Subject) -> Self {
        Self {
            tokens: subject.tokens().into_iter().map(Token::Literal).collect(),
        }
    }

    /// A subscription that spans **every tenant**.
    ///
    /// Deliberately verbose and deliberately greppable. The ingestion pipeline is one
    /// process serving every tenant, so this has to exist — but it is the one shape in
    /// the system where a subscriber sees another customer's telemetry, and the name is
    /// the audit trail. Compare `TenantScope`, which has the same problem and solves it
    /// the same way.
    #[must_use]
    pub fn across_all_tenants(signal: SignalKind) -> Self {
        Self {
            tokens: vec![
                Token::Literal(ROOT.to_owned()),
                Token::One,
                Token::Literal(signal.as_str().to_owned()),
                Token::One,
            ],
        }
    }

    /// Every message on the bus, from every tenant. For the durable writer, which is
    /// the one component that legitimately consumes all of it.
    #[must_use]
    pub fn everything() -> Self {
        Self {
            tokens: vec![Token::Literal(ROOT.to_owned()), Token::Rest],
        }
    }

    /// Parse NATS-style pattern text. For configuration files and tests; the named
    /// constructors are the normal path.
    pub fn parse(text: &str) -> Result<Self> {
        if text.is_empty() {
            return Err(Error::InvalidSubject("a pattern cannot be empty".into()));
        }

        let raw: Vec<&str> = text.split('.').collect();
        let mut tokens = Vec::with_capacity(raw.len());

        for (i, token) in raw.iter().enumerate() {
            let last = i + 1 == raw.len();
            match *token {
                "" => {
                    return Err(Error::InvalidSubject(format!(
                        "empty token in {text:?}: tokens are separated by single dots"
                    )));
                }
                "*" => tokens.push(Token::One),
                ">" if last => tokens.push(Token::Rest),
                ">" => {
                    return Err(Error::InvalidSubject(format!(
                        "`>` matches everything after it, so it must be the last token in {text:?}"
                    )));
                }
                literal if literal.contains(['*', '>']) => {
                    return Err(Error::InvalidSubject(format!(
                        "{literal:?} mixes a wildcard into a token; wildcards are whole tokens"
                    )));
                }
                literal => tokens.push(Token::Literal(literal.to_owned())),
            }
        }

        Ok(Self { tokens })
    }

    /// Whether this pattern receives that subject.
    #[must_use]
    pub fn matches(&self, subject: &Subject) -> bool {
        let target = subject.tokens();
        let mut subject_tokens = target.iter();

        for token in &self.tokens {
            match token {
                // `>` consumes everything left, but only if there IS something left:
                // NATS does not let `a.>` match `a`.
                Token::Rest => return subject_tokens.next().is_some(),
                Token::One => {
                    if subject_tokens.next().is_none() {
                        return false;
                    }
                }
                Token::Literal(expected) => match subject_tokens.next() {
                    Some(actual) if actual == expected => {}
                    _ => return false,
                },
            }
        }

        // Without a trailing `>`, the pattern must have consumed the whole subject.
        subject_tokens.next().is_none()
    }
}

impl fmt::Display for SubjectPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rendered: Vec<&str> = self
            .tokens
            .iter()
            .map(|t| match t {
                Token::Literal(s) => s.as_str(),
                Token::One => "*",
                Token::Rest => ">",
            })
            .collect();
        f.write_str(&rendered.join("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uops_core::TenantId;

    fn subject(tenant: TenantId) -> Subject {
        Subject::new(tenant, SignalKind::Log, SourceKind::Syslog)
    }

    #[test]
    fn a_subject_renders_the_documented_hierarchy() {
        let t = TenantId::new();
        let s = Subject::new(t, SignalKind::Metric, SourceKind::Snmp);
        assert_eq!(s.to_string(), format!("telemetry.{t}.metric.snmp"));
    }

    #[test]
    fn a_tenant_pattern_receives_that_tenant_and_no_other() {
        // The property the whole subject scheme exists for. A subscriber built for one
        // tenant must not see another's telemetry, and in an MSP deployment every
        // tenant's pipeline is a subscriber on the same bus.
        let mine = TenantId::new();
        let theirs = TenantId::new();
        let p = SubjectPattern::tenant(mine);

        assert!(p.matches(&subject(mine)));
        assert!(!p.matches(&subject(theirs)));
        assert!(p.matches(&Subject::new(mine, SignalKind::Flow, SourceKind::Otlp)));
    }

    #[test]
    fn a_signal_pattern_narrows_to_one_signal() {
        let t = TenantId::new();
        let p = SubjectPattern::signal(t, SignalKind::Log);

        assert!(p.matches(&Subject::new(t, SignalKind::Log, SourceKind::Syslog)));
        assert!(p.matches(&Subject::new(t, SignalKind::Log, SourceKind::Otlp)));
        assert!(!p.matches(&Subject::new(t, SignalKind::Metric, SourceKind::Snmp)));
    }

    #[test]
    fn crossing_tenants_requires_the_constructor_that_says_so() {
        let a = TenantId::new();
        let b = TenantId::new();
        let p = SubjectPattern::across_all_tenants(SignalKind::Log);

        assert!(p.matches(&subject(a)) && p.matches(&subject(b)));
        assert!(!p.matches(&Subject::new(a, SignalKind::Metric, SourceKind::Snmp)));
    }

    #[test]
    fn everything_matches_every_signal_and_every_tenant() {
        let p = SubjectPattern::everything();
        assert!(p.matches(&subject(TenantId::new())));
        assert!(p.matches(&Subject::new(
            TenantId::new(),
            SignalKind::Trace,
            SourceKind::Internal
        )));
    }

    #[test]
    fn exact_matches_one_subject_only() {
        let t = TenantId::new();
        let s = subject(t);
        let p = SubjectPattern::exact(&s);
        assert!(p.matches(&s));
        assert!(!p.matches(&Subject::new(t, SignalKind::Log, SourceKind::Otlp)));
    }

    #[test]
    fn wildcards_follow_nats_semantics_exactly() {
        // These are the cases that decide whether moving to JetStream changes which
        // messages a subscriber receives. They are asserted against the published NATS
        // rules rather than against what is convenient to implement.
        let t = TenantId::new();
        let s = Subject::new(t, SignalKind::Log, SourceKind::Syslog);
        let m = |p: &str| SubjectPattern::parse(p).unwrap().matches(&s);

        assert!(m(&format!("telemetry.{t}.log.syslog")));
        assert!(m("telemetry.*.*.*"));
        assert!(m("telemetry.>"));
        assert!(m(&format!("telemetry.{t}.>")));

        // `*` is exactly one token: it never spans a dot and never matches nothing.
        assert!(!m("telemetry.*"), "one wildcard cannot cover three tokens");
        assert!(!m("telemetry.*.*.*.*"), "the subject has only four tokens");
        assert!(!m("*"), "a single wildcard is not a whole subject");

        // A prefix is not a match without a wildcard to absorb the tail.
        assert!(!m(&format!("telemetry.{t}.log")));
    }

    #[test]
    fn malformed_patterns_are_rejected_with_a_reason() {
        // Silently accepting these produces a subscription that receives nothing, which
        // looks exactly like a quiet network.
        for bad in ["", "telemetry..log", "telemetry.>.log", "telemetry.lo*g"] {
            let err = SubjectPattern::parse(bad).unwrap_err();
            assert!(
                matches!(err, Error::InvalidSubject(_)),
                "{bad:?} should be rejected: {err}"
            );
        }
    }

    #[test]
    fn patterns_round_trip_through_text() {
        let t = TenantId::new();
        for p in [
            SubjectPattern::tenant(t),
            SubjectPattern::signal(t, SignalKind::Metric),
            SubjectPattern::across_all_tenants(SignalKind::Log),
            SubjectPattern::everything(),
        ] {
            let text = p.to_string();
            assert_eq!(
                SubjectPattern::parse(&text).unwrap(),
                p,
                "{text} did not round trip"
            );
        }
    }

    #[test]
    fn the_signal_token_comes_from_the_envelope_not_from_a_string() {
        // A publisher that spells the signal by hand can spell it wrong, and the result
        // is a subscription that quietly receives nothing.
        let log = uops_core::Signal::Log(uops_core::LogRecord {
            body: "x".into(),
            facility: None,
            trace_id: None,
            span_id: None,
        });
        assert_eq!(SignalKind::from_signal(&log), SignalKind::Log);
        assert_eq!(SignalKind::from_signal(&log).as_str(), log.kind());
    }
}
