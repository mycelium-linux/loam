# Soil pipelines as data

A pipeline is a record, not a new syntax. Its first consumer is the DSP/fusion
graph runner: the linter, node editor, constrained decoder, and deployment plan
all operate on the same value.

```lisp
(pipeline/event
  (pipeline/stage
    (pipeline/source (pipeline/make "maritime") "iq"
      (list (list :rate 2000000)
            (list :center 161975000)))
    "windowed-fft"
    (list (list :size 1024)
          (list :window "hann")))
  "spectrum" "soil.dsp/spectrum@1")
```

Fields:

- `:id` — stable pipeline name
- `:version` — format version, currently `1`
- `:source` — `(:kind "...") (:config ...)`
- `:stages` — ordered list of `(:op "...") (:options ...)`
- `:event` — `(:kind "...") (:schema "...")`
- `:placement` — optional resolver constraints

`pipeline/validate` checks the shape before anything executes. The initial stage
vocabulary is `windowed-fft`, `fuse`, `detect`, `decode`, `judge`, and `event`.
A `windowed-fft` stage requires a power-of-two size in `32..=65536` and one of
the declared windows.

This deliberately does not add `|>` or `pipeline ... end` syntax. The
S-expression/record form is already the canonical IR; a surface can be lowered
to it later without changing the contract.
