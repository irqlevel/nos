//! A method's parameters taken strictly: each by name and checked as it is
//! taken -- its kind, its range, its length, what characters it may hold --
//! and the whole refused if one is left that the method did not take, so a
//! name misspelt by the caller is an error rather than a default silently
//! used. What is refused names the parameter and why, for the caller to show
//! beside the field it came from.

use crate::json::{digits_u64, Value};
use alloc::string::String;

/// Parameters, at most: what `Params` marks taken.
const MAX_PARAMS: usize = 64;

/// A parameter refused: which, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Invalid<'a> {
    pub field: &'a str,
    pub why: &'static str,
}

pub struct Params<'a> {
    members: &'a [(String, Value)],
    /// Bit i: member i has been taken.
    taken: u64,
}

impl<'a> Params<'a> {
    /// The parameters `v` holds: an object, or null for none.
    pub fn new(v: &'a Value) -> Result<Params<'a>, Invalid<'a>> {
        let members: &'a [(String, Value)] = match v {
            Value::Null => &[],
            Value::Object(m) => m,
            _ => return Err(Invalid { field: "", why: "parameters are an object" }),
        };
        if members.len() > MAX_PARAMS {
            return Err(Invalid { field: "", why: "more parameters than a method takes" });
        }
        Ok(Params { members, taken: 0 })
    }

    /// The member `key`, marked taken; None when it is not there or null.
    fn take(&mut self, key: &str) -> Option<&'a Value> {
        let i = self.members.iter().position(|(k, _)| k == key)?;
        self.taken |= 1 << i;
        match &self.members[i].1 {
            Value::Null => None,
            v => Some(v),
        }
    }

    /// A string of 1 to `max` bytes with no control character in it: a name,
    /// a path, a URL.
    pub fn str(&mut self, key: &'a str, max: usize) -> Result<&'a str, Invalid<'a>> {
        self.opt_str(key, max)?.ok_or(Invalid { field: key, why: "is needed" })
    }

    pub fn opt_str(&mut self, key: &'a str, max: usize) -> Result<Option<&'a str>, Invalid<'a>> {
        let Some(v) = self.take(key) else { return Ok(None) };
        let s = v.as_str().ok_or(Invalid { field: key, why: "is a string" })?;
        if s.is_empty() || s.len() > max {
            return Err(Invalid { field: key, why: "is too long or empty" });
        }
        if s.chars().any(char::is_control) {
            return Err(Invalid { field: key, why: "has a control character in it" });
        }
        Ok(Some(s))
    }

    /// Text of up to `max` bytes: lines, tabs, any characters but the other
    /// control ones -- a configuration file's.
    pub fn text(&mut self, key: &'a str, max: usize) -> Result<&'a str, Invalid<'a>> {
        let v = self.take(key).ok_or(Invalid { field: key, why: "is needed" })?;
        let s = v.as_str().ok_or(Invalid { field: key, why: "is a string" })?;
        if s.len() > max {
            return Err(Invalid { field: key, why: "is too long" });
        }
        if s.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
            return Err(Invalid { field: key, why: "has a control character in it" });
        }
        Ok(s)
    }

    /// A count from `min` to `max`: a number, or its digits in a string.
    pub fn uint(&mut self, key: &'a str, min: u64, max: u64) -> Result<u64, Invalid<'a>> {
        self.opt_uint(key, min, max)?.ok_or(Invalid { field: key, why: "is needed" })
    }

    pub fn opt_uint(&mut self, key: &'a str, min: u64, max: u64) -> Result<Option<u64>, Invalid<'a>> {
        let Some(v) = self.take(key) else { return Ok(None) };
        let n = match v {
            Value::Int(n) => u64::try_from(*n).ok(),
            Value::Str(s) => digits_u64(s),
            _ => None,
        }
        .ok_or(Invalid { field: key, why: "is a whole number, not below 0" })?;
        if n < min || n > max {
            return Err(Invalid { field: key, why: "is out of its range" });
        }
        Ok(Some(n))
    }

    pub fn bool(&mut self, key: &'a str) -> Result<bool, Invalid<'a>> {
        self.opt_bool(key)?.ok_or(Invalid { field: key, why: "is needed" })
    }

    pub fn opt_bool(&mut self, key: &'a str) -> Result<Option<bool>, Invalid<'a>> {
        let Some(v) = self.take(key) else { return Ok(None) };
        v.as_bool().map(Some).ok_or(Invalid { field: key, why: "is true or false" })
    }

    /// A list of at most `max` values, each the caller's to check.
    pub fn list(&mut self, key: &'a str, max: usize) -> Result<&'a [Value], Invalid<'a>> {
        let Some(v) = self.take(key) else { return Ok(&[]) };
        let items = v.as_list().ok_or(Invalid { field: key, why: "is a list" })?;
        if items.len() > max {
            return Err(Invalid { field: key, why: "has too many items" });
        }
        Ok(items)
    }

    /// Done: refused if a parameter was handed that was never taken.
    pub fn finish(self) -> Result<(), Invalid<'a>> {
        for (i, (k, _)) in self.members.iter().enumerate() {
            if self.taken & (1 << i) == 0 {
                return Err(Invalid { field: k, why: "is not a parameter of this method" });
            }
        }
        Ok(())
    }
}
