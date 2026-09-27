//! Runtime debug/admin flags shared by the server, the client admin panel and
//! scriptable smoke tests.
//!
//! One registry — the [`AdminFlags`] resource — is the only surface a new
//! toggle touches. Producers [`register`](AdminFlags::register) a name once;
//! every consumer (in-game panel, `--set` CLI flags, stdin commands on the
//! server, scripted harnesses) writes through the same `name=value` command
//! language in [`AdminFlags::apply`]. Flags registered [`const`](AdminFlags::register_const)
//! are startup-bound and reject `set` at runtime.
use bevy_ecs::prelude::Resource;
use std::collections::BTreeMap;

/// A single admin flag value.
#[derive(Clone, Debug, PartialEq)]
pub enum FlagValue {
    Bool(bool),
    Float(f64),
    Text(String),
}

impl FlagValue {
    /// Parse `raw` as this flag's kind. Booleans accept 0/1; floats accept
    /// finite decimals; text accepts anything.
    fn parse_like(&self, raw: &str) -> Result<Self, String> {
        match self {
            FlagValue::Bool(_) => parse_bool(raw).map(FlagValue::Bool),
            FlagValue::Float(_) => parse_float(raw).map(FlagValue::Float),
            FlagValue::Text(_) => Ok(FlagValue::Text(raw.to_string())),
        }
    }
}

impl std::fmt::Display for FlagValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FlagValue::Bool(v) => write!(f, "{v}"),
            FlagValue::Float(v) => write!(f, "{v}"),
            FlagValue::Text(v) => write!(f, "{v}"),
        }
    }
}

fn parse_bool(raw: &str) -> Result<bool, String> {
    match raw.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Ok(true),
        "false" | "off" | "no" | "0" => Ok(false),
        _ => Err(format!("expected boolean, got '{raw}'")),
    }
}

fn parse_float(raw: &str) -> Result<f64, String> {
    let value: f64 = raw
        .parse()
        .map_err(|_| format!("expected number, got '{raw}'"))?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(format!("expected finite number, got '{raw}'"))
    }
}

struct Flag {
    value: FlagValue,
    /// `false` for startup-bound flags shown read-only everywhere.
    mutable: bool,
    help: String,
    /// Bumped on every successful mutation; watchers compare revisions instead
    /// of polling value diffs.
    revision: u64,
}

/// Read-only view of one flag for renderers and command output.
pub struct FlagView<'a> {
    pub name: &'a str,
    pub value: &'a FlagValue,
    pub mutable: bool,
    pub help: &'a str,
    pub revision: u64,
}

/// Registry of named runtime flags. One instance lives in each app (client,
/// server, headless harness) as a Bevy resource.
#[derive(Resource, Default)]
pub struct AdminFlags {
    flags: BTreeMap<String, Flag>,
    /// Values supplied before registration (CLI `--set`). `register` claims
    /// and coerces them so ordering never matters.
    pending: BTreeMap<String, String>,
}

impl AdminFlags {
    /// Register a mutable flag; returns its revision. A pending CLI value for
    /// `name` is applied now, coerced to the flag's kind.
    pub fn register(&mut self, name: &str, value: FlagValue, help: &str) -> u64 {
        self.register_inner(name, value, help, true)
    }

    /// Register a startup-bound flag: visible in `list` and the panel but `set`
    /// is rejected at runtime.
    pub fn register_const(&mut self, name: &str, value: FlagValue, help: &str) -> u64 {
        self.register_inner(name, value, help, false)
    }

    fn register_inner(
        &mut self,
        name: &str,
        value: FlagValue,
        help: &str,
        mutable: bool,
    ) -> u64 {
        let pending = self.pending.remove(name);
        let flag = self.flags.entry(name.to_string()).or_insert_with(|| Flag {
            value,
            mutable,
            help: help.to_string(),
            revision: 0,
        });
        if let Some(raw) = pending {
            match flag.value.parse_like(&raw) {
                Ok(value) => {
                    if flag.value != value {
                        flag.value = value;
                        flag.revision += 1;
                    }
                }
                Err(error) => {
                    eprintln!("admin: ignoring --set {name}={raw}: {error}");
                }
            }
        }
        flag.revision
    }

