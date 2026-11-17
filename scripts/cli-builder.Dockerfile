FROM rust:1.97-bullseye
RUN sed -i /debian-security/d /etc/apt/sources.list \
  && wget -qO- https://apt.llvm.org/llvm-snapshot.gpg.key | gpg --dearmor > /usr/share/keyrings/llvm.gpg \
  && echo "deb [signed-by=/usr/share/keyrings/llvm.gpg] https://apt.llvm.org/bullseye/ llvm-toolchain-bullseye-21 main" > /etc/apt/sources.list.d/llvm.list \
  && apt-get update \
  && apt-get install -y --no-install-recommends bolt-21 llvm-21 lld-21 \
  && rm -rf /var/lib/apt/lists/*
ENV LLVM_BOLT=/usr/lib/llvm-21/bin/llvm-bolt PATH=/usr/local/cargo/bin:/usr/lib/llvm-21/bin:$PATH
