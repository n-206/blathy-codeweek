# Codeweek

A Rust project created for EU Code Week 2026. The project activity was open
from **10 November 2026 through 17 November 2026**.

Copyright (C) 2026 n206 <notnandor@posteo.nl>

I chose Rust because it is the language I know best, and used widely adopted
crates from the Rust ecosystem. The project is an asynchronous client for
Codeweek challenge APIs. It contains ten task modules, five side missions,
two arena challenges, and a **Zero to Hero** run that submits the task
solutions in separate OS threads so their synchronous work can run in parallel
when CPU cores are available. Originally it was concurrent and I got a 17.5 seconds, switched to separate threads and I got 12.5 seconds.
It also includes a local parking-garage challenge
server. Do note that I might not have thought about everything, so things might break
while running.
Currently the slowest tasks are 4 and 8. 4 has to do lots of api calls and 8 has to go through lots of possible variants. There is a hard limit, 12 seconds and 500_000 nodes for task 8. There are times when it doesn't find the solution in time. I'm working on optimizing these tasks.

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

`cargo run` opens an interactive menu where you can choose Zero to Hero, an
individual task, a side mission, or an arena challenge.

### Parking-garage server

The parking-garage server starts on demand when Zero to Hero or Task 10 is
selected. Set `API_BASE` and `PARKING_URL` (an externally reachable HTTPS URL
that routes to the local listener). The default listener port is `3000`; set
`PORT` to use another port.

The client starts the listener automatically when needed. For manual testing,
the server can also be started independently with
`cargo run --bin parking_server`. Zero to Hero registers the endpoint for each
run using that run's key.

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
