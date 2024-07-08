use apollo_compiler::ast::Directive;
use apollo_compiler::coord;
use apollo_compiler::schema::Value;
use apollo_compiler::Node;
use apollo_federation::Supergraph;

#[test]
fn can_extract_subgraph() {
    let schema = r#"
      schema
        @link(url: "https://specs.apollo.dev/link/v1.0")
        @link(url: "https://specs.apollo.dev/join/v0.3", for: EXECUTION)
      {
        query: Query
      }

      directive @join__enumValue(graph: join__Graph!) repeatable on ENUM_VALUE

      directive @join__field(graph: join__Graph, requires: join__FieldSet, provides: join__FieldSet, type: String, external: Boolean, override: String, usedOverridden: Boolean) repeatable on FIELD_DEFINITION | INPUT_FIELD_DEFINITION

      directive @join__graph(name: String!, url: String!) on ENUM_VALUE

      directive @join__implements(graph: join__Graph!, interface: String!) repeatable on OBJECT | INTERFACE

      directive @join__type(graph: join__Graph!, key: join__FieldSet, extension: Boolean! = false, resolvable: Boolean! = true, isInterfaceObject: Boolean! = false) repeatable on OBJECT | INTERFACE | UNION | ENUM | INPUT_OBJECT | SCALAR

      directive @join__unionMember(graph: join__Graph!, member: String!) repeatable on UNION

      directive @link(url: String, as: String, for: link__Purpose, import: [link__Import]) repeatable on SCHEMA

      enum E
        @join__type(graph: SUBGRAPH2)
      {
        V1 @join__enumValue(graph: SUBGRAPH2)
        V2 @join__enumValue(graph: SUBGRAPH2)
      }

      scalar join__FieldSet

      enum join__Graph {
        SUBGRAPH1 @join__graph(name: "Subgraph1", url: "https://Subgraph1")
        SUBGRAPH2 @join__graph(name: "Subgraph2", url: "https://Subgraph2")
      }

      scalar link__Import

      enum link__Purpose {
        """
        \`SECURITY\` features provide metadata necessary to securely resolve fields.
        """
        SECURITY

        """
        \`EXECUTION\` features provide metadata necessary for operation execution.
        """
        EXECUTION
      }

      type Query
        @join__type(graph: SUBGRAPH1)
        @join__type(graph: SUBGRAPH2)
      {
        t: T @join__field(graph: SUBGRAPH1)
      }

      type S
        @join__type(graph: SUBGRAPH1)
      {
        x: Int
      }

      type T
        @join__type(graph: SUBGRAPH1, key: "k")
        @join__type(graph: SUBGRAPH2, key: "k")
      {
        k: ID
        a: Int @join__field(graph: SUBGRAPH2)
        b: String @join__field(graph: SUBGRAPH2)
      }

      union U
        @join__type(graph: SUBGRAPH1)
        @join__unionMember(graph: SUBGRAPH1, member: "S")
        @join__unionMember(graph: SUBGRAPH1, member: "T")
       = S | T
    "#;

    let supergraph = Supergraph::new(schema).unwrap();
    let subgraphs = supergraph
        .extract_subgraphs()
        .expect("Should have been able to extract subgraphs");

    let mut snapshot = String::new();
    for (_name, subgraph) in subgraphs {
        use std::fmt::Write;

        _ = writeln!(
            &mut snapshot,
            "{}: {}\n---\n{}",
            subgraph.name,
            subgraph.url,
            subgraph.schema.schema()
        );
    }
    insta::assert_snapshot!(snapshot);
}

