# The rendezvous (castcorder-rendezvous, AGPL-3.0-or-later): WebSocket mailbox
# on TCP 8080, STUN on UDP 3479 (coturn keeps 3478). Build context: castcorder/.
# The client build it serves is mounted at run time (--client VERSION=DIR).
#   docker build -f castcorder/rendezvous.Dockerfile castcorder
FROM rust:1.99-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p castcorder-rendezvous

FROM debian:bookworm-slim
COPY --from=build /src/target/release/castcorder-rendezvous /usr/local/bin/castcorder-rendezvous
USER nobody
EXPOSE 8080/tcp 3479/udp
ENTRYPOINT ["castcorder-rendezvous"]
CMD ["--listen", "0.0.0.0:8080", "--stun", "0.0.0.0:3479"]
