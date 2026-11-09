; Rust mod test
((attribute_item
  (attribute
    (identifier) @_attribute
    arguments: ((token_tree
      (identifier) @_test)
      (#eq? @_test "test")))
  (#eq? @_attribute "cfg"))
  .
  (mod_item
    name: (_) @run)
  (#set! tag rust-mod-test))

; Rust test
(((attribute_item
  (attribute
    [
      (identifier) @_attribute
      (scoped_identifier
        (identifier) @_attribute)
    ])
  (#match? @_attribute "test")) @_start
  .
  (attribute_item)*
  .
  [
    (line_comment)
    (block_comment)
  ]*
  .
  (function_item
    name: (_) @run @_test_name
    body: _) @_end)
  (#set! tag rust-test))

; Rust main function
(((function_item
  name: (_) @run
  body: _) @_rust_main_function_end
  (#eq? @run "main"))
  (#set! tag rust-main))
