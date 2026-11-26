# Prospect: one small binary on a minimal base image, for Render or any Docker host.
#   docker build -t prospect .
#   docker run -p 8080:8080 -v prospect-data:/data --env-file .env prospect

FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
# Thin LTO and parallel codegen: a quicker build that fits in hosted builders' memory.
ENV CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
RUN cargo build --release --locked -p server && mkdir -p /data-empty

# distroless/cc: glibc, libgcc and CA certificates only; no shell or package manager.
FROM gcr.io/distroless/cc-debian12
COPY --from=build /src/target/release/prospect /prospect
COPY --from=build /data-empty /data
# Render sets PORT itself; DATA_DIR should be a persistent disk (database, photos, secrets key).
ENV PORT=8080 DATA_DIR=/data
EXPOSE 8080
ENTRYPOINT ["/prospect"]
