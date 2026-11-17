FROM rust:1.97-bullseye
RUN sed -i /debian-security/d /etc/apt/sources.list \
  && wget -qO- https://apt.llvm.org/llvm-snapshot.gpg.key | gpg --dearmor > /usr/share/keyrings/llvm.gpg \
  && echo "deb [signed-by=/usr/share/keyrings/llvm.gpg] https://apt.llvm.org/bullseye/ llvm-toolchain-bullseye-21 main" > /etc/apt/sources.list.d/llvm.list \
  && apt-get update \
  && apt-get install -y --no-install-recommends bolt-21 llvm-21 lld-21 clang-21 xz-utils unzip \
  && if [ "$(uname -m)" = x86_64 ]; then apt-get install -y --no-install-recommends gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libc6-dev-arm64-cross && rustup target add aarch64-unknown-linux-gnu; fi \
  && rm -rf /var/lib/apt/lists/*
ENV LLVM_BOLT=/usr/lib/llvm-21/bin/llvm-bolt PATH=/usr/local/cargo/bin:/usr/lib/llvm-21/bin:$PATH \
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  CC_aarch64_unknown_linux_gnu=clang CXX_aarch64_unknown_linux_gnu=clang++ AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar
