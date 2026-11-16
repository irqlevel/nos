//! A provider's descriptor checked against what the provider registers. A
//! client builds its views from the descriptor -- its pages, tables, forms
//! and charts -- so one that does not say what the provider has shows a
//! method that is not there, misses one that is, or asks a read for what
//! only an action may do. The bus checks it as the provider registers and
//! refuses a provider whose descriptor does not hold; docs/mgmt.md has the
//! format.
//!
//! What is checked: it is JSON as the bus reads it, within `LIMITS`; it is
//! of `SCHEMA`; it names the provider; its methods are the registered ones,
//! each once and of its kind; every text a person reads is there in each of
//! `LANGUAGES`; every field has a type a client knows, and an enum its
//! values -- and, where it says how a thing is, each value's tone, which a
//! client colours it by; a method's parameter whose values a read gives
//! (`choices`) names a provider, a method and the answer's list and key --
//! the provider's own a read it has; a resource is listed by a read and
//! acted on by what is not one,
//! its columns among its fields; a metric is read by a read. Nothing is
//! there that the format does not have: a misspelt member is refused rather
//! than left for a client to ignore.

use crate::json::{parse, Limits, Value};

/// What a descriptor may make, at most: the bus's `MAX_DESCRIPTOR` of bytes.
pub const LIMITS: Limits = Limits { bytes: 32 * 1024, depth: 12, values: 4096, members: 256, string: 2048 };

/// The version of the format this checks.
pub const SCHEMA: u64 = 1;

/// The languages every text is in.
pub const LANGUAGES: [&str; 2] = ["en", "ru"];

/// What a field may be. Every count says its unit in its type: `bytes`, a
/// `duration_ns` or `duration_ms`, a `time_s` -- seconds since 1970.
pub const TYPES: [&str; 13] = [
    "string", "text", "int", "bool", "enum", "bytes", "duration_ns", "duration_ms", "time_s", "ipv4", "mac", "port",
    "list",
];

/// What kind a method is, by its name on the bus.
pub const KINDS: [&str; 3] = ["read", "action", "job"];

/// How an enum's value says a thing is, where it says one (`tones`): well,
/// a warning, an alarm, bad -- and idle, neither. A client colours a value by
/// its tone: green, yellow, orange, red, grey.
pub const TONES: [&str; 5] = ["good", "warn", "alarm", "bad", "idle"];

/// Why a descriptor was refused: where in it, and what is wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DescriptorError {
    pub at: &'static str,
    pub why: &'static str,
}

fn refuse<T>(at: &'static str, why: &'static str) -> Result<T, DescriptorError> {
    Err(DescriptorError { at, why })
}

/// `text`, the descriptor of the provider `provider` with `methods` -- each
/// a name and its kind's (`KINDS`) -- checked.
pub fn check(text: &[u8], provider: &str, methods: &[(&str, &str)]) -> Result<(), DescriptorError> {
    let Ok(top) = parse(text, &LIMITS) else { return refuse("", "is not JSON as the bus reads it") };
    let Some(members) = top.as_object() else { return refuse("", "is an object") };
    only(members, &["schema", "provider", "title", "help", "ui", "methods", "resources", "metrics"], "")?;

    if top.get("schema").and_then(Value::as_u64) != Some(SCHEMA) {
        return refuse("schema", "is 1");
    }
    if top.get("provider").and_then(Value::as_str) != Some(provider) {
        return refuse("provider", "is the provider's name");
    }
    text_of(top.get("title"), "title")?;
    opt_text(top.get("help"), "help")?;
    if let Some(ui) = top.get("ui") {
        check_ui(ui)?;
    }

    let Some(described) = top.get("methods").and_then(Value::as_object) else {
        return refuse("methods", "is an object")
    };
    if described.len() != methods.len() {
        return refuse("methods", "are the methods the provider registers");
    }
    let own = Own { provider, methods };
    for (name, kind) in methods {
        let Some(m) = top.get("methods").and_then(|v| v.get(name)) else {
            return refuse("methods", "describe every method the provider registers")
        };
        check_method(m, kind, &own)?;
    }

    if let Some(resources) = top.get("resources") {
        let Some(resources) = resources.as_object() else { return refuse("resources", "is an object") };
        for (name, r) in resources {
            name_ok(name, "resources")?;
            check_resource(r, methods)?;
        }
        /* A parent is another resource */
        for (name, r) in resources {
            if let Some(parent) = r.get("parent") {
                let Some(p) = parent.as_str() else { return refuse("resources.parent", "is a string") };
                if p == name || !resources.iter().any(|(n, _)| n == p) {
                    return refuse("resources.parent", "names another resource");
                }
            }
        }
    }

    if let Some(metrics) = top.get("metrics") {
        let Some(metrics) = metrics.as_object() else { return refuse("metrics", "is an object") };
        for (name, m) in metrics {
            name_ok(name, "metrics")?;
            check_metric(m, methods)?;
        }
    }
    Ok(())
}

