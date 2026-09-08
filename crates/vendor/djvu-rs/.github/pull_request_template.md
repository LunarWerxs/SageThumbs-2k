## Summary

<!-- What does this PR do? One paragraph is enough. -->

## Checklist

- [ ] `cargo fmt --check` passes
- [ ] `cargo clippy -- -D warnings` passes
- [ ] `cargo test` passes (unit + integration + doctests)
- [ ] `cargo build --no-default-features` passes (no_std check)
- [ ] New public items have `///` doc comments
- [ ] Tests added for new behavior
- [ ] No `unwrap()` / `expect()` / `panic!` in library code

## Performance impact

<!-- If the change touches a codec or render path, paste before/after `cargo bench` numbers. -->

## Related issue

<!-- Closes #N -->
