FROM rust:1-bookworm AS helper
WORKDIR /src/helper
COPY helper/Cargo.toml helper/Cargo.lock ./
COPY helper/src ./src
COPY helper/tf_parser ./tf_parser
RUN cargo build --release

FROM python:3.12-slim
WORKDIR /app
COPY demo_tools.py lame.min.js LAMEJS-LICENSE.txt ./
COPY --from=helper /src/helper/target/release/voice_extract /app/voice_extract
COPY --from=helper /src/helper/target/release/pov_cut /app/pov_cut
RUN chmod +x /app/voice_extract /app/pov_cut && useradd -r -u 10001 demo
USER demo
EXPOSE 8765
CMD ["python", "demo_tools.py", "serve", "--host", "0.0.0.0", "--port", "8765", "--no-browser"]