/// Every member of an object one of `allowed`.
fn only(members: &[(alloc::string::String, Value)], allowed: &[&str], at: &'static str) -> Result<(), DescriptorError> {
    if members.iter().all(|(k, _)| allowed.contains(&k.as_str())) {
        Ok(())
    } else {
        refuse(at, "has a member the format does not")
    }
}

/// A text a person reads: an object with a string, not empty, in each of
/// `LANGUAGES` and nothing else.
fn text_of(v: Option<&Value>, at: &'static str) -> Result<(), DescriptorError> {
    let Some(members) = v.and_then(Value::as_object) else { return refuse(at, "is a text: {\"en\": ..., \"ru\": ...}") };
    if members.len() != LANGUAGES.len() {
        return refuse(at, "is in exactly the languages a text is in");
    }
    for lang in LANGUAGES {
        match v.and_then(|v| v.get(lang)).and_then(Value::as_str) {
            Some(s) if !s.is_empty() => {}
            _ => return refuse(at, "has every language, none of them empty"),
        }
    }
    Ok(())
}

fn opt_text(v: Option<&Value>, at: &'static str) -> Result<(), DescriptorError> {
    match v {
        Some(v) => text_of(Some(v), at),
        None => Ok(()),
    }
}

/// A name in the descriptor: what a method's is, so the client can make an
/// id of it.
fn name_str(v: Option<&Value>, at: &'static str) -> Result<(), DescriptorError> {
    name_ok(v.and_then(Value::as_str).unwrap_or(""), at)
}

fn name_ok(s: &str, at: &'static str) -> Result<(), DescriptorError> {
    if crate::valid_method(s) {
        Ok(())
    } else {
        refuse(at, "is a name: a lowercase letter, then lowercase letters, digits, _ and .")
    }
}

fn check_ui(ui: &Value) -> Result<(), DescriptorError> {
    let Some(members) = ui.as_object() else { return refuse("ui", "is an object") };
    only(members, &["section", "order", "icon"], "ui")?;
    if let Some(section) = ui.get("section") {
        name_str(Some(section), "ui.section")?;
    }
    if let Some(order) = ui.get("order") {
        if !matches!(order, Value::Int(n) if *n >= 0) {
            return refuse("ui.order", "is a count");
        }
    }
    if let Some(icon) = ui.get("icon") {
        name_str(Some(icon), "ui.icon")?;
    }
    Ok(())
}

/// The provider a descriptor is of, and the methods it registers: what a
/// parameter's `choices` of its own are held to.
struct Own<'a> {
    provider: &'a str,
    methods: &'a [(&'a str, &'a str)],
}

fn check_method(m: &Value, kind: &str, own: &Own) -> Result<(), DescriptorError> {
    let Some(members) = m.as_object() else { return refuse("methods", "are objects") };
    only(members, &["kind", "title", "help", "confirm", "confirm_typed", "params", "result"], "methods")?;
    if m.get("kind").and_then(Value::as_str) != Some(kind) {
        return refuse("methods.kind", "is the kind the method is registered as");
    }
    text_of(m.get("title"), "methods.title")?;
    opt_text(m.get("help"), "methods.help")?;
    if let Some(confirm) = m.get("confirm") {
        if kind == "read" {
            return refuse("methods.confirm", "is for what changes something, not a read");
        }
        text_of(Some(confirm), "methods.confirm")?;
    }
    if let Some(typed) = m.get("confirm_typed") {
        /* The parameter whose value the client has typed again before it
         * calls -- a pool's name before it is destroyed: one the call is
         * always given, a string, asked with the confirm */
        let Some(name) = typed.as_str() else { return refuse("methods.confirm_typed", "names a parameter") };
        if m.get("confirm").is_none() {
            return refuse("methods.confirm_typed", "goes with a confirm");
        }
        let field = m.get("params").and_then(|p| p.get(name));
        let typed_ok = field.is_some_and(|f| {
            f.get("type").and_then(Value::as_str) == Some("string") && f.get("required").and_then(Value::as_bool) == Some(true)
        });
        if !typed_ok {
            return refuse("methods.confirm_typed", "names a parameter of the method's that is a string, required");
        }
    }
    if let Some(params) = m.get("params") {
        let Some(params) = params.as_object() else { return refuse("methods.params", "is an object") };
        for (name, field) in params {
            name_ok(name, "methods.params")?;
            check_field(field, "methods.params")?;
            if let Some(choices) = field.get("choices") {
                check_choices(field, choices, own)?;
            }
        }
    }
    if let Some(result) = m.get("result") {
        name_str(Some(result), "methods.result")?;
    }
    Ok(())
}

