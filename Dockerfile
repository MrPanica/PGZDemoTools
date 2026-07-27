FROM rust:1.85-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY web ./web
COPY helper ./helper
COPY lame.min.js LAMEJS-LICENSE.txt ./
RUN cargo build --locked --release --bin PGZDemoTools

FROM debian:bookworm-slim
WORKDIR /app
COPY --from=build /src/target/release/PGZDemoTools /app/PGZDemoTools
COPY --from=build /src/LAMEJS-LICENSE.txt /app/LAMEJS-LICENSE.txt
RUN useradd -r -u 10001 demo
ENV PGZ_DEMO_WORKSPACE=/tmp/pgz-demo-tools
USER demo
EXPOSE 8765
CMD ["/app/PGZDemoTools", "serve", "--host", "0.0.0.0", "--port", "8765", "--workspace", "/tmp/pgz-demo-tools", "--no-browser"]
