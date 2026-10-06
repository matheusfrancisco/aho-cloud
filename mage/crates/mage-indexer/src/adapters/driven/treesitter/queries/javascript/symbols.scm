; Mage JavaScript / JSX symbols.

(function_declaration name: (identifier) @name) @definition.function

(generator_function_declaration name: (identifier) @name) @definition.function

(class_declaration name: (identifier) @name) @definition.class

(method_definition name: (_) @name) @definition.method

(lexical_declaration
  (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)])) @definition.function

(variable_declaration
  (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)])) @definition.function

(field_definition property: (property_identifier) @name value: [(arrow_function) (function_expression)]) @definition.method
