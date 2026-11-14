# Codeweek

A Rust project created for EU Code Week 2026. The project activity was open
from **10 November 2026 through 17 November 2026**.

Copyright (C) 2026 n206 <notnandor@posteo.nl>

I chose Rust because it is the language I know best, and used widely adopted
crates from the Rust ecosystem. The project is an asynchronous client for
Codeweek challenge APIs. It contains ten task modules, three side missions,
two arena challenges, and a default **Zero to Hero** run that submits the task
solutions concurrently. It also includes a local parking-garage challenge
server.

## Requirements

- Rust 1.85 or newer (Rust 2024 edition)
- Access to the BMSZC Bláthy Ottó Titusz Informatikai Technikum Codeweek challenge API

## Configure and run

The client requires `API_BASE`, the base URL of the challenge API. You can set
it in your shell:

```sh
export API_BASE="https://your-challenge-api.example"
cargo run
```

Alternatively, create a `.env` file in the project directory containing:

```dotenv
API_BASE=https://your-challenge-api.example
```

`cargo run` starts the Zero to Hero run by default. Other task, side-mission,
and arena entry points are available in `src/main.rs`; select the one you want
there before running.

### Parking-garage server

The optional parking server fetches its challenge data from the API and
registers its public endpoint. Set `API_BASE` and `PARKING_URL` (an externally
reachable HTTPS URL for the server), then run:

```sh
cargo run --bin parking_server
```

The server listens on port `3000` by default. Set `PORT` to use a different
port. The externally reachable `PARKING_URL` must route to that listener.

## Main dependencies

The project uses crates from several commonly used Rust ecosystem areas:

- **Async and HTTP:** Tokio, Axum, Reqwest, Futures
- **Serialization:** Serde, Serde JSON, Serde XML, Serde YAML, CSV
- **Cryptography:** AES-GCM, HKDF, P-256, SHA-1, SHA-2, HMAC
- **Utilities:** Anyhow, Chrono, Base64, Hex, Flate2, dotenvy

See [`Cargo.toml`](Cargo.toml) for the full dependency list.

## License

Licensed under the GNU General Public License, version 3 only
(GPL-3.0-only). See [`LICENSE`](LICENSE) for the full license text.
