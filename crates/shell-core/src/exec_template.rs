//! Argument templates of the `exec` permission.
//!
//! An argument is either a literal or contains exactly one placeholder
//! `{name:type[:constraint]}` with an optional literal prefix and suffix:
//!
//! - `{count:int:1..20}`: an integer in the range (inclusive);
//! - `{host:hostname}`: a DNS name;
//! - `{addr:ip}`: an IPv4/IPv6 address;
//! - `{target:host}`: a DNS name or an IP address;
//! - `{proto:enum:tcp|udp}`: one of the listed values.
//!
//! Values are substituted without shell interpretation and cannot start with `-`
//! (except negative numbers explicitly allowed by the range), to prevent
//! option injection.

use std::collections::BTreeMap;
use std::net::IpAddr;

use anyhow::{Context, Result, bail, ensure};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueType {
    Int { min: i64, max: i64 },
    Hostname,
    Ip,
    Host,
    Enum(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgPart {
    Literal(String),
    Placeholder {
        prefix: String,
        name: String,
        ty: ValueType,
        suffix: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pub args: Vec<ArgPart>,
}

impl Template {
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut parts = Vec::with_capacity(args.len());
        let mut names = Vec::new();
        for arg in args {
            let part = parse_arg(arg).with_context(|| format!("argument {arg:?}"))?;
            if let ArgPart::Placeholder { name, .. } = &part {
                ensure!(!names.contains(name), "placeholder {name:?} is repeated");
                names.push(name.clone());
            }
            parts.push(part);
        }
        Ok(Template { args: parts })
    }

    pub fn placeholders(&self) -> impl Iterator<Item = (&str, &ValueType)> {
        self.args.iter().filter_map(|p| match p {
            ArgPart::Placeholder { name, ty, .. } => Some((name.as_str(), ty)),
            ArgPart::Literal(_) => None,
        })
    }

    /// Substitutes values. All placeholders are required; extra values are an error.
    pub fn render(&self, values: &[(String, String)]) -> Result<Vec<String>> {
        let mut map = BTreeMap::new();
        for (k, v) in values {
            ensure!(map.insert(k.as_str(), v.as_str()).is_none(), "value {k:?} given twice");
        }
        let mut out = Vec::with_capacity(self.args.len());
        for part in &self.args {
            match part {
                ArgPart::Literal(s) => out.push(s.clone()),
                ArgPart::Placeholder { prefix, name, ty, suffix } => {
                    let value = map
                        .remove(name.as_str())
                        .with_context(|| format!("value {name:?} not given"))?;
                    validate(ty, value).with_context(|| format!("value {name:?}"))?;
                    out.push(format!("{prefix}{value}{suffix}"));
                }
            }
        }
        if let Some(extra) = map.keys().next() {
            bail!("unknown parameter {extra:?}");
        }
        Ok(out)
    }
}

fn parse_arg(arg: &str) -> Result<ArgPart> {
    let Some(open) = arg.find('{') else {
        ensure!(!arg.contains('}'), "unpaired '}}'");
        return Ok(ArgPart::Literal(arg.to_string()));
    };
    let close = arg[open..].find('}').context("unclosed '{'")? + open;
    let (prefix, suffix) = (&arg[..open], &arg[close + 1..]);
    ensure!(
        !prefix.contains('}') && !suffix.contains(['{', '}']),
        "only one placeholder per argument is allowed"
    );
    let spec = &arg[open + 1..close];
    let mut it = spec.splitn(3, ':');
    let name = it.next().unwrap_or_default();
    ensure!(
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "invalid placeholder name {name:?}"
    );
    let ty = match (it.next(), it.next()) {
        (Some("int"), range) => {
            let (min, max) = match range {
                None => (0, i64::MAX),
                Some(r) => {
                    let (a, b) = r.split_once("..").context("an int range is written as a..b")?;
                    (a.parse()?, b.parse()?)
                }
            };
            ensure!(min <= max, "empty range {min}..{max}");
            ValueType::Int { min, max }
        }
        (Some("hostname"), None) => ValueType::Hostname,
        (Some("ip"), None) => ValueType::Ip,
        (Some("host"), None) => ValueType::Host,
        (Some("enum"), Some(list)) => {
            let items: Vec<String> = list.split('|').map(str::to_string).collect();
            ensure!(
                items.iter().all(|i| !i.is_empty() && !i.starts_with('-')),
                "invalid enum values"
            );
            ValueType::Enum(items)
        }
        (Some(t), _) => bail!("unknown type {t:?}"),
        (None, _) => bail!("placeholder {name:?} has no type"),
    };
    Ok(ArgPart::Placeholder {
        prefix: prefix.to_string(),
        name: name.to_string(),
        ty,
        suffix: suffix.to_string(),
    })
}

fn validate(ty: &ValueType, value: &str) -> Result<()> {
    match ty {
        ValueType::Int { min, max } => {
            let n: i64 = value.parse().context("expected an integer")?;
            ensure!((*min..=*max).contains(&n), "must be in the range {min}..{max}");
            ensure!(n.to_string() == value, "non-canonical number");
        }
        ValueType::Hostname => ensure!(is_hostname(value), "expected a DNS name"),
        ValueType::Ip => {
            value.parse::<IpAddr>().context("expected an IP address")?;
        }
        ValueType::Host => ensure!(
            is_hostname(value) || value.parse::<IpAddr>().is_ok(),
            "expected a DNS name or an IP address"
        ),
        ValueType::Enum(items) => ensure!(
            items.iter().any(|i| i == value),
            "allowed values: {}",
            items.join(", ")
        ),
    }
    Ok(())
}

pub fn is_hostname(s: &str) -> bool {
    let s = s.strip_suffix('.').unwrap_or(s);
    !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tpl(args: &[&str]) -> Template {
        Template::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn renders_ping() {
        let t = tpl(&["-c", "{count:int:1..20}", "{host:hostname}"]);
        let out = t.render(&kv(&[("count", "4"), ("host", "example.com")])).unwrap();
        assert_eq!(out, ["-c", "4", "example.com"]);
    }

    #[test]
    fn rejects_option_injection() {
        let t = tpl(&["{host:host}"]);
        assert!(t.render(&kv(&[("host", "-oProxyCommand=x")])).is_err());
        assert!(t.render(&kv(&[("host", "a b")])).is_err());
        assert!(t.render(&kv(&[("host", "a;rm")])).is_err());
        assert!(t.render(&kv(&[("host", "::1")])).is_ok());
    }

    #[test]
    fn checks_int_range_and_form() {
        let t = tpl(&["{n:int:1..20}"]);
        assert!(t.render(&kv(&[("n", "21")])).is_err());
        assert!(t.render(&kv(&[("n", "05")])).is_err());
        assert!(t.render(&kv(&[("n", "+5")])).is_err());
        assert!(t.render(&kv(&[("n", "20")])).is_ok());
    }

    #[test]
    fn prefix_suffix_and_enum() {
        let t = tpl(&["--proto={p:enum:tcp|udp}"]);
        assert_eq!(t.render(&kv(&[("p", "udp")])).unwrap(), ["--proto=udp"]);
        assert!(t.render(&kv(&[("p", "icmp")])).is_err());
    }

    #[test]
    fn missing_and_extra_values() {
        let t = tpl(&["{host:hostname}"]);
        assert!(t.render(&[]).is_err());
        assert!(t.render(&kv(&[("host", "a"), ("x", "1")])).is_err());
    }

    #[test]
    fn bad_templates() {
        for bad in ["{x}", "{x:foo}", "{a:int}{b:int}", "{x:int:5..1}", "{:int}", "x}"] {
            assert!(Template::parse(&[bad.to_string()]).is_err(), "{bad}");
        }
    }
}
