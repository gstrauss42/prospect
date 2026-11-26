# Prospect: one small binary on a minimal base image.
#   docker build -t prospect .
#   docker run -p 8080:8080 -v prospect-data:/data --env-file .env prospect
# (Not built in the original development environment, where Docker wasn't enabled for WSL.)

FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p server && mkdir -p /data-empty

# distroless/cc: glibc, libgcc and CA certificates only, no shell, runs as non-root (65532).
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/prospect /prospect
# A data directory the non-root user owns; a new named volume inherits this ownership.
COPY --from=build --chown=65532:65532 /data-empty /data
ENV ADDR=0.0.0.0:8080 DATA_DIR=/data
VOLUME ["/data"]
EXPOSE 8080
ENTRYPOINT ["/prospect"]
