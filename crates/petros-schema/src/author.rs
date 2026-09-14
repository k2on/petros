//! Building a mutation from a verb name and some JSON, without knowing what
//! either means.
//!
//! This is the other end of [`crate::mutations`]. That macro declares the verbs
//! and their argument types; `petros-codegen` reads the declaration out of a
//! built module and emits the TypeScript that calls them; and what that
//! TypeScript sends is JSON, because a foreign caller has no CBOR encoder. This
//! is where that JSON becomes a payload.
//!
//! All three are one contract, which is why they live together, and the
//! declaration is what decides: an argument is an id because the verb says its
//! type is [`Ty::Id`], not because of what it is called.
//!
//! Behind the `author` feature. A domain compiled to wasm decodes payloads and
//! never authors one, and `serde_json` has no business in a module that Metro
//! pushes on every save.

use crate::{AppSchema, Ty};
use ciborium::value::Value;

/// Build a mutation from a verb name and its arguments.
///
/// The payload is just `{ "t": kind, ...args }`. Auto-filled fields are not
/// this function's problem: `fill_auto` appends whatever the verb needs
/// afterwards, so an `Add` here is `{"text": "..."}` and the id and the
/// timestamp arrive later, chosen by the domain.
///
/// The schema decides what each argument means. An argument declared
/// [`Ty::Id`] holds a canonical uuid and becomes the sixteen bytes the log
/// uses; everything else is carried across as it stands.
///
/// An argument the verb does not declare is an error rather than a field
/// carried along and ignored. It reads like pedantry and is not: a payload
/// with a misspelled argument decodes with that field at its default, so the
/// mutation runs and quietly does nothing — no error anywhere, on any peer.
/// This is the only place that can still tell the difference.
pub fn from_value(
    schema: &AppSchema,
    kind: &str,
    args: serde_json::Value,
) -> Result<Value, String> {
    let verb = schema.verb(kind).ok_or_else(|| {
        format!(
            "no verb named {kind}; this app has {}",
            schema.names().join(", ")
        )
    })?;
    let serde_json::Value::Object(args) = args else {
        return Err("the arguments should be a json object".into());
    };
    let mut fields = vec![(Value::Text("t".into()), Value::Text(kind.to_string()))];
    for (name, value) in args {
        let ty = verb
            .args
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.ty)
            .ok_or_else(|| {
                let taken: Vec<&str> = verb.args.iter().map(|a| a.name.as_str()).collect();
                if taken.is_empty() {
                    format!("{kind} takes no arguments, and was given {name}")
                } else {
                    format!(
                        "{kind} has no argument {name}; it takes {}",
                        taken.join(", ")
                    )
                }
            })?;
        fields.push((Value::Text(name), json_to_cbor(value, ty == Ty::Id)?));
    }
    Ok(Value::Map(fields))
}

/// As [`from_value`], for a caller that has the arguments as JSON text — which
/// is every foreign one.
///
/// An empty string is an empty object, so a verb with no arguments can be
/// called without the caller inventing a `{}`.
pub fn from_json(schema: &AppSchema, kind: &str, args_json: &str) -> Result<Value, String> {
    let args: serde_json::Value = if args_json.trim().is_empty() {
        serde_json::Value::Object(Default::default())
    } else {
        serde_json::from_str(args_json).map_err(|e| format!("the arguments are not json: {e}"))?
    };
    from_value(schema, kind, args)
}

fn json_to_cbor(value: serde_json::Value, is_id_field: bool) -> Result<Value, String> {
    use serde_json::Value as J;
    Ok(match value {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(b),
        J::Number(n) => match n.as_i64() {
            Some(i) => Value::Integer(i.into()),
            // No floats anywhere near the log: `apply` must not branch on one,
            // and two peers need not agree on how one prints.
            None => return Err(format!("{n} is not an integer")),
        },
        J::String(s) if is_id_field => Value::Bytes(
            uuid::Uuid::parse_str(&s)
                .map_err(|e| format!("not an id: {e}"))?
                .as_bytes()
                .to_vec(),
        ),
        J::String(s) => Value::Text(s),
        J::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| json_to_cbor(v, false))
                .collect::<Result<_, _>>()?,
        ),
        J::Object(entries) => Value::Map(
            entries
                .into_iter()
                // An argument's declared type is scalar, so nothing nested
                // inside one is an id.
                .map(|(k, v)| Ok((Value::Text(k), json_to_cbor(v, false)?)))
                .collect::<Result<Vec<_>, String>>()?,
        ),
    })
}
