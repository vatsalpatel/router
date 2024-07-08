use apollo_compiler::ast::Argument;
use apollo_compiler::ast::Directive;
use apollo_compiler::name;
use apollo_compiler::Name;
use apollo_compiler::Node;

use crate::error::FederationError;
use crate::link::spec::Identity;
use crate::link::spec::Url;
use crate::link::spec::Version;
use crate::link::spec_definition::SpecDefinition;
use crate::schema::FederationSchema;

pub(crate) const COST_DIRECTIVE_NAME_IN_SPEC: Name = name!("cost");
pub(crate) const COST_DIRECTIVE_NAME_DEFAULT: Name = name!("federation__cost");
pub(crate) const COST_WEIGHT_ARGUMENT_NAME: Name = name!("weight");

pub(crate) const LIST_SIZE_DIRECTIVE_NAME_IN_SPEC: Name = name!("listSize");
pub(crate) const LIST_SIZE_DIRECTIVE_NAME_DEFAULT: Name = name!("federation__listSize");
pub(crate) const LIST_SIZE_ASSUMED_SIZE_ARGUMENT_NAME: Name = name!("assumedSize");
pub(crate) const LIST_SIZE_SLICING_ARGUMENTS_ARGUMENT_NAME: Name = name!("slicingArguments");
pub(crate) const LIST_SIZE_SIZED_FIELDS_ARGUMENT_NAME: Name = name!("sizedFields");
pub(crate) const LIST_SIZE_REQUIRE_ONE_SLICING_ARGUMENT_ARGUMENT_NAME: Name =
    name!("requireOneSlicingArgument");

#[derive(Clone)]
pub(crate) struct CostSpecDefinition {
    url: Url,
    minimum_federation_version: Option<Version>,
}

impl CostSpecDefinition {
    pub(crate) fn new(version: Version, minimum_federation_version: Option<Version>) -> Self {
        Self {
            url: Url {
                identity: Identity::cost_identity(),
                version,
            },
            minimum_federation_version,
        }
    }

    pub(crate) fn cost_directive(
        &self,
        schema: &FederationSchema,
        arguments: Vec<Node<Argument>>,
    ) -> Result<Directive, FederationError> {
        let name_in_schema = self
            .directive_name_in_schema(schema, &COST_DIRECTIVE_NAME_IN_SPEC)?
            .unwrap_or(COST_DIRECTIVE_NAME_DEFAULT);

        Ok(Directive {
            name: name_in_schema,
            arguments,
        })
    }

    pub(crate) fn list_size_directive(
        &self,
        schema: &FederationSchema,
        arguments: Vec<Node<Argument>>,
    ) -> Result<Directive, FederationError> {
        let name_in_schema = self
            .directive_name_in_schema(schema, &LIST_SIZE_DIRECTIVE_NAME_IN_SPEC)?
            .unwrap_or(LIST_SIZE_DIRECTIVE_NAME_DEFAULT);

        Ok(Directive {
            name: name_in_schema,
            arguments,
        })
    }
}

impl SpecDefinition for CostSpecDefinition {
    fn url(&self) -> &Url {
        &self.url
    }

    fn minimum_federation_version(&self) -> Option<&Version> {
        self.minimum_federation_version.as_ref()
    }
}
