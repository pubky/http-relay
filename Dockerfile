FROM rust:1.84-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

FROM scratch

COPY --from=builder /app/target/release/http-relay /http-relay

EXPOSE 8080

ENTRYPOINT ["/http-relay"]
