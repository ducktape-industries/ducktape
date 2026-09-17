//! A view's snapshot layout, taken from the state's own serde impls.
//!
//! [`Snapshot::schema`](crate::Snapshot::schema) is the one thing between
//! bytes an older build wrote and the positional decode a view restores them
//! with: `restore` refuses a snapshot carrying any tag but the view's own. A
//! constant a human types cannot do that job — it holds whatever was last
//! typed, and a state that gains a field while the tag stands still hands
//! moved bytes to a decode that reads them as if they had not moved.
//!
//! So the tag is a digest of the shape, and each view holds its constant to
//! this digest in a test: the drift is a red, not a silent accept.
//!
//! Host-only, like [`manifest`](crate::manifest) — the tracer never enters
//! wasm. The guest carries the answer as a constant; the test is what checks
//! the constant is still the answer.
use std::fmt::Write as _;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_reflection::{
    ContainerFormat, Format, Named, Samples, Tracer, TracerConfig, VariantFormat,
};
use sha2::{Digest, Sha256};

/// What the trace has been told before it starts: see [`Shape::sample`].
pub struct Shape {
    tracer: Tracer,
    samples: Samples,
}

impl Shape {
    /// A value standing in for a type the trace cannot reach on its own: one
    /// whose `Deserialize` validates what it decodes and so refuses the
    /// tracer's empty stand-in, or one with no static shape to describe
    /// (`serde_json::Value` deserializes whatever it is given).
    ///
    /// The sample's shape stands in for the type's, so what the value does
    /// not reach is not described: an empty collection or a `None` inside a
    /// sample stands in the text as `?`, and a layout change under it does not
    /// move the tag. Reach them — give the sample one of each — except where
    /// there is nothing to reach, which is the honest answer for a field the
    /// state never stores and for one whose shape is whatever it was given.
    pub fn sample<T: Serialize>(&mut self, value: &T) -> &mut Self {
        if let Err(error) = self.tracer.trace_value(&mut self.samples, value) {
            panic!(
                "a snapshot-schema sample must serialize: {error}\n\
                 ({} is the type that refused)",
                std::any::type_name::<T>()
            );
        }
        self
    }
}

/// Holds a view's pinned `SNAPSHOT_SCHEMA` to the layout it claims to name.
/// The one call each view makes; what it reports is what to pin instead.
pub fn holds<T: DeserializeOwned>(tag: &str, prepare: impl FnOnce(&mut Shape)) {
    pinned(tag, &digest(&text::<T>(prepare)));
}

/// Holds the tag of a state the trace cannot enter at all: one holding a
/// document that refuses the byte a tracer makes up, where the document is a
/// field of the state itself and so there is no smaller thing to sample.
///
/// The layout is then whatever this value reaches, which is the field list
/// and the shape of everything the value is not empty of. That is where the
/// drift has been; it is not everywhere the drift could be, and a field added
/// under an empty collection does not move this tag.
pub fn holds_value<T: Serialize>(tag: &str, value: &T) {
    let mut shape = shape();
    shape.sample(value);
    pinned(tag, &digest(&render(shape.tracer.registry_unchecked())));
}

fn shape() -> Shape {
    Shape {
        // Samples are what a type that refuses a synthetic value is described
        // by, and a struct is the shape most of them come in.
        tracer: Tracer::new(TracerConfig::default().record_samples_for_structs(true)),
        samples: Samples::new(),
    }
}

fn pinned(tag: &str, layout: &str) {
    assert!(
        tag == layout,
        "the state's layout moved and its snapshot tag did not, so a \
         snapshot written before the move still passes `restore` and is \
         decoded as if it had not moved. Pin SNAPSHOT_SCHEMA to {layout}: \
         every snapshot the build before it wrote is then refused instead, \
         and the view starts fresh where it would have read moved bytes."
    );
}