    /// Supply a value before registration; the flag claims it at `register`.
    /// Already-registered names go through normal `set` validation.
    pub fn preset(&mut self, assignment: &str) -> Result<(), String> {
        let (name, raw) = assignment
            .split_once('=')
            .ok_or_else(|| format!("expected name=value, got '{assignment}'"))?;
        if name.is_empty() {
            return Err("empty flag name".into());
        }
        if self.flags.contains_key(name) {
            return self.set(name, raw).map(|_| ());
        }
        self.pending.insert(name.to_string(), raw.to_string());
        Ok(())
    }

    /// Set a registered flag from a raw string; returns the new revision.
    pub fn set(&mut self, name: &str, raw: &str) -> Result<u64, String> {
        let flag = self
            .flags
            .get_mut(name)
            .ok_or_else(|| format!("unknown flag '{name}'"))?;
        if !flag.mutable {
            return Err(format!("'{name}' is startup-bound (read-only)"));
        }
        let value = flag.value.parse_like(raw)?;
        if flag.value != value {
            flag.value = value;
            flag.revision += 1;
        }
        Ok(flag.revision)
    }

    /// Toggle a boolean flag.
    pub fn toggle(&mut self, name: &str) -> Result<u64, String> {
        let flag = self
            .flags
            .get(name)
            .ok_or_else(|| format!("unknown flag '{name}'"))?;
        let FlagValue::Bool(v) = flag.value else {
            return Err(format!("'{name}' is not a boolean flag"));
        };
        self.set(name, if v { "false" } else { "true" })
    }