/// A field: a method's parameter (`methods.params`) or a resource's
/// (`resources.fields`). `choices` is a parameter's alone, and checked by
/// `check_choices`: a resource's field shows a value, it offers none.
fn check_field(f: &Value, at: &'static str) -> Result<(), DescriptorError> {
    let Some(members) = f.as_object() else { return refuse(at, "are fields: objects") };
    let param = at == "methods.params";
    only(members, &["type", "title", "help", "unit", "min", "max", "values", "tones", "required", "default", "of",
                    "choices"], at)?;
    if !param && f.get("choices").is_some() {
        return refuse(at, "have choices only as a method's parameters");
    }
    let Some(ty) = f.get("type").and_then(Value::as_str) else { return refuse(at, "have a type") };
    if !TYPES.contains(&ty) {
        return refuse(at, "have a type the format has");
    }
    text_of(f.get("title"), at)?;
    opt_text(f.get("help"), at)?;
    for bound in ["min", "max"] {
        /* A number written as one: a client compares it as a number */
        if f.get(bound).is_some_and(|b| !matches!(b, Value::Int(_))) {
            return refuse(at, "have bounds that are numbers");
        }
    }
    if let Some(unit) = f.get("unit") {
        if !unit.as_str().is_some_and(|u| !u.is_empty() && u.len() <= 16) {
            return refuse(at, "have a unit of 1 to 16 bytes");
        }
    }
    if let Some(required) = f.get("required") {
        if required.as_bool().is_none() {
            return refuse(at, "have a required that is true or false");
        }
    }
    if let Some(default) = f.get("default") {
        if matches!(default, Value::List(_) | Value::Object(_)) {
            return refuse(at, "have a default that is one value");
        }
    }
    match (ty, f.get("values")) {
        ("enum", Some(values)) => {
            let Some(values) = values.as_object() else { return refuse(at, "have values: an object") };
            if values.is_empty() {
                return refuse(at, "have an enum's values");
            }
            for (_, title) in values {
                text_of(Some(title), at)?;
            }
        }
        ("enum", None) => return refuse(at, "have an enum's values"),
        (_, Some(_)) => return refuse(at, "have values only for an enum"),
        _ => {}
    }
    match (ty, f.get("tones")) {
        ("enum", Some(tones)) => {
            let Some(tones) = tones.as_object() else { return refuse(at, "have tones: an object") };
            let values = f.get("values").and_then(Value::as_object).unwrap_or(&[]);
            for (value, tone) in tones {
                if !values.iter().any(|(v, _)| v == value) {
                    return refuse(at, "have tones of their own values alone");
                }
                if !tone.as_str().is_some_and(|t| TONES.contains(&t)) {
                    return refuse(at, "have tones the format has");
                }
            }
        }
        (_, Some(_)) => return refuse(at, "have tones only for an enum"),
        _ => {}
    }
    match (ty, f.get("of")) {
        ("list", Some(of)) => {
            if !of.as_str().is_some_and(|t| t != "list" && TYPES.contains(&t)) {
                return refuse(at, "have a list of a type the format has");
            }
        }
        ("list", None) => return refuse(at, "say what a list is of"),
        (_, Some(_)) => return refuse(at, "say what they are of only for a list"),
        _ => {}
    }
    Ok(())
}

/// A parameter's `choices`: the values a client offers it, each item's `key`
/// in the list `items` of the answer of `provider`'s read `method` on the
/// same host -- of a string or a count, what an item's key is. Another
/// provider may not be there, and a client takes the value typed then; the
/// descriptor's own provider is, and its method is a read it registers.
fn check_choices(field: &Value, choices: &Value, own: &Own) -> Result<(), DescriptorError> {
    const AT: &str = "methods.params.choices";
    let Some(members) = choices.as_object() else { return refuse(AT, "is an object") };
    only(members, &["provider", "method", "items", "key"], AT)?;
    if !matches!(field.get("type").and_then(Value::as_str), Some("string" | "int")) {
        return refuse(AT, "is for a string or an int: what an item's key is");
    }
    let Some(provider) = choices.get("provider").and_then(Value::as_str).filter(|p| crate::valid_provider(p)) else {
        return refuse(AT, "names a provider: a lowercase letter, then lowercase letters, digits and _")
    };
    name_str(choices.get("method"), AT)?;
    name_str(choices.get("items"), AT)?;
    name_str(choices.get("key"), AT)?;
    if provider == own.provider && kind_of(own.methods, choices.get("method")) != Some("read") {
        return refuse(AT, "of the provider's own is a read it has");
    }
    Ok(())
}

