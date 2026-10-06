; Mage Go name refs. @reference.<kind> = ref node, @name = referenced name, @qualifier = selector operand.

(call_expression function: (identifier) @name) @reference.call

(call_expression
  function: (selector_expression operand: (_) @qualifier field: (field_identifier) @name)) @reference.call

(qualified_type package: (package_identifier) @qualifier name: (type_identifier) @name) @reference.type

(type_identifier) @name @reference.type

(import_spec path: (_) @name) @reference.import
