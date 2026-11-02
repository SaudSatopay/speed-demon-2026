FROM rust:1-slim-trixie@sha256:1a939748a7cbab172de91a014e3804a97af1394a8c844faa67fce9401c4e40fb
COPY . /src
RUN cd /src && cargo build --release --offline
ENTRYPOINT ["/src/target/release/index"]
