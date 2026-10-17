//! The fields of each span, which the line of each event in it gives.

use tracing::Subscriber;
use tracing::field::Field;
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use super::{Value, Visitor};

/// Keeps the fields of each span in its extensions, for the line of each event in it.
pub(super) struct Keep;

/// The fields of one span, in the order that it got them.
pub(super) struct Fields(pub(super) Vec<(&'static str, Value)>);

impl Fields {
    /// Sets `name` to `value`, in place when the span has the field.
    fn set(&mut self, name: &'static str, value: Value) {
        match self.0.iter_mut().find(|(field, _)| *field == name) {
            Some((_, old)) => *old = value,
            None => self.0.push((name, value)),
        }
    }
}

impl<S> Layer<S> for Keep
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let span = ctx.span(id).expect("the registry has each new span");
        let mut fields = Fields(Vec::new());
        attrs.record(&mut Visitor(|field: &Field, value| {
            fields.set(field.name(), value);
        }));
        span.extensions_mut().insert(fields);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        // Before the lock, as a value can log an event that reads the span.
        let mut recorded = Vec::new();
        values.record(&mut Visitor(|field: &Field, value| {
            recorded.push((field.name(), value));
        }));
        let span = ctx.span(id).expect("the registry has each recorded span");
        let mut extensions = span.extensions_mut();
        let fields = extensions
            .get_mut::<Fields>()
            .expect("on_new_span keeps the fields of each span");
        for (name, value) in recorded {
            fields.set(name, value);
        }
    }
}
