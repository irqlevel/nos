//! A provider's descriptor as the bus checks it (`mgmtwire::descriptor`):
//! one that has every member the format has taken, each defect of a
//! descriptor refused, a broken one never a panic -- and the tree's own
//! descriptors, each held to the methods it says it has, which the kernel
//! then holds to what the provider registers.

use crate::{Fails, Rng};
use mgmtwire::descriptor::{self, LIMITS};
use mgmtwire::{json, Value};

/// Every member the format has: a method of each kind, parameters of every
/// shape, a resource and a resource of it, a metric.
const BASE: &str = r#"{"schema": 1, "provider": "demo",
 "title": {"en": "Demo", "ru": "Демо"},
 "help": {"en": "A provider for the test.", "ru": "Провайдер для проверки."},
 "ui": {"section": "demo", "order": 5, "icon": "box"},
 "methods": {
  "list": {"kind": "read", "title": {"en": "List", "ru": "Список"}},
  "get": {"kind": "read", "title": {"en": "Get", "ru": "Получить"}, "result": "thing",
    "params": {"name": {"type": "string", "required": true, "title": {"en": "Name", "ru": "Имя"}}}},
  "make": {"kind": "action", "title": {"en": "Make", "ru": "Создать"}, "confirm": {"en": "Sure?", "ru": "Точно?"},
    "params": {"size": {"type": "bytes", "min": 1, "max": 100, "unit": "B", "default": 10, "title": {"en": "Size", "ru": "Размер"}},
               "mode": {"type": "enum", "title": {"en": "Mode", "ru": "Режим"}, "values": {"a": {"en": "A", "ru": "А"}}},
               "tags": {"type": "list", "of": "string", "title": {"en": "Tags", "ru": "Метки"}}}},
  "drop": {"kind": "job", "title": {"en": "Drop", "ru": "Удалить"}}
 },
 "resources": {
  "things": {"title": {"en": "Things", "ru": "Вещи"}, "list": "list", "get": "get", "items": "things", "key": "name",
    "fields": {"name": {"type": "string", "title": {"en": "Name", "ru": "Имя"}}, "size": {"type": "bytes", "title": {"en": "Size", "ru": "Размер"}}},
    "columns": ["name", "size"],
    "actions": [{"method": "drop", "params": {"name": "$name", "force": true}}]},
  "parts": {"title": {"en": "Parts", "ru": "Части"}, "list": "list", "items": "parts", "key": "id", "parent": "things",
    "fields": {"id": {"type": "int", "title": {"en": "Id", "ru": "Номер"}}}, "columns": ["id"]}
 },
 "metrics": {"size": {"title": {"en": "Size", "ru": "Размер"}, "method":"list", "items":"things", "key":"name", "field": "size", "kind": "gauge", "unit": "B"}}
}"#;

const METHODS: [(&str, &str); 4] = [("list", "read"), ("get", "read"), ("make", "action"), ("drop", "job")];

