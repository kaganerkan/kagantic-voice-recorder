# Match Rust's +crt-static for the bundled libopus, including CMake's
# explicit MSVC runtime selection (which otherwise overrides /MT flags).
set(OPUS_STATIC_RUNTIME ON CACHE BOOL "Link libopus with the static MSVC runtime" FORCE)
