//! Small helpers for untyped JavaScript objects (message payloads, AudioData,
//! stats reports) where web-sys has no binding or a binding is clumsier.

use js_sys::Reflect;
use wasm_bindgen::{JsCast, JsValue};

/// `o[k]`, or `undefined`.
pub fn get(o: &JsValue, k: &str) -> JsValue {
    Reflect::get(o, &JsValue::from_str(k)).unwrap_or(JsValue::UNDEFINED)
}

/// `o[k] = v`.
pub fn set(o: &JsValue, k: &str, v: impl Into<JsValue>) {
    let _ = Reflect::set(o, &JsValue::from_str(k), &v.into());
}

/// `o[k]` as a number, or 0.
pub fn num(o: &JsValue, k: &str) -> f64 {
    get(o, k).as_f64().unwrap_or(0.0)
}

/// `o[k]` as a string, or empty.
pub fn string(o: &JsValue, k: &str) -> String {
    get(o, k).as_string().unwrap_or_default()
}

/// Call method `name` on `o` with `args`.
pub fn call(o: &JsValue, name: &str, args: &[&JsValue]) -> Result<JsValue, JsValue> {
    let f: js_sys::Function = get(o, name).dyn_into()?;
    let arr = js_sys::Array::new();
    for a in args {
        arr.push(a);
    }
    f.apply(o, &arr)
}

/// A new empty object.
pub fn object() -> JsValue {
    js_sys::Object::new().into()
}

/// A readable description of a thrown JavaScript value.
pub fn describe(e: &JsValue) -> String {
    if let Some(s) = e.as_string() {
        return s;
    }
    let message = string(e, "message");
    if !message.is_empty() {
        return format!("{}: {message}", string(e, "name"));
    }
    format!("{e:?}")
}

/// JSON text of a value, or empty.
pub fn stringify(v: &JsValue) -> String {
    js_sys::JSON::stringify(v)
        .map(String::from)
        .unwrap_or_default()
}