/// Each defect, as what in BASE becomes what: the descriptor it makes must
/// be refused.
const DEFECTS: [(&str, &str, &str); 31] = [
    ("another schema", r#""schema": 1"#, r#""schema": 2"#),
    ("a text without a language", r#""title": {"en": "Demo", "ru": "Демо"}"#, r#""title": {"en": "Demo"}"#),
    ("a text in a language more", r#""title": {"en": "Demo", "ru": "Демо"}"#, r#""title": {"en": "Demo", "ru": "Демо", "de": "Demo"}"#),
    ("an empty text", r#""ru": "Демо""#, r#""ru": """#),
    ("a text that is a string", r#""title": {"en": "Demo", "ru": "Демо"}"#, r#""title": "Demo""#),
    ("a method not described", r#",
  "drop": {"kind": "job", "title": {"en": "Drop", "ru": "Удалить"}}"#, ""),
    ("a method of another kind", r#""drop": {"kind": "job""#, r#""drop": {"kind": "action""#),
    ("a read that asks to be confirmed", r#""list": {"kind": "read", "title": {"en": "List", "ru": "Список"}}"#,
     r#""list": {"kind": "read", "title": {"en": "List", "ru": "Список"}, "confirm": {"en": "?", "ru": "?"}}"#),
    ("a method with no title", r#""drop": {"kind": "job", "title": {"en": "Drop", "ru": "Удалить"}}"#, r#""drop": {"kind": "job"}"#),
    ("a member the format does not have", r#""schema": 1,"#, r#""schema": 1, "extra": 1,"#),
    ("a method's member the format does not have", r#""drop": {"kind": "job""#, r#""drop": {"role": "admin", "kind": "job""#),
    ("a type the format does not have", r#""type": "bytes", "min": 1"#, r#""type": "float", "min": 1"#),
    ("an enum without its values", r#", "values": {"a": {"en": "A", "ru": "А"}}"#, ""),
    ("values for what is no enum", r#""type": "list", "of": "string""#, r#""type": "list", "of": "string", "values": {"a": {"en": "A", "ru": "А"}}"#),
    ("a list of nothing said", r#", "of": "string""#, ""),
    ("a list of lists", r#""of": "string""#, r#""of": "list""#),
    ("a bound that is no number", r#""max": 100"#, r#""max": "100""#),
    ("an order that is no count", r#""order": 5"#, r#""order": "5""#),
    ("a default that is a list", r#""default": 10"#, r#""default": [10]"#),
    ("a resource listed by an action", r#""list": "list", "get": "get""#, r#""list": "make", "get": "get""#),
    ("a resource got by no method there is", r#""get": "get", "items""#, r#""get": "fetch", "items""#),
    ("a column that is no field", r#""columns": ["name", "size"]"#, r#""columns": ["name", "weight"]"#),
    ("a key that is no field", r#""items": "things", "key": "name""#, r#""items": "things", "key": "nick""#),
    ("a resource with no field", r#""fields": {"id": {"type": "int", "title": {"en": "Id", "ru": "Номер"}}}"#, r#""fields": {}"#),
    ("an action that is a read", r#"{"method": "drop""#, r#"{"method": "list""#),
    ("a parameter from a field there is not", r#""$name""#, r#""$nosuch""#),
    ("a resource its own parent", r#""parent": "things""#, r#""parent": "parts""#),
    ("a parent there is not", r#""parent": "things""#, r#""parent": "nosuch""#),
    ("a metric neither gauge nor rate", r#""kind": "gauge""#, r#""kind": "average""#),
    ("a metric read by an action", r#""method":"list""#, r#""method":"make""#),
    ("a metric with no unit", r#""unit": "B"}}"#, r#""unit": ""}}"#),
];

pub fn vectors(f: &mut Fails) {
    let base = BASE.as_bytes();
    f.check("a descriptor with every member the format has", descriptor::check(base, "demo", &METHODS).is_ok(),
            descriptor::check(base, "demo", &METHODS));
    f.check("another provider's", descriptor::check(base, "other", &METHODS).is_err(), ());
    f.check("one with a method the provider does not register",
            descriptor::check(base, "demo", &METHODS[..3]).is_err(), ());
    f.check("one missing a method the provider registers",
            descriptor::check(base, "demo", &[METHODS[0], METHODS[1], METHODS[2], METHODS[3], ("more", "read")]).is_err(), ());
    f.check("what is not JSON", descriptor::check(b"{\"schema\": 1", "demo", &METHODS).is_err(), ());
    f.check("what is not an object", descriptor::check(b"[]", "demo", &METHODS).is_err(), ());
    let mut long = BASE.to_string();
    long.push_str(&" ".repeat(LIMITS.bytes));
    f.check("one past its limit", descriptor::check(long.as_bytes(), "demo", &METHODS).is_err(), ());
    for (what, from, to) in DEFECTS {
        f.check("each defect is in the base once", BASE.matches(from).count() == 1, what);
        let broken = BASE.replacen(from, to, 1);
        f.check(what, descriptor::check(broken.as_bytes(), "demo", &METHODS).is_err(), "taken");
    }
}

/// The base, broken at random: refused or taken, never a panic.
pub fn one(rng: &mut Rng, f: &mut Fails) {
    let mut b = BASE.as_bytes().to_vec();
    crate::fuzzing::mutate(rng, &mut b, b"\"{}[],:$");
    let _ = descriptor::check(&b, "demo", &METHODS);
    /* And a defect, with the base broken somewhere else as well */
    let (what, from, to) = DEFECTS[rng.below(DEFECTS.len() as u64) as usize];
    let mut broken = BASE.replacen(from, to, 1).into_bytes();
    if rng.chance(2) {
        crate::fuzzing::mutate(rng, &mut broken, b" ");
    }
    if json::parse(&broken, &LIMITS).is_ok() {
        f.check(what, descriptor::check(&broken, "demo", &METHODS).is_err(), "taken, broken further at random");
    }
}

/// The descriptor in `path`, held to the methods it says it has: the format
/// checked as the bus checks it, for the file's own provider.
pub fn file(path: &str) -> usize {
    let text = match std::fs::read(path) {
        Ok(t) => t,
        Err(e) => {
            println!("FAIL: {}: {}", path, e);
            return 1;
        }
    };
    let Ok(v) = json::parse(&text, &LIMITS) else {
        println!("FAIL: {}: not JSON as the bus reads it, or past its limits", path);
        return 1;
    };
    let provider = v.get("provider").and_then(Value::as_str).unwrap_or("");
    let methods: Vec<(&str, &str)> = v
        .get("methods")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(name, m)| (name.as_str(), m.get("kind").and_then(Value::as_str).unwrap_or(""))).collect())
        .unwrap_or_default();
    match descriptor::check(&text, provider, &methods) {
        Ok(()) => {
            println!("descriptor {}: provider {}, {} methods, taken", path, provider, methods.len());
            0
        }
        Err(e) => {
            println!("FAIL: {}: {} {}", path, e.at, e.why);
            1
        }
    }
}
