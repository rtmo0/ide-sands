; Sands, like JetBrains IDEs, puts one run marker on what can be run on its own: `main` and
; test functions. Package-wide tests, `go:generate`, subtests and table cases have none.

; Functions names start with `Test`
(((function_declaration
  name: (_) @run
  (#match? @run "^Test.*")
  (#not-match? @run "^TestMain$"))) @_
  (#set! tag go-test))

; Suite test methods (testify/suite)
((method_declaration
  receiver: (parameter_list
    (parameter_declaration
      type: [
        (pointer_type
          (type_identifier) @_suite_name)
        (type_identifier) @_suite_name
      ]))
  name: (field_identifier) @run @_subtest_name
  (#match? @_subtest_name "^Test.*")
  (#match? @_suite_name ".*Suite")) @_
  (#set! tag go-testify-suite))

; Functions names start with `Example`
(((function_declaration
  name: (_) @run @_name
  (#match? @_name "^Example.*"))) @_
  (#set! tag go-example))

; Functions names start with `Benchmark`
(((function_declaration
  name: (_) @run @_name
  (#match? @_name "^Benchmark.*"))) @_
  (#set! tag go-benchmark))

; Functions names start with `Fuzz`
(((function_declaration
  name: (_) @run @_name
  (#match? @_name "^Fuzz"))) @_
  (#set! tag go-fuzz))

; go run
(((function_declaration
  name: (_) @run
  (#eq? @run "main"))) @_
  (#set! tag go-main))
