; Mage Python name refs.

(call function: (identifier) @name) @reference.call

(call function: (attribute object: (_) @qualifier attribute: (identifier) @name)) @reference.call

(import_statement name: (dotted_name) @name) @reference.import

(import_statement name: (aliased_import name: (dotted_name) @name alias: (identifier) @qualifier)) @reference.import

(import_from_statement module_name: (_) @name) @reference.import
