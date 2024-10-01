use schemars::JsonSchema;
use serde::Deserialize;

use crate::plugins::telemetry::config_new::connector::http::attributes::ConnectorHttpAttributes;
use crate::plugins::telemetry::config_new::connector::http::instruments::ConnectorHttpInstrumentsConfig;
use crate::plugins::telemetry::config_new::connector::http::selectors::ConnectorHttpSelector;
use crate::plugins::telemetry::config_new::connector::http::selectors::ConnectorHttpValue;
use crate::plugins::telemetry::config_new::extendable::Extendable;
use crate::plugins::telemetry::config_new::instruments::Instrument;

#[derive(Clone, Deserialize, JsonSchema, Debug, Default)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct ConnectorInstrumentsKind {
    pub(crate) http: Extendable<
        ConnectorHttpInstrumentsConfig,
        Instrument<ConnectorHttpAttributes, ConnectorHttpSelector, ConnectorHttpValue>,
    >,
}
