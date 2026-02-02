# HTTP Relay

A Rust implementation of _some_ of [Http relay spec](https://httprelay.io/).


## Example

```rust
#[tokio::main]
async fn main() {
    let http_relay = http_relay::HttpRelay::builder()
        .http_port(15412)
        .run()
        .await
        .unwrap();

    println!(
        "Running http relay {}",
        http_relay.local_link_url().as_str()
    );

    tokio::signal::ctrl_c().await.unwrap();

    http_relay.shutdown().await.unwrap();
}
```
