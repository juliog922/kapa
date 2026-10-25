# =============================================================================
#  Kapa tachyon service: `tachyon serve` (github.com/juliog922/tachyon) from a
#  prebuilt binary, bin/tachyon (provenance and checksum in bin/VERSION; made by
#  `make -C services/tachyon import`, in Debian 12 like this runtime). Tachyon's
#  source is not part of this repository.
# =============================================================================

# ---- tools: checks Kapa's own files here (eval/, scripts/); not shipped --------
FROM python:3.12-slim AS tools
RUN pip install --no-cache-dir lizard==1.24.2 pyflakes==4.0.3

# The models volume starts from this directory, owned by the runtime user (distroless has
# no shell to fix ownership later), so tachyon can write to a new volume from its first start.
FROM debian:bookworm-slim AS dirs
RUN mkdir -p /out/models && chown -R 65532:65532 /out

# glibc only: libcuda.so.1 is dlopen'ed at run time, mounted by the NVIDIA Container Toolkit.
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
COPY bin/tachyon /tachyon
COPY --from=dirs --chown=65532:65532 /out/models /models
# CUDA_CACHE_PATH: kernels are PTX for sm_80; on newer GPUs (prike's RTX 5070) the driver
# compiles them on first use. In the models volume, that compilation survives restarts.
ENV TACHYON_HOME=/models \
    XDG_CACHE_HOME=/models/.cache \
    XDG_STATE_HOME=/models/.state \
    TACHYON_SOCKET=/run/tachyon/tachyon.sock \
    TACHYON_LIBCUDA=/usr/lib/x86_64-linux-gnu/libcuda.so.1 \
    NVIDIA_DRIVER_CAPABILITIES=compute,utility \
    CUDA_CACHE_PATH=/models/.cuda-cache
USER nonroot:nonroot
# `ps` goes through the socket: it proves the daemon is up and admits this user.
HEALTHCHECK --interval=15s --timeout=10s --start-period=60s --retries=3 CMD ["/tachyon", "ps"]
ENTRYPOINT ["/tachyon"]
CMD ["serve"]
