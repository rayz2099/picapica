FROM rust:1.91-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p picapica

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/picapica /usr/local/bin/picapica
COPY config.example.yaml /etc/picapica/config.example.yaml
WORKDIR /var/lib/picapica
EXPOSE 8080
ENTRYPOINT ["picapica"]
CMD ["serve", "--config", "/etc/picapica/config.yaml"]
