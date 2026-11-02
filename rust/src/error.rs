use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{Value, json};

/// Python returns HTTP 200 with {"error": ...}; index.html reads data.error after
/// r.json(). A 4xx here would break the dashboard silently.
pub struct AppError(pub String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::OK, Json(json!({ "error": self.0 }))).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> AppError { AppError(e.to_string()) }
}

/// What one declared request-body field accepts, for the pre-deserialization type pass in
/// `ValidationErrors::parse_json_body`. Mirrors the pydantic field kinds FastAPI validates with:
/// `Int` = `int` (rejects strings/floats/null, coerces integral floats and numeric strings),
/// `Str` = `str`, `OptStr` = `Optional[str]` (also accepts `null`).
#[derive(Clone, Copy, PartialEq)]
pub enum FieldKind { Int, Str, OptStr }

/// One declared field: its name, its pydantic type, and — for `Int` — the `Field(ge=..., le=...)`
/// bounds. The bounds live here rather than in the model because pydantic validates type and
/// constraint in the SAME pass and answers ONE 422 listing every offending field; live on
/// `:8501`, `{"model": 5, "max_tokens": 5000}` returns `string_type` for `model` followed by
/// `less_than_equal` for `max_tokens`. Checking constraints after deserialization (the previous
/// `TestReq::validate`) could only ever report the first error it found.
pub struct FieldSpec {
    pub name: &'static str, pub kind: FieldKind,
    pub min: Option<i64>, pub max: Option<i64>,
}

impl FieldSpec {
    /// The common case: a typed field with no constraints.
    pub const fn plain(name: &'static str, kind: FieldKind) -> FieldSpec {
        FieldSpec { name, kind, min: None, max: None }
    }
    /// An `int` field with an inclusive `ge`/`le` range.
    pub const fn ranged(name: &'static str, min: i64, max: i64) -> FieldSpec {
        FieldSpec { name, kind: FieldKind::Int, min: Some(min), max: Some(max) }
    }
}

/// pydantic answers 422 with a "detail" array before the handler runs. Mirrored, not
/// "fixed" into a friendly 200, because wire parity is the goal. See spec 5.1.
#[derive(Debug)]
pub struct ValidationErrors(pub Value);

/// The character offset of serde's 1-based (line, column) in `text`. FastAPI reports
/// json_invalid's `loc[1]` as the 0-based character index of the offending character
/// (measured live against `:8501`: `{ bad` -> 2, `{\n  x` -> 4,
/// `{\n  "a": 1,\n  bad }` -> 14). Every preceding line contributes its full length including
/// its `\n`, and the column is 0-based within the failing line — so this reproduces the rule
/// with `line`-1 whole lines plus `column`-1. For ASCII, which every body
/// `static/index.html` sends is, the character and byte index are the same number.
fn char_offset(text: &str, line: usize, column: usize) -> usize {
    text.split_inclusive('\n').take(line.saturating_sub(1)).map(str::len).sum::<usize>()
        + column.saturating_sub(1)
}

/// pydantic's json_invalid offset for a *truncated* body is one past the last character, i.e.
/// the length of the input; serde reports the position of the last character it consumed, one
/// less. Live on `:8501`: `{` -> 1, `{"a"` -> 4, `{"model":` -> 9, `[1,` -> 3, `{"a":1,` -> 7,
/// `  ` -> 2 — always the byte length, because the failure point is end-of-input itself. So an
/// EOF error takes the length and every other error the rule above.
fn json_error_offset(text: &str, e: &serde_json::Error) -> usize {
    if e.is_eof() { text.chars().count() } else { char_offset(text, e.line(), e.column()) }
}

impl ValidationErrors {
    /// FastAPI's answer to an unparseable body: a `json_invalid` entry whose `loc[1]` is the
    /// *character offset* of the parse failure. The `ctx.error` wording is the json parser's
    /// own (`{ bad` is "Expecting property name enclosed in double quotes" in CPython's `json`
    /// and "key must be a string at line 1 column 3" in `serde_json`): the structure, offset
    /// and status are parity, the prose is parser-specific and not reproduced.
    pub fn json_invalid(msg: &str, offset: usize) -> ValidationErrors {
        ValidationErrors(json!({ "detail": [
            { "type": "json_invalid", "loc": ["body", offset], "msg": "JSON decode error",
              "input": {}, "ctx": { "error": msg } }
        ]}))
    }