fn kind_of<'a>(methods: &[(&str, &'a str)], name: Option<&Value>) -> Option<&'a str> {
    let name = name?.as_str()?;
    methods.iter().find(|(m, _)| *m == name).map(|(_, k)| *k)
}

fn check_resource(r: &Value, methods: &[(&str, &str)]) -> Result<(), DescriptorError> {
    let Some(members) = r.as_object() else { return refuse("resources", "are objects") };
    only(members, &["title", "help", "list", "get", "items", "key", "fields", "columns", "actions", "parent"],
         "resources")?;
    text_of(r.get("title"), "resources.title")?;
    opt_text(r.get("help"), "resources.help")?;
    if kind_of(methods, r.get("list")) != Some("read") {
        return refuse("resources.list", "is a read the provider has");
    }
    if r.get("get").is_some() && kind_of(methods, r.get("get")) != Some("read") {
        return refuse("resources.get", "is a read the provider has");
    }
    name_str(r.get("items"), "resources.items")?;
    let Some(fields) = r.get("fields").and_then(Value::as_object) else {
        return refuse("resources.fields", "is an object")
    };
    if fields.is_empty() {
        return refuse("resources.fields", "has a field");
    }
    for (name, field) in fields {
        name_ok(name, "resources.fields")?;
        check_field(field, "resources.fields")?;
    }
    let is_field = |v: &Value| v.as_str().is_some_and(|s| fields.iter().any(|(n, _)| n == s));
    if !r.get("key").is_some_and(is_field) {
        return refuse("resources.key", "is one of its fields");
    }
    let Some(columns) = r.get("columns").and_then(Value::as_list) else {
        return refuse("resources.columns", "is a list")
    };
    if columns.is_empty() || !columns.iter().all(is_field) {
        return refuse("resources.columns", "are some of its fields");
    }
    if let Some(actions) = r.get("actions") {
        let Some(actions) = actions.as_list() else { return refuse("resources.actions", "is a list") };
        for a in actions {
            let Some(members) = a.as_object() else { return refuse("resources.actions", "are objects") };
            only(members, &["method", "params"], "resources.actions")?;
            match kind_of(methods, a.get("method")) {
                Some("action") | Some("job") => {}
                _ => return refuse("resources.actions", "are actions and jobs the provider has"),
            }
            /* What each parameter is given: one of the resource's fields,
             * by "$name", or a value */
            if let Some(params) = a.get("params") {
                let Some(params) = params.as_object() else { return refuse("resources.actions", "give params as an object") };
                for (_, v) in params {
                    match v {
                        Value::Str(s) if s.starts_with('$') => {
                            if !fields.iter().any(|(n, _)| *n == s[1..]) {
                                return refuse("resources.actions", "give a parameter one of the resource's fields");
                            }
                        }
                        Value::List(_) | Value::Object(_) => {
                            return refuse("resources.actions", "give a parameter one value")
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

fn check_metric(m: &Value, methods: &[(&str, &str)]) -> Result<(), DescriptorError> {
    let Some(members) = m.as_object() else { return refuse("metrics", "are objects") };
    only(members, &["title", "help", "method", "items", "key", "field", "kind", "unit"], "metrics")?;
    text_of(m.get("title"), "metrics.title")?;
    opt_text(m.get("help"), "metrics.help")?;
    if kind_of(methods, m.get("method")) != Some("read") {
        return refuse("metrics.method", "is a read the provider has");
    }
    if m.get("items").is_some() {
        name_str(m.get("items"), "metrics.items")?;
        name_str(m.get("key"), "metrics.key")?;
    } else if m.get("key").is_some() {
        return refuse("metrics.key", "is for a metric of items");
    }
    name_str(m.get("field"), "metrics.field")?;
    match m.get("kind").and_then(Value::as_str) {
        Some("gauge") | Some("rate") => {}
        _ => return refuse("metrics.kind", "is gauge or rate"),
    }
    if !m.get("unit").and_then(Value::as_str).is_some_and(|u| !u.is_empty() && u.len() <= 16) {
        return refuse("metrics.unit", "is 1 to 16 bytes");
    }
    Ok(())
}
