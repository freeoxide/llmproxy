# 10. Language bindings

- **Status:** Not started
- **Depends on:** 03, with a stable API
- **Audit rows:** None
- **Code-audit bugs:** None
- **Backlog:** None
- **Conflicts:** None

Give Python and Node users the in-process engine as native packages. These wrap the
engine and add no logic of their own, so they start once its API has stopped changing.

## To do

1. **Define the shared surface.** Cover translation, running a request, streaming events
   and configuration from code.
2. **Python package.** Check that `pyo3` and `maturin` are still maintained, then build
   wheels for Linux, macOS and Windows.
3. **Node package.** Check that `napi` is still maintained, then build prebuilt binaries
   for the same platforms.
4. **Share the tests.** Run the plan 02 contract fixtures through each binding.

## Done when

- `pip install` and `npm install` work on each platform with no Rust toolchain.
- The same fixtures give the same results through Rust, Python and Node.
- Streaming and cancellation work from each language.
