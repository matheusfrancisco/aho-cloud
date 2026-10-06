; Mage TypeScript / TSX symbols.

(function_declaration name: (identifier) @name) @definition.function

(generator_function_declaration name: (identifier) @name) @definition.function

(function_signature name: (identifier) @name) @definition.function

(class_declaration name: (type_identifier) @name) @definition.class

(abstract_class_declaration name: (type_identifier) @name) @definition.class

(method_definition name: (_) @name) @definition.method

(abstract_method_signature name: (_) @name) @definition.method

(interface_declaration name: (type_identifier) @name) @definition.interface

(type_alias_declaration name: (type_identifier) @name) @definition.type_alias

(enum_declaration name: (identifier) @name) @definition.enum

(lexical_declaration
  (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)])) @definition.function

(variable_declaration
  (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)])) @definition.function

(public_field_definition name: (property_identifier) @name value: [(arrow_function) (function_expression)]) @definition.method
