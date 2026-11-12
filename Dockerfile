FROM rust:1-slim-trixie AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY static static
RUN cargo build --release

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* && mkdir /data
COPY --from=build /app/target/release/courier /usr/local/bin/courier
ENV COURIER_HOST=0.0.0.0 COURIER_PORT=7878 COURIER_DATA=/data/courier-data.json
VOLUME /data
EXPOSE 7878
CMD ["courier"]
