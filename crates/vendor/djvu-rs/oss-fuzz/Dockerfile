# Dockerfile for OSS-Fuzz integration of djvu-rs.
# Patterned after upstream Rust projects (image-rs, zip2).
#
# Build locally with:
#   python infra/helper.py build_image djvu-rs
#   python infra/helper.py build_fuzzers --sanitizer address djvu-rs
#   python infra/helper.py run_fuzzer djvu-rs fuzz_full
FROM gcr.io/oss-fuzz-base/base-builder-rust

RUN git clone --depth 1 https://github.com/matyushkin/djvu-rs.git $SRC/djvu-rs
WORKDIR $SRC/djvu-rs

COPY build.sh $SRC/