/// The SHA-256 of a layout text, as the 64 lowercase hex digits a
/// [`Snapshot`](crate::Snapshot) tag is made of.
fn digest(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Every container `T` reaches, by name, as one canonical text. The names
/// are a sorted map, so the text is stable across runs and its hash
/// identifies the layout.
fn text<T: DeserializeOwned>(prepare: impl FnOnce(&mut Shape)) -> String {
    let mut shape = shape();
    prepare(&mut shape);
    let Shape {
        mut tracer,
        samples,
    } = shape;
    if let Err(error) = tracer.trace_type::<T>(&samples) {
        panic!(
            "{} has no traceable snapshot layout: {error}\n\
             a type that validates what it decodes, or one with no static \
             shape, needs a `sample` before the trace reaches it",
            std::any::type_name::<T>()
        );
    }
    // Unchecked: a shape no sample reached stands in the text as `?`, which
    // is the whole truth about it. Refusing the trace instead would demand a
    // value for a field that has no shape to give one of.
    render(tracer.registry_unchecked())
}

fn render(registry: serde_reflection::Registry) -> String {
    let mut out = String::new();
    for (name, format) in &registry {
        let _ = writeln!(out, "{name} = {}", container(format));
    }
    out
}

/// Written out rather than derived from `Debug`: this text is an identity, so
/// it says what the layout is in terms that belong to the layout, and a
/// tracer's own printing (`Variable(RefCell { .. })`) is neither.
fn container(format: &ContainerFormat) -> String {
    match format {
        ContainerFormat::UnitStruct => "unit".into(),
        ContainerFormat::NewTypeStruct(held) => format!("newtype({})", value(held)),
        ContainerFormat::TupleStruct(held) => format!("tuple({})", values(held)),
        ContainerFormat::Struct(fields) => format!("struct {{{}}}", named(fields)),
        ContainerFormat::Enum(variants) => {
            let variants: Vec<_> = variants
                .iter()
                .map(|(index, variant)| {
                    format!(
                        "{index} {}: {}",
                        variant.name,
                        variant_value(&variant.value)
                    )
                })
                .collect();
            format!("enum {{{}}}", variants.join(", "))
        }
    }
}

fn variant_value(format: &VariantFormat) -> String {
    match format {
        // Only a variant the trace never reached, which is a variant this
        // text does not describe.
        VariantFormat::Variable(held) => match &*held.borrow() {
            Some(held) => variant_value(held),
            None => "?".into(),
        },
        VariantFormat::Unit => "unit".into(),
        VariantFormat::NewType(held) => format!("newtype({})", value(held)),
        VariantFormat::Tuple(held) => format!("tuple({})", values(held)),
        VariantFormat::Struct(fields) => format!("struct {{{}}}", named(fields)),
    }
}

fn named(fields: &[Named<Format>]) -> String {
    let fields: Vec<_> = fields
        .iter()
        .map(|field| format!("{}: {}", field.name, value(&field.value)))
        .collect();
    fields.join(", ")
}

fn values(formats: &[Format]) -> String {
    let formats: Vec<_> = formats.iter().map(value).collect();
    formats.join(", ")
}

fn value(format: &Format) -> String {
    match format {
        Format::Variable(held) => match &*held.borrow() {
            Some(held) => value(held),
            None => "?".into(),
        },
        Format::TypeName(name) => name.clone(),
        Format::Unit => "unit".into(),
        Format::Bool => "bool".into(),
        Format::I8 => "i8".into(),
        Format::I16 => "i16".into(),
        Format::I32 => "i32".into(),
        Format::I64 => "i64".into(),
        Format::I128 => "i128".into(),
        Format::U8 => "u8".into(),
        Format::U16 => "u16".into(),
        Format::U32 => "u32".into(),
        Format::U64 => "u64".into(),
        Format::U128 => "u128".into(),
        Format::F32 => "f32".into(),
        Format::F64 => "f64".into(),
        Format::Char => "char".into(),
        Format::Str => "str".into(),
        Format::Bytes => "bytes".into(),
        Format::Option(held) => format!("option<{}>", value(held)),
        Format::Seq(held) => format!("seq<{}>", value(held)),
        Format::Map { key, value: held } => format!("map<{}, {}>", value(key), value(held)),
        Format::Tuple(held) => format!("({})", values(held)),
        Format::TupleArray { content, size } => format!("[{}; {size}]", value(content)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize)]
    struct Held {
        name: String,
        seq: i64,
    }

    #[derive(Serialize, Deserialize)]
    struct Grown {
        name: String,
        seq: i64,
        note: String,
    }

    #[derive(Serialize, Deserialize)]
    struct Retyped {
        name: String,
        seq: u64,
    }

    #[derive(Serialize, Deserialize)]
    struct Rows {
        rows: Vec<Held>,
    }

    #[derive(Serialize, Deserialize)]
    struct RetypedRows {
        rows: Vec<Retyped>,
    }

    fn tag<T: DeserializeOwned>() -> String {
        digest(&text::<T>(|_| {}))
    }

    #[test]
    fn a_tag_is_a_snapshot_tag_and_moves_with_the_layout() {
        let held = tag::<Held>();
        assert_eq!(held.len(), 64, "a tag is a SHA-256 identifier");
        assert!(held.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(held, tag::<Held>(), "the same layout is the same tag");
        assert_ne!(held, tag::<Grown>(), "a field added moves the tag");
        assert_ne!(held, tag::<Retyped>(), "a field retyped moves the tag");
    }

    #[test]
    fn an_empty_collections_elements_are_still_described() {
        // The drift that started this: `Vec<String>` became `Vec<CallPeer>`
        // in a state whose default value holds neither. A trace reads the
        // type, not a value, so the element's shape is in the text either way.
        assert_ne!(tag::<Rows>(), tag::<RetypedRows>());
    }
}
