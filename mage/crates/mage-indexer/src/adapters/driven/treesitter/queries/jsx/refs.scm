; Mage JSX render refs (appended to the JavaScript and TSX ref queries). Lowercase intrinsic
; elements (`div`) are not refs.

(jsx_opening_element name: (identifier) @name (#match? @name "^[A-Z]")) @reference.render

(jsx_opening_element name: (member_expression object: (_) @qualifier property: (_) @name)) @reference.render

(jsx_self_closing_element name: (identifier) @name (#match? @name "^[A-Z]")) @reference.render

(jsx_self_closing_element name: (member_expression object: (_) @qualifier property: (_) @name)) @reference.render
