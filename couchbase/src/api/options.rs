use crate::api::collection::MutationState;
use serde::Serializer;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use uuid::Uuid;

/// Macro to DRY up the repetitive timeout setter.
macro_rules! timeout {
    () => {
        pub fn timeout(mut self, timeout: Duration) -> Self {
            self.timeout = Some(timeout);
            self
        }
    };
}

macro_rules! expiry {
    () => {
        pub fn expiry(mut self, expiry: Duration) -> Self {
            self.expiry = Some(expiry);
            self
        }
    };
}

macro_rules! xattr {
    () => {
        pub fn xattr(mut self, xattr: bool) -> Self {
            self.xattr = xattr;
            self
        }
    };
}

macro_rules! preserve_expiry {
    () => {
        pub fn preserve_expiry(mut self, preserve: bool) -> Self {
            self.preserve_expiry = preserve;
            self
        }
    };
}

pub(crate) fn convert_mutation_state<S>(
    _x: &Option<MutationState>,
    _s: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    todo!()
}

pub(crate) fn convert_duration_for_golang<S>(x: &Option<Duration>, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_str(&format!(
        "{}ms",
        x.expect("Expected a duration!").as_millis()
    ))
}

pub(crate) fn default_client_context_id<S>(x: &Option<String>, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if x.is_some() {
        s.serialize_str(x.as_ref().unwrap())
    } else {
        s.serialize_str(&format!("{}", Uuid::new_v4()))
    }
}

pub(crate) fn convert_named_params<S>(
    x: &Option<serde_json::Map<String, Value>>,
    s: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match x {
        Some(m) => {
            let conv: HashMap<String, &Value> =
                m.iter().map(|(k, v)| (format!("${}", k), v)).collect();
            s.serialize_some(&conv)
        }
        None => s.serialize_none(),
    }
}
