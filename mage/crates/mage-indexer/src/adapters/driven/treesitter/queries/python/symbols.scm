; Mage Python symbols. Methods (functions inside a class body) and UPPER_CASE constants are
; refined by the adapter.

(class_definition name: (identifier) @name) @definition.class

(function_definition name: (identifier) @name) @definition.function

(module (expression_statement (assignment left: (identifier) @name)) @definition.variable)
