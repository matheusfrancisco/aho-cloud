; Mage Rust name refs.

(call_expression function: (identifier) @name) @reference.call

(call_expression function: (scoped_identifier path: (_) @qualifier name: (identifier) @name)) @reference.call

(call_expression function: (field_expression value: (_) @qualifier field: (field_identifier) @name)) @reference.call

(call_expression function: (generic_function function: (identifier) @name)) @reference.call

(call_expression
  function: (generic_function function: (scoped_identifier path: (_) @qualifier name: (identifier) @name))) @reference.call

(call_expression
  function: (generic_function function: (field_expression value: (_) @qualifier field: (field_identifier) @name))) @reference.call

(macro_invocation macro: (identifier) @name) @reference.call

(macro_invocation macro: (scoped_identifier path: (_) @qualifier name: (identifier) @name)) @reference.call

(type_identifier) @name @reference.type

(use_declaration argument: (use_as_clause path: (_) @name alias: (identifier) @qualifier)) @reference.import

(use_declaration
  argument: [(identifier) (scoped_identifier) (scoped_use_list) (use_list) (use_wildcard) (crate) (self) (super)] @name) @reference.import