    /// FastAPI's answer to a body the model never saw: an empty or literal-`null` body is a
    /// `missing` error on `["body"]` (both verified live against `:8501`), not a syntax error.
    fn missing_body() -> ValidationErrors {
        ValidationErrors(json!({ "detail": [
            { "type": "missing", "loc": ["body"], "msg": "Field required", "input": null }
        ]}))
    }

    /// The single mapping from a raw JSON request body to either a validated model or the
    /// FastAPI-shaped 422, used by every JSON POST handler (`/api/test`, `POST /api/profile`,
    /// and Task 6's `/api/stress`) instead of a per-handler shim.
    ///
    /// This replaces `from_json_rejection(&JsonRejection)` as the shared mapping: axum's
    /// rejection carries only a static description string — `body_text()` renders the
    /// `#[body = ...]` literal plus the inner error's `Display` (axum-core 0.5.6
    /// `macros.rs`), never the raw body or the offending field — so neither the json_invalid
    /// character offset nor the per-field `int_parsing` / `string_type` entries Ruling 1
    /// requires can be built from it. Handlers therefore extract `Bytes` and route the body
    /// through here; the rejection type no longer reaches the handlers at all.
    ///
    /// The passes mirror pydantic's, with each shape verified live against the reference app:
    /// syntax error -> `json_invalid` at the character offset; `null`/empty -> `missing`;
    /// non-object -> `model_attributes_type`; a wrong-typed field -> `int_parsing` /
    /// `int_from_float` / `int_type` / `string_type` echoing the input; an in-range-coercible
    /// value that violates `ge`/`le` -> `greater_than_equal` / `less_than_equal`; lax coercions
    /// pydantic performs (integral floats, numeric strings like `"20"`) are applied to the value
    /// before it is deserialized, so the accepted shapes match too. Type and constraint are one
    /// pass over declaration order, so a 422 can carry entries for several fields at once.
    pub fn parse_json_body<T: serde::de::DeserializeOwned>(
        body: &[u8], fields: &[FieldSpec]) -> Result<T, ValidationErrors>
    {
        if body.is_empty() { return Err(Self::missing_body()); }
        let text = String::from_utf8_lossy(body);
        let mut value = match serde_json::from_str::<Value>(&text) {
            Ok(v) => v,
            Err(e) => return Err(Self::json_invalid(
                &e.to_string(), json_error_offset(&text, &e))),
        };
        if value.is_null() { return Err(Self::missing_body()); }
        if !value.is_object() {
            return Err(ValidationErrors(json!({ "detail": [{
                "type": "model_attributes_type", "loc": ["body"],
                "msg": "Input should be a valid dictionary or object to extract fields from",
                "input": value
            }]})));
        }
        let map = value.as_object().expect("checked is_object above");
        let mut errors: Vec<Value> = Vec::new();
        // Pydantic's lax coercions are collected while reading and applied after the loop:
        // `map` borrows `value` immutably, so writing back inside the loop would not borrow-check.
        let mut coerced: Vec<(&'static str, Value)> = Vec::new();
        for spec in fields {
            let Some(v) = map.get(spec.name) else { continue };
            match spec.kind {
                // pydantic parses the type first and checks the bounds only for a value it
                // accepted: `{"max_tokens":"x"}` is `int_parsing` alone, never a range entry.
                FieldKind::Int => {
                    let parsed: Option<i64> = match v {
                        Value::Number(n) => match n.as_i64() {
                            Some(i) => Some(i),
                            None => {
                                // Lax mode accepts an integral float (`20.0`) and an integer
                                // above `i64::MAX`; `1.5` is `int_from_float` (live).
                                let f = n.as_f64()
                                    .expect("every JSON number is f64-representable");
                                if f.fract() != 0.0 {
                                    errors.push(entry("int_from_float", spec.name, v,
                                        "Input should be a valid integer, got a number with a fractional part"));
                                    None
                                } else if f.abs() <= i64::MAX as f64 {
                                    Some(f as i64)
                                } else {
                                    // Integral but not an `i64`: pydantic accepts it (Python
                                    // ints are unbounded) and only a bound can reject it.
                                    errors.extend(range_entry_for_huge(spec, v, f > 0.0));
                                    None
                                }
                            }
                        },
                        Value::String(s) => match s.trim().parse::<i64>() {
                            // pydantic lax mode coerces a numeric string (`{"max_tokens":"20"}`
                            // passes validation live on :8501); anything else is `int_parsing`.
                            Ok(i) => Some(i),
                            Err(_) => {
                                errors.push(entry("int_parsing", spec.name, v,
                                    "Input should be a valid integer, unable to parse string as an integer"));
                                None
                            }
                        },
                        _ => {
                            errors.push(entry("int_type", spec.name, v,
                                "Input should be a valid integer"));
                            None
                        }
                    };
                    let Some(i) = parsed else { continue };
                    if let Some(min) = spec.min.filter(|min| i < *min) {
                        errors.push(range_entry(spec, v, "greater_than_equal", min, "ge"));
                    } else if let Some(max) = spec.max.filter(|max| i > *max) {
                        errors.push(range_entry(spec, v, "less_than_equal", max, "le"));
                    } else {
                        // Also normalises `"20"` / `20.0` to the integer the struct wants.
                        coerced.push((spec.name, json!(i)));
                    }
                }
                kind @ (FieldKind::Str | FieldKind::OptStr) => {
                    let ok = v.is_string() || (kind == FieldKind::OptStr && v.is_null());
                    if !ok {
                        errors.push(entry("string_type", spec.name, v,
                            "Input should be a valid string"));
                    }
                }
            }
        }
        if !errors.is_empty() {
            return Err(ValidationErrors(json!({ "detail": errors })));
        }
        if !coerced.is_empty() {
            let map = value.as_object_mut().expect("checked is_object above");
            for (name, v) in coerced { map.insert(name.to_string(), v); }
        }
        // Every declared field is type-checked and coerced above; a residual failure means the
        // `FieldSpec` table and the struct drifted apart. Report it as a decode error rather
        // than panicking; it is unreachable while the tables next to the models stay complete.
        serde_json::from_value(value)
            .map_err(|e| ValidationErrors::json_invalid(&e.to_string(), 0))
    }
}

/// One `detail` entry for a type error: pydantic's ordered keys `type`/`loc`/`msg`/`input`;
/// none of the type errors carries a `ctx`, so it is simply absent (as in the live bodies).
fn entry(kind: &str, field: &str, input: &Value, msg: &str) -> Value {
    json!({ "type": kind, "loc": ["body", field], "msg": msg, "input": input })
}

/// A `ge`/`le` constraint entry. `input` echoes the RAW value, not the coerced integer: live on
/// `:8501`, `{"max_tokens":"5000"}` answers `"input": "5000"` and `{"max_tokens":2049.0}`
/// answers `"input": 2049.0`. The `ctx` bound key is `ge` or `le`, matching the rule.
fn range_entry(spec: &FieldSpec, raw: &Value, kind: &str, bound: i64, ctx_key: &str) -> Value {
    let msg = match ctx_key {
        "le" => format!("Input should be less than or equal to {bound}"),
        _ => format!("Input should be greater than or equal to {bound}"),
    };
    let ctx = Value::Object(serde_json::Map::from_iter([(ctx_key.to_string(), json!(bound))]));
    json!({ "type": kind, "loc": ["body", spec.name], "msg": msg, "input": raw, "ctx": ctx })
}

/// The constraint entry for an integral value beyond `i64`: pydantic would accept the integer
/// itself and reject it only on the bound it exceeds. With no bound declared the value is outside
/// what this API's `i64` models can carry, and a type error is the closest honest answer — no
/// such field exists today (`max_tokens` is the only `Int`, and it is bounded).
fn range_entry_for_huge(spec: &FieldSpec, raw: &Value, above: bool) -> Vec<Value> {
    let hit = if above {
        spec.max.map(|max| range_entry(spec, raw, "less_than_equal", max, "le"))
    } else {
        spec.min.map(|min| range_entry(spec, raw, "greater_than_equal", min, "ge"))
    };
    vec![hit.unwrap_or_else(|| entry("int_type", spec.name, raw,
        "Input should be a valid integer"))]
}

impl IntoResponse for ValidationErrors {
    fn into_response(self) -> Response {
        (StatusCode::UNPROCESSABLE_ENTITY, Json(self.0)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::{FieldKind, FieldSpec, ValidationErrors};
    use serde::Deserialize;
    use serde_json::{Value, json};

    #[derive(Deserialize, PartialEq, Debug)]
    struct M { model: String, max_tokens: i64, base_url: Option<String> }

    // The same table shape as `TestReq::FIELDS`, bounds included, so the unit tests exercise
    // the code path /api/test actually takes.
    const FIELDS: &[FieldSpec] = &[
        FieldSpec::plain("model", FieldKind::Str),
        FieldSpec::ranged("max_tokens", 1, 2048),
        FieldSpec::plain("base_url", FieldKind::OptStr),
    ];

    fn detail(body: &str) -> Value {
        let err = ValidationErrors::parse_json_body::<M>(body.as_bytes(), FIELDS)
            .expect_err("the input must be rejected");
        err.0["detail"].clone()
    }

    // The split Ruling 1 owns, each shape pinned against the live Python app first:
    #[test]
    fn syntax_error_carries_the_character_offset() {
        // Python: `{"detail":[{"type":"json_invalid","loc":["body",2],...` for `{ bad`.
        let d = detail("{ bad");
        assert_eq!(d[0]["type"], "json_invalid");
        assert_eq!(d[0]["loc"], json!(["body", 2]));
        assert_eq!(d[0]["msg"], "JSON decode error");
        assert_eq!(d[0]["input"], json!({}));
        assert!(d[0]["ctx"].get("error").is_some());
        // A later line offsets past the newline. Measured against `:8501`, not reasoned out:
        // `{\n  x` answers `loc[1] == 4` — the 0-based character index of the offending `x`.
        // (An earlier draft of this test expected 3; the live app says 4 and the implementation
        // agrees with the live app, so the expectation, not the code, was wrong.)
        let d = detail("{\n  x");
        assert_eq!(d[0]["loc"], json!(["body", 4]));
        // Two more live measurements pin the same rule at larger offsets:
        //   `{\n  "a": 1,\n  bad }` -> 14 (index of `b`, both preceding lines counted)
        let d = detail("{\n  \"a\": 1,\n  bad }");
        assert_eq!(d[0]["loc"], json!(["body", 14]));
    }

    // A truncated body fails at end-of-input, and pydantic's offset there is the length of the
    // body, not serde's position of the last consumed character (one less). All six inputs and
    // their `loc[1]` values were captured from `:8501` before this branch was written.
    #[test]
    fn truncated_bodies_offset_to_end_of_input() {
        for (body, offset) in [("{", 1), ("{\"a\"", 4), ("{\"model\":", 9),
                               ("[1,", 3), ("{\"a\":1,", 7), ("  ", 2)] {
            let d = detail(body);
            assert_eq!(d[0]["type"], "json_invalid", "{body}");
            assert_eq!(d[0]["loc"], json!(["body", offset]), "{body} -> {d}");
        }
    }

    #[test]
    fn empty_or_null_body_is_missing_not_syntax() {
        for body in ["", "null"] {
            let d = detail(body);
            assert_eq!(d[0]["type"], "missing", "{body:?}");
            assert_eq!(d[0]["loc"], json!(["body"]));
            assert_eq!(d[0]["msg"], "Field required");
            assert_eq!(d[0]["input"], Value::Null);
        }
    }

    #[test]
    fn non_object_body_is_model_attributes_type() {
        for (body, input) in [("[1,2]", json!([1, 2])), ("\"s\"", json!("s")), ("123", json!(123))] {
            let d = detail(body);
            assert_eq!(d[0]["type"], "model_attributes_type", "{body}");
            assert_eq!(d[0]["loc"], json!(["body"]));
            assert_eq!(d[0]["msg"],
                       "Input should be a valid dictionary or object to extract fields from");
            assert_eq!(d[0]["input"], input);
        }
    }

    #[test]
    fn wrong_typed_fields_echo_their_input() {
        let d = detail(r#"{"model":"a","max_tokens":"x","base_url":5,"model2":null}"#);
        // max_tokens (int given a non-numeric string) then base_url (Optional[str] given 5):
        // one entry per offending field, in declaration order.
        assert_eq!(d.as_array().unwrap().len(), 2, "{d}");
        assert_eq!(d[0]["type"], "int_parsing");
        assert_eq!(d[0]["loc"], json!(["body", "max_tokens"]));
        assert_eq!(d[0]["msg"],
                   "Input should be a valid integer, unable to parse string as an integer");
        assert_eq!(d[0]["input"], "x");
        // int_parsing carries no ctx in pydantic (see the live body above).
        assert!(d[0].get("ctx").is_none(), "{}", d[0]);
        assert_eq!(d[1]["type"], "string_type");
        assert_eq!(d[1]["loc"], json!(["body", "base_url"]));
        assert_eq!(d[1]["input"], 5);

        // The fractional/boolean int variants pydantic distinguishes (both captured live):
        let d = detail(r#"{"model":"a","max_tokens":1.5,"base_url":null}"#);
        assert_eq!(d[0]["type"], "int_from_float");
        assert_eq!(d[0]["msg"], "Input should be a valid integer, got a number with a fractional part");
        let d = detail(r#"{"model":"a","max_tokens":null,"base_url":null}"#);
        assert_eq!(d[0]["type"], "int_type");
        // A required str given null is a string_type too (`model: String` has no default path).
        let d = detail(r#"{"model":null,"max_tokens":1,"base_url":null}"#);
        assert_eq!(d[0]["type"], "string_type");
        assert_eq!(d[0]["input"], Value::Null);
    }

    #[test]
    fn pydantic_lax_coercions_are_accepted() {
        // Integral floats and numeric strings validate in pydantic lax mode (live: `"20"`
        // reaches the probe), so they must parse here too, into the same numbers.
        let m = ValidationErrors::parse_json_body::<M>(
            br#"{"model":"m","max_tokens":"20","base_url":null}"#, FIELDS).unwrap();
        assert_eq!(m.max_tokens, 20);
        let m = ValidationErrors::parse_json_body::<M>(
            br#"{"model":"m","max_tokens":20.0,"base_url":"http://x"}"#, FIELDS).unwrap();
        assert_eq!(m, M { model: "m".into(), max_tokens: 20, base_url: Some("http://x".into()) });
    }

    // The constraint pass, every shape measured live on `:8501` before it was written. The raw
    // input is echoed (a numeric string stays a string, an integral float stays a float), the
    // `ctx` names the bound, and entries from different fields aggregate in declaration order.
    #[test]
    fn bounds_echo_the_raw_input_and_aggregate_with_type_errors() {
        let d = detail(r#"{"model":"m","max_tokens":"5000","base_url":null}"#);
        assert_eq!(d, json!([{ "type": "less_than_equal", "loc": ["body", "max_tokens"],
            "msg": "Input should be less than or equal to 2048", "input": "5000",
            "ctx": { "le": 2048 } }]), "the string input must survive, not become 5000");
        let d = detail(r#"{"model":"m","max_tokens":2049.0,"base_url":null}"#);
        assert_eq!(d[0]["type"], "less_than_equal");
        assert_eq!(d[0]["input"], json!(2049.0), "the float input must survive, not become 2049");

        let d = detail(r#"{"model":"m","max_tokens":0,"base_url":null}"#);
        assert_eq!(d[0]["type"], "greater_than_equal");
        assert_eq!(d[0]["ctx"], json!({ "ge": 1 }));

        // One 422, every offending field, declaration order.
        let d = detail(r#"{"model":5,"max_tokens":5000,"base_url":7}"#);
        assert_eq!(d.as_array().unwrap().len(), 3, "{d}");
        assert_eq!(d[0]["type"], "string_type");
        assert_eq!(d[1]["type"], "less_than_equal");
        assert_eq!(d[2]["type"], "string_type");

        // A field whose type failed gets no constraint entry: pydantic checks `le` only for a
        // value it parsed.
        let d = detail(r#"{"model":"m","max_tokens":"x","base_url":null}"#);
        assert_eq!(d.as_array().unwrap().len(), 1, "{d}");
        assert_eq!(d[0]["type"], "int_parsing");
    }

    // An integral value beyond `i64` is a valid pydantic int, so it fails the ceiling rather
    // than the type — and still echoes the raw number.
    #[test]
    fn an_integral_number_beyond_i64_is_a_bound_violation() {
        let d = detail(r#"{"model":"m","max_tokens":1e30,"base_url":null}"#);
        assert_eq!(d[0]["type"], "less_than_equal", "{d}");
        assert_eq!(d[0]["input"], json!(1e30));
    }

    #[test]
    fn a_valid_object_deserializes_unchanged() {
        let m = ValidationErrors::parse_json_body::<M>(
            br#"{"model":"m","max_tokens":20,"base_url":null,"extra":"ignored"}"#, FIELDS).unwrap();
        assert_eq!(m, M { model: "m".into(), max_tokens: 20, base_url: None });
    }

    // `Value` equality is key-order-insensitive, so the order pydantic emits (and the UI's
    // editor diff sees) is pinned with an explicit `keys()` check.
    #[test]
    fn range_entry_has_pydantic_key_order() {
        let max = FieldSpec::ranged("max_tokens", 1, 2048);
        let e = super::range_entry(&max, &json!(5000), "less_than_equal", 2048, "le");
        let keys: Vec<&str> = e.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["type", "loc", "msg", "input", "ctx"]);
        assert_eq!(e, json!({"type": "less_than_equal", "loc": ["body", "max_tokens"],
            "msg": "Input should be less than or equal to 2048", "input": 5000,
            "ctx": {"le": 2048}}));
        let e = super::range_entry(&max, &json!(0), "greater_than_equal", 1, "ge");
        assert_eq!(e["msg"], "Input should be greater than or equal to 1");
        assert_eq!(e["ctx"], json!({"ge": 1}));
    }
}
