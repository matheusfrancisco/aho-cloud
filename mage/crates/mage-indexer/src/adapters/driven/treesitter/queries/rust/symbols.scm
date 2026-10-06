; Mage Rust symbols. Methods (functions in impl / trait bodies) and #[test] functions are
; refined by the adapter.

(function_item name: (identifier) @name) @definition.function

(function_signature_item name: (identifier) @name) @definition.function

(struct_item name: (type_identifier) @name) @definition.struct

(union_item name: (type_identifier) @name) @definition.struct

(enum_item name: (type_identifier) @name) @definition.enum

(trait_item name: (type_identifier) @name) @definition.trait

(type_item name: (type_identifier) @name) @definition.type_alias

(const_item name: (identifier) @name) @definition.constant

(static_item name: (identifier) @name) @definition.variable

(macro_definition name: (identifier) @name) @definition.macro

(mod_item name: (identifier) @name) @definition.module