    /// Run one command line (`set`, `toggle`, `get`, `list`, `help`, or a bare
    /// `name=value`). Returns the human-readable result line.
    pub fn apply(&mut self, line: &str) -> Result<String, String> {
        let line = line.trim();
        if line.is_empty() {
            return Err("empty command".into());
        }
        let mut parts = line.split_whitespace();
        match parts.next().unwrap() {
            "set" => {
                let name = parts
                    .next()
                    .ok_or_else(|| "usage: set <name> <value>".to_string())?;
                // Values may contain spaces (text flags): keep the remainder.
                let raw = line
                    .split_once(name)
                    .map(|(_, rest)| rest.trim_start())
                    .unwrap_or_default();
                if raw.is_empty() {
                    return Err("usage: set <name> <value>".into());
                }
                self.set(name, raw)
                    .map(|_| format!("{name} = {}", self.flags[name].value))
            }
            "toggle" => {
                let name = parts
                    .next()
                    .ok_or_else(|| "usage: toggle <name>".to_string())?;
                self.toggle(name)
                    .map(|_| format!("{name} = {}", self.flags[name].value))
            }
            "get" => {
                let name = parts
                    .next()
                    .ok_or_else(|| "usage: get <name>".to_string())?;
                let flag = self
                    .flags
                    .get(name)
                    .ok_or_else(|| format!("unknown flag '{name}'"))?;
                Ok(format!("{name} = {}", flag.value))
            }
            "list" | "ls" => Ok(self
                .list()
                .map(|v| {
                    format!(
                        "{} = {}{}",
                        v.name,
                        v.value,
                        if v.mutable { "" } else { " (const)" }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")),
            "help" | "?" => Ok("commands: list | get <name> | set <name> <value> | toggle <name> | <name>=<value>".into()),
            _ => {
                // Bare `name=value` assignment keeps CLI-style calls working.
                if let Some((name, raw)) = line.split_once('=') {
                    self.set(name.trim(), raw.trim())
                        .map(|_| format!("{name} = {}", self.flags[name.trim()].value))
                } else {
                    // Treat a lone word as `get` for convenience.
                    self.apply(&format!("get {line}"))
                }
            }
        }
    }

    /// Ordered view over every registered flag.
    pub fn list(&self) -> impl Iterator<Item = FlagView<'_>> {
        self.flags.iter().map(|(name, flag)| FlagView {
            name,
            value: &flag.value,
            mutable: flag.mutable,
            help: &flag.help,
            revision: flag.revision,
        })
    }

    pub fn flag<'a>(&'a self, name: &'a str) -> Option<FlagView<'a>> {
        self.flags.get(name).map(|flag| FlagView {
            name,
            value: &flag.value,
            mutable: flag.mutable,
            help: &flag.help,
            revision: flag.revision,
        })
    }

    /// Revision of `name`, or `None` when unregistered.
    pub fn revision(&self, name: &str) -> Option<u64> {
        self.flags.get(name).map(|f| f.revision)
    }

    /// Current boolean; panics when `name` is unregistered or non-bool — a
    /// caller-side bug, not user input.
    pub fn bool(&self, name: &str) -> bool {
        match self.flag(name).map(|v| v.value) {
            Some(FlagValue::Bool(v)) => *v,
            _ => panic!("admin flag '{name}' is not a registered bool"),
        }
    }

    /// Current float; panics like [`bool`](Self::bool).
    pub fn float(&self, name: &str) -> f64 {
        match self.flag(name).map(|v| v.value) {
            Some(FlagValue::Float(v)) => *v,
            _ => panic!("admin flag '{name}' is not a registered float"),
        }
    }

    /// Drain `--set` assignments that no registration claimed, for a one-time
    /// "unknown flag" warning at startup.
    pub fn take_unclaimed(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending).into_keys().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_toggle_roundtrip() {
        let mut flags = AdminFlags::default();
        flags.register("noclip", FlagValue::Bool(false), "fly");
        flags.register("day_length", FlagValue::Float(1200.0), "seconds/day");
        assert!(!flags.bool("noclip"));
        flags.set("noclip", "true").unwrap();
        assert!(flags.bool("noclip"));
        flags.toggle("noclip").unwrap();
        assert!(!flags.bool("noclip"));
        flags.set("day_length", "0").unwrap();
        assert_eq!(flags.float("day_length"), 0.0);
        assert!(flags.set("missing", "1").is_err());
        assert!(flags.set("noclip", "banana").is_err());
        assert!(flags.toggle("day_length").is_err());
    }

    #[test]
    fn preset_applies_at_registration() {
        let mut flags = AdminFlags::default();
        flags.preset("spawn_titan=false").unwrap();
        flags.preset("metrics_every=30").unwrap();
        flags.register("spawn_titan", FlagValue::Bool(true), "");
        flags.register("metrics_every", FlagValue::Float(600.0), "");
        assert!(!flags.bool("spawn_titan"));
        assert_eq!(flags.float("metrics_every"), 30.0);
        assert!(flags.take_unclaimed().is_empty());
    }

    #[test]
    fn unclaimed_preset_reports() {
        let mut flags = AdminFlags::default();
        flags.preset("typo_flag=1").unwrap();
        assert_eq!(flags.take_unclaimed(), vec!["typo_flag".to_string()]);
    }

    #[test]
    fn const_flag_rejects_set() {
        let mut flags = AdminFlags::default();
        flags.register_const("gpu_physics", FlagValue::Bool(false), "");
        assert!(flags.set("gpu_physics", "true").is_err());
        assert_eq!(flags.apply("get gpu_physics").unwrap(), "gpu_physics = false");
    }

    #[test]
    fn apply_commands() {
        let mut flags = AdminFlags::default();
        flags.register("noclip", FlagValue::Bool(false), "fly");
        flags.register("title", FlagValue::Text("hi".into()), "label");
        assert_eq!(flags.apply("noclip").unwrap(), "noclip = false");
        assert_eq!(flags.apply("set noclip true").unwrap(), "noclip = true");
        assert!(flags.bool("noclip"));
        assert_eq!(flags.apply("noclip=false").unwrap(), "noclip = false");
        assert_eq!(flags.apply("toggle noclip").unwrap(), "noclip = true");
        flags.apply("set title hello world").unwrap();
        assert_eq!(flags.apply("get title").unwrap(), "title = hello world");
        assert!(flags.apply("list").unwrap().contains("noclip = true"));
        assert!(flags.apply("list").unwrap().contains("title = hello world"));
        assert!(flags.apply("bogus xyz").is_err());
    }

    #[test]
    fn revisions_track_mutations() {
        let mut flags = AdminFlags::default();
        let rev = flags.register("a", FlagValue::Bool(false), "");
        assert_eq!(rev, flags.revision("a").unwrap());
        let next = flags.set("a", "true").unwrap();
        assert!(next > rev);
        // Same value: no bump.
        assert_eq!(flags.set("a", "true").unwrap(), next);
        assert_eq!(flags.revision("missing"), None);
    }
}
