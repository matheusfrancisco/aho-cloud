; Mage Go symbols. @definition.<kind> = declaration node, @name = its name.
; `definition.type` is refined by the adapter (struct / interface / type alias).

(package_clause (package_identifier) @name) @definition.module

(function_declaration name: (identifier) @name) @definition.function

(method_declaration name: (field_identifier) @name) @definition.method

(type_spec name: (type_identifier) @name) @definition.type

(type_alias name: (type_identifier) @name) @definition.type_alias

(source_file (const_declaration (const_spec name: (identifier) @name) @definition.constant))

(source_file (var_declaration (var_spec name: (identifier) @name) @definition.variable))

(source_file (var_declaration (var_spec_list (var_spec name: (identifier) @name) @definition.variable)))
