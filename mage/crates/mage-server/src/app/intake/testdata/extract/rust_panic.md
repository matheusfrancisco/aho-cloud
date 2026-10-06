<!-- description -->
The indexer crashed while processing repo `acme/payments-service`:

```
thread 'tokio-runtime-worker' panicked at crates/indexer/src/graph.rs:212:38:
called `Option::unwrap()` on a `None` value
stack backtrace:
   0: rust_begin_unwind
             at /rustc/07dca489ac2d933c78d3c5158e3f43beefeb02ce/library/std/src/panicking.rs:645:5
   1: core::panicking::panic_fmt
             at /rustc/07dca489ac2d933c78d3c5158e3f43beefeb02ce/library/core/src/panicking.rs:72:14
```

Older runner version printed it differently:

> thread 'main' panicked at 'index out of bounds: the len is 3 but the index is 7', src/parser.rs:88:17
