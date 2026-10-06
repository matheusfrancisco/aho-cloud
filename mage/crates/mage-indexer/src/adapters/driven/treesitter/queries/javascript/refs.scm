; Mage JavaScript name refs (JSX render refs: ../jsx/refs.scm).

(call_expression function: (identifier) @name) @reference.call

(call_expression function: (member_expression object: (_) @qualifier property: (_) @name)) @reference.call

(new_expression constructor: (identifier) @name) @reference.call

(new_expression constructor: (member_expression object: (_) @qualifier property: (_) @name)) @reference.call

(import_statement source: (string) @name) @reference.import

(import_statement (import_clause (identifier) @qualifier) source: (string) @name) @reference.import

(import_statement (import_clause (namespace_import (identifier) @qualifier)) source: (string) @name) @reference.import

(call_expression
  function: (identifier) @_require
  arguments: (arguments . (string) @name .)
  (#eq? @_require "require")) @reference.import