#[test]
fn preserve_default_values_of_input_fields() {
    let supergraph = Supergraph::new(r#"
    schema
      @link(url: "https://specs.apollo.dev/link/v1.0")
      @link(url: "https://specs.apollo.dev/join/v0.2", for: EXECUTION)
    {
      query: Query
    }

    directive @join__field(graph: join__Graph!, requires: join__FieldSet, provides: join__FieldSet, type: String, external: Boolean, override: String, usedOverridden: Boolean) repeatable on FIELD_DEFINITION | INPUT_FIELD_DEFINITION

    directive @join__graph(name: String!, url: String!) on ENUM_VALUE

    directive @join__implements(graph: join__Graph!, interface: String!) repeatable on OBJECT | INTERFACE

    directive @join__type(graph: join__Graph!, key: join__FieldSet, extension: Boolean! = false, resolvable: Boolean! = true) repeatable on OBJECT | INTERFACE | UNION | ENUM | INPUT_OBJECT | SCALAR

    directive @link(url: String, as: String, for: link__Purpose, import: [link__Import]) repeatable on SCHEMA

    input Input
      @join__type(graph: SERVICE)
    {
      a: Int! = 1234
    }

    scalar join__FieldSet

    enum join__Graph {
      SERVICE @join__graph(name: "service", url: "")
    }

    scalar link__Import

    enum link__Purpose {
      """
      \`SECURITY\` features provide metadata necessary to securely resolve fields.
      """
      SECURITY

      """
      \`EXECUTION\` features provide metadata necessary for operation execution.
      """
      EXECUTION
    }

    type Query
      @join__type(graph: SERVICE)
    {
      field(input: Input!): String
    }
    "#).expect("should parse");

    let subgraphs = supergraph
        .extract_subgraphs()
        .expect("should extract subgraphs");

    let service = subgraphs
        .get("service")
        .expect("missing subgraph")
        .schema
        .schema();
    let field_a = coord!(Input.a).lookup_input_field(service).unwrap();
    assert_eq!(
        field_a.default_value,
        Some(Node::new(Value::Int(1234.into())))
    );
}

#[test]
fn erase_empty_types_due_to_overridden_fields() {
    let supergraph = Supergraph::new(r#"
    schema
      @link(url: "https://specs.apollo.dev/link/v1.0")
      @link(url: "https://specs.apollo.dev/join/v0.3", for: EXECUTION)
      @link(url: "https://specs.apollo.dev/tag/v0.3")
    {
      query: Query
    }

    directive @join__enumValue(graph: join__Graph!) repeatable on ENUM_VALUE

    directive @join__field(graph: join__Graph, requires: join__FieldSet, provides: join__FieldSet, type: String, external: Boolean, override: String, usedOverridden: Boolean) repeatable on FIELD_DEFINITION | INPUT_FIELD_DEFINITION

    directive @join__graph(name: String!, url: String!) on ENUM_VALUE

    directive @join__implements(graph: join__Graph!, interface: String!) repeatable on OBJECT | INTERFACE

    directive @join__type(graph: join__Graph!, key: join__FieldSet, extension: Boolean! = false, resolvable: Boolean! = true, isInterfaceObject: Boolean! = false) repeatable on OBJECT | INTERFACE | UNION | ENUM | INPUT_OBJECT | SCALAR

    directive @join__unionMember(graph: join__Graph!, member: String!) repeatable on UNION

    directive @link(url: String, as: String, for: link__Purpose, import: [link__Import]) repeatable on SCHEMA

    directive @tag(name: String!) repeatable on FIELD_DEFINITION | OBJECT | INTERFACE | UNION | ARGUMENT_DEFINITION | SCALAR | ENUM | ENUM_VALUE | INPUT_OBJECT | INPUT_FIELD_DEFINITION | SCHEMA
    input Input
      @join__type(graph: B)
    {
      a: Int! = 1234
    }

    scalar join__FieldSet

    enum join__Graph {
      A @join__graph(name: "a", url: "")
      B @join__graph(name: "b", url: "")
    }

    scalar link__Import

    enum link__Purpose {
      """
      \`SECURITY\` features provide metadata necessary to securely resolve fields.
      """
      SECURITY

      """
      \`EXECUTION\` features provide metadata necessary for operation execution.
      """
      EXECUTION
    }

    type Query
      @join__type(graph: A)
    {
      field: String
    }

    type User
    @join__type(graph: A)
    @join__type(graph: B)
    {
      foo: String @join__field(graph: A, override: "b")

      bar: String @join__field(graph: A)

      baz: String @join__field(graph: A)
    }
    "#).expect("should parse");

    let subgraphs = supergraph
        .extract_subgraphs()
        .expect("should extract subgraphs");

    let b = subgraphs
        .get("b")
        .expect("missing subgraph")
        .schema
        .schema();
    assert!(!b.types.contains_key("User"));
}

#[test]
fn extracts_cost_directives_to_correct_subgraphs() {
    let supergraph = Supergraph::new(r#"
    schema
      @link(url: "https://specs.apollo.dev/link/v1.0")
      @link(url: "https://specs.apollo.dev/join/v0.5", for: EXECUTION)
      @join__directive(graphs: [SUBGRAPH_A, SUBGRAPH_B], name: "link", args: {url: "https://specs.apollo.dev/cost/v0.1", import: ["@cost"]})
    {
      query: Query
    }
    
    directive @join__directive(graphs: [join__Graph!], name: String!, args: join__DirectiveArguments) repeatable on SCHEMA | OBJECT | INTERFACE | FIELD_DEFINITION
    
    directive @join__enumValue(graph: join__Graph!) repeatable on ENUM_VALUE
    
    directive @join__field(graph: join__Graph, requires: join__FieldSet, provides: join__FieldSet, type: String, external: Boolean, override: String, usedOverridden: Boolean, overrideLabel: String, contextArguments: [join__ContextArgument!]) repeatable on FIELD_DEFINITION | INPUT_FIELD_DEFINITION
    
    directive @join__graph(name: String!, url: String!) on ENUM_VALUE
    
    directive @join__implements(graph: join__Graph!, interface: String!) repeatable on OBJECT | INTERFACE
    
    directive @join__type(graph: join__Graph!, key: join__FieldSet, extension: Boolean! = false, resolvable: Boolean! = true, isInterfaceObject: Boolean! = false) repeatable on OBJECT | INTERFACE | UNION | ENUM | INPUT_OBJECT | SCALAR
    
    directive @join__unionMember(graph: join__Graph!, member: String!) repeatable on UNION
    
    directive @link(url: String, as: String, for: link__Purpose, import: [link__Import]) repeatable on SCHEMA
    
    input join__ContextArgument {
      name: String!
      type: String!
      context: String!
      selection: join__FieldValue!
    }
    
    scalar join__DirectiveArguments
    
    scalar join__FieldSet
    
    scalar join__FieldValue
    
    enum join__Graph {
      SUBGRAPH_A @join__graph(name: "subgraph-a", url: "")
      SUBGRAPH_B @join__graph(name: "subgraph-b", url: "")
    }
    
    scalar link__Import
    
    enum link__Purpose {
      """
      `SECURITY` features provide metadata necessary to securely resolve fields.
      """
      SECURITY
    
      """
      `EXECUTION` features provide metadata necessary for operation execution.
      """
      EXECUTION
    }
    
    type Query
      @join__type(graph: SUBGRAPH_A)
      @join__type(graph: SUBGRAPH_B)
    {
      sharedWithCost: Int @join__directive(graphs: [SUBGRAPH_A], name: "cost", args: {weight: 5}) @join__directive(graphs: [SUBGRAPH_B], name: "cost", args: {weight: 10})
    }
    "#).expect("should parse");

    let subgraphs = supergraph
        .extract_subgraphs()
        .expect("should extract subgraphs");

    let a = subgraphs
        .get("subgraph-a")
        .expect("missing subgraph")
        .schema
        .schema();
    let cost = coord!(Query.sharedWithCost)
        .lookup_field(a)
        .expect("has cost field")
        .directives
        .get("federation__cost")
        .expect("has cost directive")
        .argument_by_name("weight")
        .expect("has weight argument");
    assert_eq!(*cost.as_ref(), apollo_compiler::ast::Value::Int(5.into()));

    let b = subgraphs
        .get("subgraph-b")
        .expect("missing subgraph")
        .schema
        .schema();
    let cost = coord!(Query.sharedWithCost)
        .lookup_field(b)
        .expect("has cost field")
        .directives
        .get("federation__cost")
        .expect("has cost directive")
        .argument_by_name("weight")
        .expect("has weight argument");
    assert_eq!(*cost.as_ref(), apollo_compiler::ast::Value::Int(10.into()));
}

#[test]
fn extracts_list_size_directives_to_correct_subgraphs() {
    let supergraph = Supergraph::new(r#"
    schema
      @link(url: "https://specs.apollo.dev/link/v1.0")
      @link(url: "https://specs.apollo.dev/join/v0.5", for: EXECUTION)
      @join__directive(graphs: [SUBGRAPH_A, SUBGRAPH_B], name: "link", args: {url: "https://specs.apollo.dev/cost/v0.1", import: ["@listSize"]})
    {
      query: Query
    }
    
    directive @join__directive(graphs: [join__Graph!], name: String!, args: join__DirectiveArguments) repeatable on SCHEMA | OBJECT | INTERFACE | FIELD_DEFINITION
    
    directive @join__enumValue(graph: join__Graph!) repeatable on ENUM_VALUE
    
    directive @join__field(graph: join__Graph, requires: join__FieldSet, provides: join__FieldSet, type: String, external: Boolean, override: String, usedOverridden: Boolean, overrideLabel: String, contextArguments: [join__ContextArgument!]) repeatable on FIELD_DEFINITION | INPUT_FIELD_DEFINITION
    
    directive @join__graph(name: String!, url: String!) on ENUM_VALUE
    
    directive @join__implements(graph: join__Graph!, interface: String!) repeatable on OBJECT | INTERFACE
    
    directive @join__type(graph: join__Graph!, key: join__FieldSet, extension: Boolean! = false, resolvable: Boolean! = true, isInterfaceObject: Boolean! = false) repeatable on OBJECT | INTERFACE | UNION | ENUM | INPUT_OBJECT | SCALAR
    
    directive @join__unionMember(graph: join__Graph!, member: String!) repeatable on UNION
    
    directive @link(url: String, as: String, for: link__Purpose, import: [link__Import]) repeatable on SCHEMA
    
    input join__ContextArgument {
      name: String!
      type: String!
      context: String!
      selection: join__FieldValue!
    }
    
    scalar join__DirectiveArguments
    
    scalar join__FieldSet
    
    scalar join__FieldValue
    
    enum join__Graph {
      SUBGRAPH_A @join__graph(name: "subgraph-a", url: "")
      SUBGRAPH_B @join__graph(name: "subgraph-b", url: "")
    }
    
    scalar link__Import
    
    enum link__Purpose {
      """
      `SECURITY` features provide metadata necessary to securely resolve fields.
      """
      SECURITY
    
      """
      `EXECUTION` features provide metadata necessary for operation execution.
      """
      EXECUTION
    }
    
    type Query
      @join__type(graph: SUBGRAPH_A)
      @join__type(graph: SUBGRAPH_B)
    {
      sharedWithListSize: Int @join__directive(graphs: [SUBGRAPH_A], name: "listSize", args: {assumedSize: 10, requireOneSlicingArgument: false}) @join__directive(graphs: [SUBGRAPH_B], name: "listSize", args: {assumedSize: 20, requireOneSlicingArgument: false})
    }
    "#).expect("should parse");

    let subgraphs = supergraph
        .extract_subgraphs()
        .expect("should extract subgraphs");

    let a = subgraphs
        .get("subgraph-a")
        .expect("missing subgraph")
        .schema
        .schema();
    let list_size = coord!(Query.sharedWithListSize)
        .lookup_field(a)
        .expect("has cost field")
        .directives
        .get("federation__listSize")
        .expect("has listSize directive");
    assert_assumed_size(list_size, Some(10));
    assert_sized_fields(list_size, None);
    assert_slicing_arguments(list_size, None);
    assert_require_one_slicing_argument(list_size, Some(false));

    let b = subgraphs
        .get("subgraph-b")
        .expect("missing subgraph")
        .schema
        .schema();
    let list_size = coord!(Query.sharedWithListSize)
        .lookup_field(b)
        .expect("has cost field")
        .directives
        .get("federation__listSize")
        .expect("has listSize directive");
    assert_assumed_size(list_size, Some(20));
    assert_sized_fields(list_size, None);
    assert_slicing_arguments(list_size, None);
    assert_require_one_slicing_argument(list_size, Some(false));
}

#[test]
fn extracts_list_size_directives_with_dynamic_sizing_arguments() {
    let supergraph = Supergraph::new(r#"
     schema
      @link(url: "https://specs.apollo.dev/link/v1.0")
      @link(url: "https://specs.apollo.dev/join/v0.5", for: EXECUTION)
      @join__directive(graphs: [SUBGRAPH_A, SUBGRAPH_B], name: "link", args: {url: "https://specs.apollo.dev/cost/v0.1", import: ["@listSize"]})
    {
      query: Query
    }
    
    directive @join__directive(graphs: [join__Graph!], name: String!, args: join__DirectiveArguments) repeatable on SCHEMA | OBJECT | INTERFACE | FIELD_DEFINITION
    
    directive @join__enumValue(graph: join__Graph!) repeatable on ENUM_VALUE
    
    directive @join__field(graph: join__Graph, requires: join__FieldSet, provides: join__FieldSet, type: String, external: Boolean, override: String, usedOverridden: Boolean, overrideLabel: String, contextArguments: [join__ContextArgument!]) repeatable on FIELD_DEFINITION | INPUT_FIELD_DEFINITION
    
    directive @join__graph(name: String!, url: String!) on ENUM_VALUE
    
    directive @join__implements(graph: join__Graph!, interface: String!) repeatable on OBJECT | INTERFACE
    
    directive @join__type(graph: join__Graph!, key: join__FieldSet, extension: Boolean! = false, resolvable: Boolean! = true, isInterfaceObject: Boolean! = false) repeatable on OBJECT | INTERFACE | UNION | ENUM | INPUT_OBJECT | SCALAR
    
    directive @join__unionMember(graph: join__Graph!, member: String!) repeatable on UNION
    
    directive @link(url: String, as: String, for: link__Purpose, import: [link__Import]) repeatable on SCHEMA
    
    type HasInts
      @join__type(graph: SUBGRAPH_A)
      @join__type(graph: SUBGRAPH_B)
    {
      ints: [Int!]
    }
    
    input join__ContextArgument {
      name: String!
      type: String!
      context: String!
      selection: join__FieldValue!
    }
    
    scalar join__DirectiveArguments
    
    scalar join__FieldSet
    
    scalar join__FieldValue
    
    enum join__Graph {
      SUBGRAPH_A @join__graph(name: "subgraph-a", url: "")
      SUBGRAPH_B @join__graph(name: "subgraph-b", url: "")
    }
    
    scalar link__Import
    
    enum link__Purpose {
      """
      `SECURITY` features provide metadata necessary to securely resolve fields.
      """
      SECURITY
    
      """
      `EXECUTION` features provide metadata necessary for operation execution.
      """
      EXECUTION
    }
    
    type Query
      @join__type(graph: SUBGRAPH_A)
      @join__type(graph: SUBGRAPH_B)
    {
      sizedList(first: Int!): HasInts @join__directive(graphs: [SUBGRAPH_A], name: "listSize", args: {slicingArguments: ["first"], sizedFields: ["ints"], requireOneSlicingArgument: true}) @join__directive(graphs: [SUBGRAPH_B], name: "listSize", args: {slicingArguments: ["first"], sizedFields: ["ints"], requireOneSlicingArgument: false})
    }
    "#).expect("should parse");

    let subgraphs = supergraph
        .extract_subgraphs()
        .expect("should extract subgraphs");

    let a = subgraphs
        .get("subgraph-a")
        .expect("missing subgraph")
        .schema
        .schema();
    let list_size = coord!(Query.sizedList)
        .lookup_field(a)
        .expect("has cost field")
        .directives
        .get("federation__listSize")
        .expect("has listSize directive");
    assert_assumed_size(list_size, None);
    assert_sized_fields(list_size, Some(&["ints"]));
    assert_slicing_arguments(list_size, Some(&["first"]));
    assert_require_one_slicing_argument(list_size, Some(true));

    let b = subgraphs
        .get("subgraph-b")
        .expect("missing subgraph")
        .schema
        .schema();
    let list_size = coord!(Query.sizedList)
        .lookup_field(b)
        .expect("has cost field")
        .directives
        .get("federation__listSize")
        .expect("has listSize directive");
    assert_assumed_size(list_size, None);
    assert_sized_fields(list_size, Some(&["ints"]));
    assert_slicing_arguments(list_size, Some(&["first"]));
    assert_require_one_slicing_argument(list_size, Some(false));
}

fn assert_assumed_size(list_size: &Directive, expected_assumed_size: Option<i32>) {
    let assumed_size = list_size.argument_by_name("assumedSize");
    match expected_assumed_size {
        Some(i) => {
            assert_eq!(
                *assumed_size.expect("has assumed size").as_ref(),
                apollo_compiler::ast::Value::Int(i.into())
            );
        }
        None => {
            assert!(assumed_size.is_none());
        }
    }
}

fn assert_sized_fields(list_size: &Directive, expected_sized_fields: Option<&[&str]>) {
    let sized_fields = list_size
        .argument_by_name("sizedFields")
        .and_then(|arg| arg.as_list());
    assert_node_list(sized_fields, expected_sized_fields);
}

fn assert_slicing_arguments(list_size: &Directive, expected_slicing_arguments: Option<&[&str]>) {
    let slicing_arguments = list_size
        .argument_by_name("slicingArguments")
        .and_then(|arg| arg.as_list());
    assert_node_list(slicing_arguments, expected_slicing_arguments);
}

fn assert_node_list(node_list: Option<&[Node<Value>]>, expected_slice: Option<&[&str]>) {
    match expected_slice {
        Some(slice) => {
            assert!(node_list.is_some());
            let node_list = node_list.unwrap();
            assert_eq!(node_list.len(), slice.len());
            for i in 0..node_list.len() {
                assert_eq!(
                    *node_list[i].as_ref(),
                    apollo_compiler::ast::Value::String(slice[i].to_string())
                );
            }
        }
        None => {
            assert!(node_list.is_none());
        }
    }
}

fn assert_require_one_slicing_argument(
    list_size: &Directive,
    expected_require_one_slicing_argument: Option<bool>,
) {
    let require_one_slicing_argument = list_size.argument_by_name("requireOneSlicingArgument");

    match expected_require_one_slicing_argument {
        Some(b) => {
            assert_eq!(
                *require_one_slicing_argument
                    .expect("has requireOneSlicingArgument")
                    .as_ref(),
                apollo_compiler::ast::Value::Boolean(b)
            )
        }
        None => {
            assert!(require_one_slicing_argument.is_none());
        }
    }
}
